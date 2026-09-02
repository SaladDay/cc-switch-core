use std::{error::Error, fmt};

use thiserror::Error;

use crate::{
    apply_skill_reference, builtin_app_registry, execute_operation_plan, AppType,
    ContentExpectation, OperationExecutionError, OperationHost, OperationPlan, OperationPlanError,
    OperationReceipt, OperationRollbackError, PlannedWrite, SkillCatalogColumn,
    SkillReferenceError, SkillReferenceReceipt, OPERATION_CONTRACT_MAJOR,
};

use super::{
    catalog::valid_skill_id,
    config::{project_native_control, SkillConfigWriteError},
    inspect_installed_skills,
    read::validate_catalog_identity,
    SkillCatalogEntry, SkillControlReason, SkillReadError, SkillReferencePlan, SkillRuntime,
};

/// The order in which a host applies the two live parts of a Skill plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillWriteOrder {
    /// Make the Skill visible before removing a native disabled-list entry.
    ReferenceThenConfiguration,
    /// Disable native discovery before removing the native directory entry.
    ConfigurationThenReference,
}

/// Catalog identity that must still match before a live change is committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCatalogGuard {
    skill_id: String,
    expected_name: String,
    expected_directory: String,
    expected_selection: Option<(SkillCatalogColumn, bool)>,
}

impl SkillCatalogGuard {
    pub fn skill_id(&self) -> &str {
        &self.skill_id
    }

    pub fn expected_name(&self) -> &str {
        &self.expected_name
    }

    pub fn expected_directory(&self) -> &str {
        &self.expected_directory
    }

    /// Returns the catalog selection that must still be current.
    pub fn expected_selection(&self) -> Option<(SkillCatalogColumn, bool)> {
        self.expected_selection
    }

    /// Checks the complete row precondition before the catalog transaction is
    /// committed.
    pub fn matches(&self, entry: &SkillCatalogEntry) -> bool {
        self.identity_matches(entry)
            && self
                .expected_selection
                .is_none_or(|(column, expected)| selected_column(entry, column) == Some(expected))
    }

    fn identity_matches(&self, entry: &SkillCatalogEntry) -> bool {
        entry.id() == self.skill_id
            && entry.name() == self.expected_name
            && entry.directory() == self.expected_directory
    }
}

/// One compare-and-swap requested against the shared `skills` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCatalogChange {
    skill_id: String,
    column: SkillCatalogColumn,
    expected: bool,
    replacement: bool,
}

impl SkillCatalogChange {
    pub fn skill_id(&self) -> &str {
        &self.skill_id
    }

    pub fn column(&self) -> SkillCatalogColumn {
        self.column
    }

    pub fn expected(&self) -> bool {
        self.expected
    }

    pub fn replacement(&self) -> bool {
        self.replacement
    }
}

/// A complete, host-neutral plan for one Skill/app state transition.
#[derive(Debug)]
pub struct SkillSwitchPlan {
    app: AppType,
    target_enabled: bool,
    write_order: SkillWriteOrder,
    catalog_guard: SkillCatalogGuard,
    reference: Option<SkillReferencePlan>,
    configuration: Option<OperationPlan>,
    catalog_change: Option<SkillCatalogChange>,
}

impl SkillSwitchPlan {
    pub fn app(&self) -> &AppType {
        &self.app
    }

    pub fn target_enabled(&self) -> bool {
        self.target_enabled
    }

    pub fn write_order(&self) -> SkillWriteOrder {
        self.write_order
    }

    pub fn catalog_guard(&self) -> &SkillCatalogGuard {
        &self.catalog_guard
    }

    pub fn reference(&self) -> Option<&SkillReferencePlan> {
        self.reference.as_ref()
    }

    pub fn configuration(&self) -> Option<&OperationPlan> {
        self.configuration.as_ref()
    }

    pub fn catalog_change(&self) -> Option<&SkillCatalogChange> {
        self.catalog_change.as_ref()
    }

    pub fn is_live_noop(&self) -> bool {
        self.reference.is_none() && self.configuration.is_none()
    }

    /// Resolves an uncertain catalog commit from a freshly-read row.
    ///
    /// `KeepLive` commits the receipt, `RestoreLive` rolls it back, and
    /// `Conflict` must preserve the newly-read row while attempting only the
    /// receipt's guarded rollback before a fresh reconciliation.
    pub fn decide_catalog(&self, entry: Option<&SkillCatalogEntry>) -> SkillCatalogDecision {
        let Some(entry) = entry.filter(|entry| self.catalog_guard.identity_matches(entry)) else {
            return SkillCatalogDecision::Conflict;
        };
        match self.catalog_change.as_ref() {
            Some(change) if selected_column(entry, change.column) == Some(change.replacement) => {
                SkillCatalogDecision::KeepLive
            }
            Some(change) if selected_column(entry, change.column) == Some(change.expected) => {
                SkillCatalogDecision::RestoreLive
            }
            Some(_) => SkillCatalogDecision::Conflict,
            None if self.catalog_guard.matches(entry) => SkillCatalogDecision::KeepLive,
            None => SkillCatalogDecision::Conflict,
        }
    }
}

fn selected_column(entry: &SkillCatalogEntry, column: SkillCatalogColumn) -> Option<bool> {
    entry
        .selections()
        .find_map(|(candidate, selected)| (candidate == column).then_some(selected))
}

/// What to do with an already-applied live receipt after re-reading catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillCatalogDecision {
    KeepLive,
    RestoreLive,
    Conflict,
}

/// A successfully applied live plan awaiting the host's catalog decision.
pub struct SkillLiveReceipt<R> {
    write_order: SkillWriteOrder,
    reference: Option<SkillReferenceReceipt>,
    configuration: Option<OperationReceipt<R>>,
}

impl<R> SkillLiveReceipt<R> {
    pub fn is_empty(&self) -> bool {
        self.reference.is_none() && self.configuration.is_none()
    }

    /// Rechecks the live reference after the database commit succeeds.
    pub fn commit(self) -> Result<(), SkillReferenceError> {
        match self.reference {
            Some(receipt) => receipt.commit(),
            None => Ok(()),
        }
    }

    /// Restores the live state after a catalog compare-and-swap is known to
    /// have failed. An uncertain database commit must be re-read first.
    pub fn rollback<H>(self, host: &mut H) -> Result<(), SkillLiveRollbackError<H::Error>>
    where
        H: OperationHost<Resource = R>,
    {
        let mut failures = Vec::new();
        rollback_parts(
            self.write_order,
            self.reference,
            self.configuration,
            host,
            &mut failures,
        );
        if failures.is_empty() {
            Ok(())
        } else {
            Err(SkillLiveRollbackError { failures })
        }
    }
}

impl<R> fmt::Debug for SkillLiveReceipt<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SkillLiveReceipt")
            .field("write_order", &self.write_order)
            .field("has_reference", &self.reference.is_some())
            .field("has_configuration", &self.configuration.is_some())
            .finish()
    }
}

/// Applies both live parts in visibility-safe order and rolls the first part
/// back if the second fails. This function does not change the shared catalog.
/// Any error can still follow a visible but non-durable filesystem change, so
/// the host must leave the catalog unchanged (rolling back if needed) and
/// reconcile the committed selection before releasing the shared live-config
/// lock.
pub fn execute_skill_live_plan<H>(
    plan: &SkillSwitchPlan,
    host: &mut H,
) -> Result<SkillLiveReceipt<H::Resource>, SkillLiveExecutionError<H::Error>>
where
    H: OperationHost,
{
    match plan.write_order {
        SkillWriteOrder::ReferenceThenConfiguration => {
            let reference = plan
                .reference
                .as_ref()
                .map(apply_skill_reference)
                .transpose()
                .map_err(|failure| SkillLiveExecutionError {
                    failure: SkillLiveFailure::Reference(failure),
                    rollback_failures: Vec::new(),
                })?;
            let configuration = match plan
                .configuration
                .as_ref()
                .map(|configuration| execute_operation_plan(configuration, host))
                .transpose()
            {
                Ok(configuration) => configuration,
                Err(failure) => {
                    let mut rollback_failures = Vec::new();
                    if let Some(reference) = reference {
                        if let Err(error) = reference.rollback() {
                            rollback_failures.push(SkillLiveRollbackFailure::Reference(error));
                        }
                    }
                    return Err(SkillLiveExecutionError {
                        failure: SkillLiveFailure::Configuration(failure),
                        rollback_failures,
                    });
                }
            };
            Ok(SkillLiveReceipt {
                write_order: plan.write_order,
                reference,
                configuration,
            })
        }
        SkillWriteOrder::ConfigurationThenReference => {
            let configuration = plan
                .configuration
                .as_ref()
                .map(|configuration| execute_operation_plan(configuration, host))
                .transpose()
                .map_err(|failure| SkillLiveExecutionError {
                    failure: SkillLiveFailure::Configuration(failure),
                    rollback_failures: Vec::new(),
                })?;
            let reference = match plan
                .reference
                .as_ref()
                .map(apply_skill_reference)
                .transpose()
            {
                Ok(reference) => reference,
                Err(failure) => {
                    let mut rollback_failures = Vec::new();
                    if let Some(configuration) = configuration {
                        if let Err(error) = configuration.rollback(host) {
                            rollback_failures.push(SkillLiveRollbackFailure::Configuration(error));
                        }
                    }
                    return Err(SkillLiveExecutionError {
                        failure: SkillLiveFailure::Reference(failure),
                        rollback_failures,
                    });
                }
            };
            Ok(SkillLiveReceipt {
                write_order: plan.write_order,
                reference,
                configuration,
            })
        }
    }
}

fn rollback_parts<H, R>(
    write_order: SkillWriteOrder,
    reference: Option<SkillReferenceReceipt>,
    configuration: Option<OperationReceipt<R>>,
    host: &mut H,
    failures: &mut Vec<SkillLiveRollbackFailure<H::Error>>,
) where
    H: OperationHost<Resource = R>,
{
    match write_order {
        SkillWriteOrder::ReferenceThenConfiguration => {
            rollback_configuration(configuration, host, failures);
            rollback_reference(reference, failures);
        }
        SkillWriteOrder::ConfigurationThenReference => {
            rollback_reference(reference, failures);
            rollback_configuration(configuration, host, failures);
        }
    }
}

fn rollback_reference<E>(
    reference: Option<SkillReferenceReceipt>,
    failures: &mut Vec<SkillLiveRollbackFailure<E>>,
) {
    if let Some(reference) = reference {
        if let Err(error) = reference.rollback() {
            failures.push(SkillLiveRollbackFailure::Reference(error));
        }
    }
}

fn rollback_configuration<H, R>(
    configuration: Option<OperationReceipt<R>>,
    host: &mut H,
    failures: &mut Vec<SkillLiveRollbackFailure<H::Error>>,
) where
    H: OperationHost<Resource = R>,
{
    if let Some(configuration) = configuration {
        if let Err(error) = configuration.rollback(host) {
            failures.push(SkillLiveRollbackFailure::Configuration(error));
        }
    }
}

/// The live step that prevented a prepared plan from completing.
#[derive(Debug)]
pub enum SkillLiveFailure<E> {
    Reference(SkillReferenceError),
    Configuration(OperationExecutionError<E>),
}

/// A live-plan failure plus any incomplete compensation.
#[derive(Debug)]
pub struct SkillLiveExecutionError<E> {
    failure: SkillLiveFailure<E>,
    rollback_failures: Vec<SkillLiveRollbackFailure<E>>,
}

impl<E> SkillLiveExecutionError<E> {
    pub fn failure(&self) -> &SkillLiveFailure<E> {
        &self.failure
    }

    pub fn rollback_failures(&self) -> &[SkillLiveRollbackFailure<E>] {
        &self.rollback_failures
    }
}

impl<E: fmt::Display> fmt::Display for SkillLiveExecutionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.failure {
            SkillLiveFailure::Reference(error) => write!(formatter, "{error}"),
            SkillLiveFailure::Configuration(error) => write!(formatter, "{error}"),
        }?;
        if !self.rollback_failures.is_empty() {
            write!(
                formatter,
                "; rollback was incomplete for {} live part(s)",
                self.rollback_failures.len()
            )?;
        }
        Ok(())
    }
}

impl<E: Error + 'static> Error for SkillLiveExecutionError<E> {}

/// One live part that could not be restored.
#[derive(Debug)]
pub enum SkillLiveRollbackFailure<E> {
    Reference(SkillReferenceError),
    Configuration(OperationRollbackError<E>),
}

/// Failures encountered while explicitly rolling back a complete live plan.
#[derive(Debug)]
pub struct SkillLiveRollbackError<E> {
    failures: Vec<SkillLiveRollbackFailure<E>>,
}

impl<E> SkillLiveRollbackError<E> {
    pub fn failures(&self) -> &[SkillLiveRollbackFailure<E>] {
        &self.failures
    }
}

impl<E> fmt::Display for SkillLiveRollbackError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "rollback was incomplete for {} live part(s)",
            self.failures.len()
        )
    }
}

impl<E: Error + 'static> Error for SkillLiveRollbackError<E> {}

/// Prepares a user-requested switch from a fresh catalog and live snapshot.
///
/// `catalog` must be the complete freshly-read shared catalog so Core can
/// reject duplicate path and native-control identities before writing.
///
/// The host owns the transaction boundary. It should begin its shared-catalog
/// write transaction, take `shared_live_config_lock_path`, create any missing
/// native/state roots as real directories, then re-read all inputs, prepare
/// and apply this plan. While still holding both, it verifies
/// `catalog_guard`, compare-and-swaps `catalog_change` when present, and
/// commits. If the commit outcome is uncertain, it re-reads the row and uses
/// [`SkillSwitchPlan::decide_catalog`] before releasing the live lock. Startup
/// reconciliation follows the committed catalog; Core keeps no second
/// selection store. Pending recovery or catalog/live drift must be reconciled
/// before another user-requested target is accepted.
pub fn prepare_skill_switch(
    catalog: &[SkillCatalogEntry],
    skill_id: &str,
    runtime: &SkillRuntime,
    app: &AppType,
    target_enabled: bool,
) -> Result<SkillSwitchPlan, SkillPrepareError> {
    if !valid_skill_id(skill_id) {
        return Err(SkillPrepareError::InvalidSkillId);
    }
    prepare(catalog, skill_id, runtime, app, Some(target_enabled))
}

/// Builds the same live plan using only the already-committed selection.
///
/// `catalog` has the same completeness requirement as
/// [`prepare_skill_switch`].
///
/// Every application converges to its committed shared-catalog value. Valid
/// interrupted reference states are resumed by this path; conflicting or
/// unowned native entries remain unavailable.
pub fn prepare_skill_reconciliation(
    catalog: &[SkillCatalogEntry],
    skill_id: &str,
    runtime: &SkillRuntime,
    app: &AppType,
) -> Result<SkillSwitchPlan, SkillPrepareError> {
    if !valid_skill_id(skill_id) {
        return Err(SkillPrepareError::InvalidSkillId);
    }
    prepare(catalog, skill_id, runtime, app, None)
}

fn prepare(
    catalog: &[SkillCatalogEntry],
    skill_id: &str,
    runtime: &SkillRuntime,
    app: &AppType,
    requested: Option<bool>,
) -> Result<SkillSwitchPlan, SkillPrepareError> {
    validate_catalog_identity(catalog, runtime)?;
    let entry = catalog
        .iter()
        .find(|entry| entry.id() == skill_id)
        .ok_or_else(|| SkillPrepareError::MissingSkill {
            skill_id: skill_id.to_owned(),
        })?;
    let app_runtime =
        runtime
            .app_runtime(app)
            .ok_or_else(|| SkillPrepareError::MissingRuntime {
                app: app.as_str().to_owned(),
            })?;
    let snapshots = inspect_installed_skills(std::slice::from_ref(entry), runtime)?;
    let state = snapshots[0]
        .apps()
        .find(|state| state.app() == app)
        .expect("the requested runtime produces an application state");
    let recovery_pending = state.reason() == Some(SkillControlReason::RecoveryPending);
    let managed_reference_drift = state.reason() == Some(SkillControlReason::ManagedReferenceDrift);
    let catalog_drift = state.reason() == Some(SkillControlReason::CatalogDrift);
    let reconciliation_only = recovery_pending || managed_reference_drift || catalog_drift;
    if reconciliation_only && requested.is_some() {
        return Err(SkillPrepareError::Unavailable {
            app: app.as_str().to_owned(),
            reason: state.reason(),
        });
    }
    let selected = state
        .selected()
        .ok_or_else(|| SkillPrepareError::SelectionUnavailable {
            app: app.as_str().to_owned(),
        })?;
    let target_enabled = requested.unwrap_or(selected);
    if !reconciliation_only && target_enabled {
        state
            .enabled()
            .ok_or_else(|| SkillPrepareError::Unavailable {
                app: app.as_str().to_owned(),
                reason: state.reason(),
            })?;
    }
    if !reconciliation_only {
        let target_allowed = if target_enabled {
            state.can_enable()
        } else {
            state.can_disable()
        };
        if !target_allowed {
            return Err(SkillPrepareError::Constrained {
                app: app.as_str().to_owned(),
                target_enabled,
                reason: state.reason(),
            });
        }
    }

    let descriptor = builtin_app_registry().for_app(app);
    let contract = descriptor
        .skill_contract()
        .expect("Skill runtimes require an application contract");
    let direct_discovery_noop = contract.discovery().reads_unified_store()
        && runtime.source_root() == runtime.unified_root();
    let reference =
        (recovery_pending || managed_reference_drift || !direct_discovery_noop).then(|| {
            SkillReferencePlan::new(
                entry.id(),
                app.clone(),
                runtime.source_root(),
                app_runtime.native_root(),
                app_runtime.state_root(),
                entry.directory(),
                target_enabled && !direct_discovery_noop,
            )
        });

    let configuration = match contract.config_target() {
        None => None,
        Some(target) => {
            let document = app_runtime
                .config_document()
                .expect("runtime validates application config observations");
            let projected = project_native_control(
                target,
                document.contents(),
                app_runtime.hermes_platform(),
                entry.name(),
                entry.directory(),
                target_enabled,
            )?;
            projected
                .map(|contents| OperationPlan {
                    contract_major: OPERATION_CONTRACT_MAJOR,
                    app_id: app.as_str().to_owned(),
                    writes: vec![PlannedWrite {
                        target: target.logical_target(),
                        expected: ContentExpectation::for_contents(document.contents()),
                        contents: Some(contents),
                    }],
                })
                .map(|plan| {
                    plan.validate()?;
                    Ok::<_, OperationPlanError>(plan)
                })
                .transpose()?
        }
    };

    let column = contract.catalog_column();
    let catalog_change = if selected == target_enabled {
        None
    } else {
        Some(SkillCatalogChange {
            skill_id: entry.id().to_owned(),
            column,
            expected: selected,
            replacement: target_enabled,
        })
    };

    Ok(SkillSwitchPlan {
        app: app.clone(),
        target_enabled,
        write_order: if target_enabled {
            SkillWriteOrder::ReferenceThenConfiguration
        } else {
            SkillWriteOrder::ConfigurationThenReference
        },
        catalog_guard: SkillCatalogGuard {
            skill_id: entry.id().to_owned(),
            expected_name: entry.name().to_owned(),
            expected_directory: entry.directory().to_owned(),
            expected_selection: Some((column, selected)),
        },
        reference,
        configuration,
        catalog_change,
    })
}

/// A fresh Skill switch plan could not be produced safely.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SkillPrepareError {
    #[error(transparent)]
    Read(#[from] SkillReadError),
    #[error("Skill id is invalid")]
    InvalidSkillId,
    #[error("Skill is missing from the catalog: {skill_id}")]
    MissingSkill { skill_id: String },
    #[error("application '{app}' is missing from the Skill runtime")]
    MissingRuntime { app: String },
    #[error("application '{app}' Skill state is unavailable: {reason:?}")]
    Unavailable {
        app: String,
        reason: Option<SkillControlReason>,
    },
    #[error("application '{app}' native Skill selection is unavailable")]
    SelectionUnavailable { app: String },
    #[error("application '{app}' cannot transition to enabled={target_enabled}: {reason:?}")]
    Constrained {
        app: String,
        target_enabled: bool,
        reason: Option<SkillControlReason>,
    },
    #[error("native Skill configuration cannot be projected: {message}")]
    Configuration { message: String },
    #[error("generated Skill operation plan is invalid: {0}")]
    InvalidOperation(#[from] OperationPlanError),
}

impl From<SkillConfigWriteError> for SkillPrepareError {
    fn from(error: SkillConfigWriteError) -> Self {
        Self::Configuration {
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, convert::Infallible, fs, path::Path};

    use tempfile::tempdir;

    use super::*;
    use crate::{
        skill_catalog_columns, CompareExchangeOutcome, LogicalTarget, ObservedDocument,
        OperationRead, SkillAppRuntime,
    };

    #[derive(Default)]
    struct MemoryHost {
        documents: HashMap<LogicalTarget, Vec<u8>>,
    }

    impl OperationHost for MemoryHost {
        type Resource = LogicalTarget;
        type Error = Infallible;

        fn resolve(&mut self, target: LogicalTarget) -> Result<Self::Resource, Self::Error> {
            Ok(target)
        }

        fn read(
            &mut self,
            resource: &Self::Resource,
            maximum: usize,
        ) -> Result<OperationRead, Self::Error> {
            Ok(match self.documents.get(resource) {
                None => OperationRead::Missing,
                Some(contents) if contents.len() > maximum => OperationRead::TooLarge,
                Some(contents) => OperationRead::Contents(contents.clone()),
            })
        }

        fn compare_exchange(
            &mut self,
            resource: &Self::Resource,
            expected: Option<&[u8]>,
            replacement: Option<&[u8]>,
        ) -> Result<CompareExchangeOutcome, Self::Error> {
            if self.documents.get(resource).map(Vec::as_slice) != expected {
                return Ok(CompareExchangeOutcome::Conflict);
            }
            match replacement {
                Some(contents) => {
                    self.documents.insert(*resource, contents.to_vec());
                }
                None => {
                    self.documents.remove(resource);
                }
            }
            Ok(CompareExchangeOutcome::Applied)
        }
    }

    fn entry(selected: bool) -> SkillCatalogEntry {
        SkillCatalogEntry::try_new(
            "owner/repo:demo",
            "demo",
            None,
            "demo",
            skill_catalog_columns().map(|column| (column, selected)),
        )
        .unwrap()
    }

    fn write_skill(root: &Path) {
        fs::create_dir_all(root.join("demo")).unwrap();
        fs::write(root.join("demo/SKILL.md"), "# Demo\n").unwrap();
    }

    fn runtime(
        source: &Path,
        unified: &Path,
        native: &Path,
        app: AppType,
        config: Option<ObservedDocument>,
    ) -> SkillRuntime {
        let state = source.parent().expect("source has a parent").join("state");
        fs::create_dir_all(native).unwrap();
        fs::create_dir_all(&state).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        }
        SkillRuntime::try_new(
            source,
            unified,
            [SkillAppRuntime::try_new(app, native, state, config).unwrap()],
        )
        .unwrap()
    }

    #[test]
    fn catalog_switches_are_compare_and_swap_plans() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let unified = temporary.path().join("unified");
        let native = temporary.path().join("native");
        write_skill(&source);
        let runtime = runtime(&source, &unified, &native, AppType::Claude, None);
        let catalog = [entry(false)];

        let plan = prepare_skill_switch(
            &catalog,
            "owner/repo:demo",
            &runtime,
            &AppType::Claude,
            true,
        )
        .unwrap();

        assert_eq!(
            plan.write_order(),
            SkillWriteOrder::ReferenceThenConfiguration
        );
        assert!(plan.configuration().is_none());
        assert!(plan.reference().is_some());
        assert_eq!(plan.catalog_guard().skill_id(), "owner/repo:demo");
        assert_eq!(plan.catalog_guard().expected_name(), "demo");
        assert_eq!(plan.catalog_guard().expected_directory(), "demo");
        assert_eq!(
            plan.catalog_guard()
                .expected_selection()
                .map(|(column, selected)| (column.as_str(), selected)),
            Some(("enabled_claude", false))
        );
        assert!(plan.catalog_guard().matches(&catalog[0]));
        assert_eq!(
            plan.decide_catalog(Some(&catalog[0])),
            SkillCatalogDecision::RestoreLive
        );
        assert_eq!(
            plan.decide_catalog(Some(&entry(true))),
            SkillCatalogDecision::KeepLive
        );
        assert_eq!(plan.decide_catalog(None), SkillCatalogDecision::Conflict);
        let change = plan.catalog_change().unwrap();
        assert!(!change.expected());
        assert!(change.replacement());
        assert_eq!(change.column().as_str(), "enabled_claude");
    }

    #[test]
    fn reconciliation_uses_committed_selection_without_another_catalog_write() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let unified = temporary.path().join("unified");
        let native = temporary.path().join("native");
        write_skill(&source);
        let runtime = runtime(&source, &unified, &native, AppType::Claude, None);

        let plan = prepare_skill_reconciliation(
            &[entry(true)],
            "owner/repo:demo",
            &runtime,
            &AppType::Claude,
        )
        .unwrap();

        assert!(plan.target_enabled());
        assert!(plan.reference().is_some());
        assert!(plan.catalog_change().is_none());
        assert_eq!(
            plan.catalog_guard()
                .expected_selection()
                .map(|(column, selected)| (column.as_str(), selected)),
            Some(("enabled_claude", true))
        );
    }

    #[test]
    fn gemini_switches_project_only_its_declared_document() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let native = temporary.path().join("native");
        write_skill(&source);
        let runtime = runtime(
            &source,
            &source,
            &native,
            AppType::Gemini,
            Some(ObservedDocument::present(
                crate::LogicalTarget::GeminiSettings,
                b"{ theme: 'dark' }",
            )),
        );

        let plan = prepare_skill_switch(
            &[entry(true)],
            "owner/repo:demo",
            &runtime,
            &AppType::Gemini,
            false,
        )
        .unwrap();

        let configuration = plan.configuration().unwrap();
        assert_eq!(configuration.writes.len(), 1);
        assert_eq!(
            configuration.writes[0].target,
            crate::LogicalTarget::GeminiSettings
        );
        assert!(configuration.writes[0]
            .contents
            .as_deref()
            .unwrap()
            .contains("theme: 'dark'"));
        assert_eq!(
            plan.write_order(),
            SkillWriteOrder::ConfigurationThenReference
        );
    }

    #[test]
    fn direct_discovery_without_a_native_control_cannot_be_disabled() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let native = temporary.path().join("native");
        write_skill(&source);
        let runtime = runtime(&source, &source, &native, AppType::Codex, None);

        assert!(matches!(
            prepare_skill_switch(
                &[entry(false)],
                "owner/repo:demo",
                &runtime,
                &AppType::Codex,
                false,
            ),
            Err(SkillPrepareError::Constrained { .. })
        ));
    }

    #[test]
    fn direct_discovery_with_a_native_control_needs_no_reference() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let native = temporary.path().join("native");
        write_skill(&source);
        let runtime = runtime(
            &source,
            &source,
            &native,
            AppType::Gemini,
            Some(ObservedDocument::present(
                LogicalTarget::GeminiSettings,
                b"{ skills: { disabled: [] } }",
            )),
        );

        let plan = prepare_skill_switch(
            &[entry(true)],
            "owner/repo:demo",
            &runtime,
            &AppType::Gemini,
            false,
        )
        .unwrap();

        assert!(plan.reference().is_none());
        assert!(plan.configuration().is_some());
    }

    #[test]
    fn prepare_enforces_the_complete_catalog_limit() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let unified = temporary.path().join("unified");
        let native = temporary.path().join("native");
        write_skill(&source);
        let runtime = runtime(&source, &unified, &native, AppType::Claude, None);
        let catalog = vec![entry(false); 10_001];

        assert!(matches!(
            prepare_skill_switch(
                &catalog,
                "owner/repo:demo",
                &runtime,
                &AppType::Claude,
                true,
            ),
            Err(SkillPrepareError::Read(
                SkillReadError::CatalogTooLarge { .. }
            ))
        ));
    }

    #[test]
    fn public_switches_reject_unbounded_or_control_character_ids() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let unified = temporary.path().join("unified");
        let native = temporary.path().join("native");
        write_skill(&source);
        let runtime = runtime(&source, &unified, &native, AppType::Claude, None);
        let catalog = [entry(false)];

        for invalid in ["bad\nlog", &"x".repeat(1025)] {
            assert!(matches!(
                prepare_skill_switch(&catalog, invalid, &runtime, &AppType::Claude, true),
                Err(SkillPrepareError::InvalidSkillId)
            ));
            assert!(matches!(
                prepare_skill_reconciliation(&catalog, invalid, &runtime, &AppType::Claude),
                Err(SkillPrepareError::InvalidSkillId)
            ));
        }
    }

    #[test]
    fn pi_switches_use_the_declared_catalog_column() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let unified = temporary.path().join("unified");
        let native = temporary.path().join("native");
        write_skill(&source);
        let runtime = runtime(&source, &unified, &native, AppType::Pi, None);

        let plan = prepare_skill_switch(
            &[entry(false)],
            "owner/repo:demo",
            &runtime,
            &AppType::Pi,
            true,
        )
        .unwrap();

        let change = plan.catalog_change().expect("Pi catalog change");
        assert_eq!(change.column().as_str(), "enabled_pi");
        assert!(!change.expected());
        assert!(change.replacement());
        assert_eq!(plan.catalog_guard().skill_id(), "owner/repo:demo");
        assert_eq!(
            plan.catalog_guard()
                .expected_selection()
                .map(|(column, selected)| (column.as_str(), selected)),
            Some(("enabled_pi", false))
        );
        assert!(plan.reference().is_some());
    }

    #[test]
    fn a_late_config_conflict_rolls_back_an_enabled_reference() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let unified = temporary.path().join("unified");
        let native = temporary.path().join("native");
        write_skill(&source);
        let original = b"{ skills: { disabled: ['demo'] } }";
        let runtime = runtime(
            &source,
            &unified,
            &native,
            AppType::Gemini,
            Some(ObservedDocument::present(
                LogicalTarget::GeminiSettings,
                original,
            )),
        );
        let plan = prepare_skill_switch(
            &[entry(false)],
            "owner/repo:demo",
            &runtime,
            &AppType::Gemini,
            true,
        )
        .unwrap();
        let mut host = MemoryHost::default();
        host.documents
            .insert(LogicalTarget::GeminiSettings, b"{ changed: true }".to_vec());

        let error = execute_skill_live_plan(&plan, &mut host).unwrap_err();

        assert!(matches!(
            error.failure(),
            SkillLiveFailure::Configuration(_)
        ));
        assert!(error.rollback_failures().is_empty());
        assert!(!native.join("demo").exists());
    }

    #[test]
    fn a_late_reference_conflict_rolls_back_disabled_configuration() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let unified = temporary.path().join("unified");
        let native = temporary.path().join("native");
        write_skill(&source);
        let original = b"{ theme: 'dark' }";
        let baseline_runtime = runtime(
            &source,
            &unified,
            &native,
            AppType::Gemini,
            Some(ObservedDocument::present(
                LogicalTarget::GeminiSettings,
                original,
            )),
        );
        let baseline = prepare_skill_reconciliation(
            &[entry(true)],
            "owner/repo:demo",
            &baseline_runtime,
            &AppType::Gemini,
        )
        .unwrap();
        let mut host = MemoryHost::default();
        host.documents
            .insert(LogicalTarget::GeminiSettings, original.to_vec());
        execute_skill_live_plan(&baseline, &mut host)
            .unwrap()
            .commit()
            .unwrap();
        let switch_runtime = runtime(
            &source,
            &unified,
            &native,
            AppType::Gemini,
            Some(ObservedDocument::present(
                LogicalTarget::GeminiSettings,
                original,
            )),
        );
        let plan = prepare_skill_switch(
            &[entry(true)],
            "owner/repo:demo",
            &switch_runtime,
            &AppType::Gemini,
            false,
        )
        .unwrap();
        fs::remove_file(native.join("demo")).unwrap();
        fs::create_dir_all(native.join("demo")).unwrap();
        fs::write(native.join("demo/SKILL.md"), "external").unwrap();

        let error = execute_skill_live_plan(&plan, &mut host).unwrap_err();

        assert!(matches!(error.failure(), SkillLiveFailure::Reference(_)));
        assert!(error.rollback_failures().is_empty());
        assert_eq!(
            host.documents
                .get(&LogicalTarget::GeminiSettings)
                .map(Vec::as_slice),
            Some(original.as_slice())
        );
        assert_eq!(
            fs::read_to_string(native.join("demo/SKILL.md")).unwrap(),
            "external"
        );
    }

    #[test]
    fn managed_references_round_trip_through_the_read_snapshot() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let unified = temporary.path().join("unified");
        let native = temporary.path().join("native");
        write_skill(&source);
        let before = runtime(&source, &unified, &native, AppType::Claude, None);
        let plan = prepare_skill_switch(
            &[entry(false)],
            "owner/repo:demo",
            &before,
            &AppType::Claude,
            true,
        )
        .unwrap();
        execute_skill_live_plan(&plan, &mut MemoryHost::default())
            .unwrap()
            .commit()
            .unwrap();

        let after = runtime(&source, &unified, &native, AppType::Claude, None);
        let snapshots = inspect_installed_skills(&[entry(true)], &after).unwrap();
        let state = snapshots[0]
            .apps()
            .find(|state| state.app() == &AppType::Claude)
            .unwrap();
        assert_eq!(state.enabled(), Some(true));
        assert!(state.writable());
    }

    #[test]
    fn reconciliation_disables_an_old_reference_without_the_current_source() {
        let temporary = tempdir().unwrap();
        let old_source = temporary.path().join("old-source");
        let missing_source = temporary.path().join("missing-source");
        let unified = temporary.path().join("unified");
        let native = temporary.path().join("native");
        write_skill(&old_source);
        let initial = runtime(&old_source, &unified, &native, AppType::Claude, None);
        let enable = prepare_skill_switch(
            &[entry(false)],
            "owner/repo:demo",
            &initial,
            &AppType::Claude,
            true,
        )
        .unwrap();
        execute_skill_live_plan(&enable, &mut MemoryHost::default())
            .unwrap()
            .commit()
            .unwrap();

        let relocated = runtime(&missing_source, &unified, &native, AppType::Claude, None);
        let unavailable = inspect_installed_skills(&[entry(true)], &relocated).unwrap();
        let state = unavailable[0].apps().next().unwrap();
        assert_eq!(state.reason(), Some(SkillControlReason::MissingSource));
        assert!(state.can_disable());
        let requested_disable = prepare_skill_switch(
            &[entry(true)],
            "owner/repo:demo",
            &relocated,
            &AppType::Claude,
            false,
        )
        .unwrap();
        assert!(!requested_disable.target_enabled());

        let snapshots = inspect_installed_skills(&[entry(false)], &relocated).unwrap();
        let state = snapshots[0].apps().next().unwrap();
        assert_eq!(state.enabled(), Some(true));
        assert_eq!(
            state.reason(),
            Some(SkillControlReason::ManagedReferenceDrift)
        );

        let disable = prepare_skill_reconciliation(
            &[entry(false)],
            "owner/repo:demo",
            &relocated,
            &AppType::Claude,
        )
        .unwrap();
        assert!(!disable.reference().unwrap().enabled());
        execute_skill_live_plan(&disable, &mut MemoryHost::default())
            .unwrap()
            .commit()
            .unwrap();
        assert!(!native.join("demo/SKILL.md").exists());
    }

    #[test]
    fn reconciliation_repairs_reference_drift_in_both_directions() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let unified = temporary.path().join("unified");
        let native = temporary.path().join("native");
        write_skill(&source);
        let initial = runtime(&source, &unified, &native, AppType::Claude, None);

        let snapshots = inspect_installed_skills(&[entry(true)], &initial).unwrap();
        let state = snapshots[0].apps().next().unwrap();
        assert_eq!(
            state.reason(),
            Some(SkillControlReason::ManagedReferenceDrift)
        );

        let enable = prepare_skill_switch(
            &[entry(false)],
            "owner/repo:demo",
            &initial,
            &AppType::Claude,
            true,
        )
        .unwrap();
        execute_skill_live_plan(&enable, &mut MemoryHost::default())
            .unwrap()
            .commit()
            .unwrap();

        let enabled_live = runtime(&source, &unified, &native, AppType::Claude, None);
        let snapshots = inspect_installed_skills(&[entry(false)], &enabled_live).unwrap();
        let state = snapshots[0].apps().next().unwrap();
        assert_eq!(state.enabled(), Some(true));
        assert_eq!(
            state.reason(),
            Some(SkillControlReason::ManagedReferenceDrift)
        );
        assert!(matches!(
            prepare_skill_switch(
                &[entry(false)],
                "owner/repo:demo",
                &enabled_live,
                &AppType::Claude,
                false,
            ),
            Err(SkillPrepareError::Unavailable {
                reason: Some(SkillControlReason::ManagedReferenceDrift),
                ..
            })
        ));
        let disable = prepare_skill_reconciliation(
            &[entry(false)],
            "owner/repo:demo",
            &enabled_live,
            &AppType::Claude,
        )
        .unwrap();
        execute_skill_live_plan(&disable, &mut MemoryHost::default())
            .unwrap()
            .commit()
            .unwrap();

        let disabled_live = runtime(&source, &unified, &native, AppType::Claude, None);
        let enable = prepare_skill_reconciliation(
            &[entry(true)],
            "owner/repo:demo",
            &disabled_live,
            &AppType::Claude,
        )
        .unwrap();
        execute_skill_live_plan(&enable, &mut MemoryHost::default())
            .unwrap()
            .commit()
            .unwrap();
        let enabled_committed = runtime(&source, &unified, &native, AppType::Claude, None);
        let disable = prepare_skill_switch(
            &[entry(true)],
            "owner/repo:demo",
            &enabled_committed,
            &AppType::Claude,
            false,
        )
        .unwrap();
        execute_skill_live_plan(&disable, &mut MemoryHost::default())
            .unwrap()
            .commit()
            .unwrap();

        let disabled_again = runtime(&source, &unified, &native, AppType::Claude, None);
        let snapshots = inspect_installed_skills(&[entry(true)], &disabled_again).unwrap();
        let state = snapshots[0].apps().next().unwrap();
        assert_eq!(state.enabled(), Some(false));
        assert_eq!(
            state.reason(),
            Some(SkillControlReason::ManagedReferenceDrift)
        );
        let restore = prepare_skill_reconciliation(
            &[entry(true)],
            "owner/repo:demo",
            &disabled_again,
            &AppType::Claude,
        )
        .unwrap();
        execute_skill_live_plan(&restore, &mut MemoryHost::default())
            .unwrap()
            .commit()
            .unwrap();

        let restored = runtime(&source, &unified, &native, AppType::Claude, None);
        let snapshots = inspect_installed_skills(&[entry(true)], &restored).unwrap();
        let state = snapshots[0].apps().next().unwrap();
        assert_eq!(state.enabled(), Some(true));
        assert_eq!(state.reason(), None);
    }

    #[test]
    fn direct_discovery_reconciles_configuration_without_creating_a_reference() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let native = temporary.path().join("native");
        write_skill(&source);
        let enabled = b"{ skills: { disabled: [] } }";
        let initial = runtime(
            &source,
            &source,
            &native,
            AppType::Gemini,
            Some(ObservedDocument::present(
                LogicalTarget::GeminiSettings,
                enabled,
            )),
        );
        let snapshots = inspect_installed_skills(&[entry(false)], &initial).unwrap();
        assert_eq!(
            snapshots[0].apps().next().unwrap().reason(),
            Some(SkillControlReason::CatalogDrift)
        );

        let disable = prepare_skill_reconciliation(
            &[entry(false)],
            "owner/repo:demo",
            &initial,
            &AppType::Gemini,
        )
        .unwrap();
        assert!(disable.reference().is_none());
        assert!(disable.configuration().is_some());
        let mut host = MemoryHost::default();
        host.documents
            .insert(LogicalTarget::GeminiSettings, enabled.to_vec());
        execute_skill_live_plan(&disable, &mut host)
            .unwrap()
            .commit()
            .unwrap();

        let disabled = host
            .documents
            .get(&LogicalTarget::GeminiSettings)
            .unwrap()
            .clone();
        let disabled_runtime = runtime(
            &source,
            &source,
            &native,
            AppType::Gemini,
            Some(ObservedDocument::present(
                LogicalTarget::GeminiSettings,
                disabled,
            )),
        );
        let snapshots = inspect_installed_skills(&[entry(false)], &disabled_runtime).unwrap();
        let state = snapshots[0].apps().next().unwrap();
        assert_eq!(state.enabled(), Some(false));
        assert_eq!(state.reason(), None);

        let enable = prepare_skill_reconciliation(
            &[entry(true)],
            "owner/repo:demo",
            &disabled_runtime,
            &AppType::Gemini,
        )
        .unwrap();
        assert!(enable.reference().is_none());
        assert!(enable.configuration().is_some());
        execute_skill_live_plan(&enable, &mut host)
            .unwrap()
            .commit()
            .unwrap();

        let enabled = host
            .documents
            .get(&LogicalTarget::GeminiSettings)
            .unwrap()
            .clone();
        let enabled_runtime = runtime(
            &source,
            &source,
            &native,
            AppType::Gemini,
            Some(ObservedDocument::present(
                LogicalTarget::GeminiSettings,
                enabled,
            )),
        );
        let snapshots = inspect_installed_skills(&[entry(true)], &enabled_runtime).unwrap();
        let state = snapshots[0].apps().next().unwrap();
        assert_eq!(state.enabled(), Some(true));
        assert_eq!(state.reason(), None);
    }
}
