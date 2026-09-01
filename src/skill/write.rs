use std::{error::Error, fmt};

use thiserror::Error;

use crate::{
    apply_skill_deployment, builtin_app_registry, execute_operation_plan, AppType,
    ContentExpectation, OperationExecutionError, OperationHost, OperationPlan, OperationPlanError,
    OperationReceipt, OperationRollbackError, PlannedWrite, SkillCatalogColumn,
    SkillDeploymentError, SkillDeploymentReceipt, SkillSelectionStore, OPERATION_CONTRACT_MAJOR,
};

use super::{
    config::{project_native_control, SkillConfigWriteError},
    inspect_installed_skills,
    read::validate_catalog_identity,
    SkillCatalogEntry, SkillControlReason, SkillDeploymentPlan, SkillReadError, SkillRuntime,
    SkillSyncMethod,
};

/// The order in which a host applies the two live parts of a Skill plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillWriteOrder {
    /// Make the Skill visible before removing a native disabled-list entry.
    DeploymentThenConfiguration,
    /// Disable native discovery before removing the native directory entry.
    ConfigurationThenDeployment,
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
    ///
    /// Pi returns `None` because its native directory is the selection store.
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
    deployment: Option<SkillDeploymentPlan>,
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

    pub fn deployment(&self) -> Option<&SkillDeploymentPlan> {
        self.deployment.as_ref()
    }

    pub fn configuration(&self) -> Option<&OperationPlan> {
        self.configuration.as_ref()
    }

    pub fn catalog_change(&self) -> Option<&SkillCatalogChange> {
        self.catalog_change.as_ref()
    }

    pub fn is_live_noop(&self) -> bool {
        self.deployment.is_none() && self.configuration.is_none()
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
    deployment: Option<SkillDeploymentReceipt>,
    configuration: Option<OperationReceipt<R>>,
}

impl<R> SkillLiveReceipt<R> {
    pub fn is_empty(&self) -> bool {
        self.deployment.is_none() && self.configuration.is_none()
    }

    /// Finishes hidden deployment cleanup after the database commit succeeds.
    pub fn commit(self) -> Result<(), SkillDeploymentError> {
        match self.deployment {
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
            self.deployment,
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
            .field("has_deployment", &self.deployment.is_some())
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
        SkillWriteOrder::DeploymentThenConfiguration => {
            let deployment = plan
                .deployment
                .as_ref()
                .map(apply_skill_deployment)
                .transpose()
                .map_err(|failure| SkillLiveExecutionError {
                    failure: SkillLiveFailure::Deployment(failure),
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
                    if let Some(deployment) = deployment {
                        if let Err(error) = deployment.rollback() {
                            rollback_failures.push(SkillLiveRollbackFailure::Deployment(error));
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
                deployment,
                configuration,
            })
        }
        SkillWriteOrder::ConfigurationThenDeployment => {
            let configuration = plan
                .configuration
                .as_ref()
                .map(|configuration| execute_operation_plan(configuration, host))
                .transpose()
                .map_err(|failure| SkillLiveExecutionError {
                    failure: SkillLiveFailure::Configuration(failure),
                    rollback_failures: Vec::new(),
                })?;
            let deployment = match plan
                .deployment
                .as_ref()
                .map(apply_skill_deployment)
                .transpose()
            {
                Ok(deployment) => deployment,
                Err(failure) => {
                    let mut rollback_failures = Vec::new();
                    if let Some(configuration) = configuration {
                        if let Err(error) = configuration.rollback(host) {
                            rollback_failures.push(SkillLiveRollbackFailure::Configuration(error));
                        }
                    }
                    return Err(SkillLiveExecutionError {
                        failure: SkillLiveFailure::Deployment(failure),
                        rollback_failures,
                    });
                }
            };
            Ok(SkillLiveReceipt {
                write_order: plan.write_order,
                deployment,
                configuration,
            })
        }
    }
}

fn rollback_parts<H, R>(
    write_order: SkillWriteOrder,
    deployment: Option<SkillDeploymentReceipt>,
    configuration: Option<OperationReceipt<R>>,
    host: &mut H,
    failures: &mut Vec<SkillLiveRollbackFailure<H::Error>>,
) where
    H: OperationHost<Resource = R>,
{
    match write_order {
        SkillWriteOrder::DeploymentThenConfiguration => {
            rollback_configuration(configuration, host, failures);
            rollback_deployment(deployment, failures);
        }
        SkillWriteOrder::ConfigurationThenDeployment => {
            rollback_deployment(deployment, failures);
            rollback_configuration(configuration, host, failures);
        }
    }
}

fn rollback_deployment<E>(
    deployment: Option<SkillDeploymentReceipt>,
    failures: &mut Vec<SkillLiveRollbackFailure<E>>,
) {
    if let Some(deployment) = deployment {
        if let Err(error) = deployment.rollback() {
            failures.push(SkillLiveRollbackFailure::Deployment(error));
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
    Deployment(SkillDeploymentError),
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
            SkillLiveFailure::Deployment(error) => write!(formatter, "{error}"),
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
    Deployment(SkillDeploymentError),
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
/// The host owns the transaction boundary. It should take
/// `shared_live_config_lock_path`, re-read all inputs, prepare and apply this
/// plan, then open a short immediate database transaction. Inside that
/// transaction it must verify `catalog_guard`, compare-and-swap
/// `catalog_change` when present, and commit. If the commit outcome is
/// uncertain, the host re-reads the committed row, uses
/// [`SkillSwitchPlan::decide_catalog`] for the receipt, and then calls
/// [`prepare_skill_reconciliation`]. Core keeps no second journal or selection
/// store.
pub fn prepare_skill_switch(
    catalog: &[SkillCatalogEntry],
    skill_id: &str,
    runtime: &SkillRuntime,
    app: &AppType,
    target_enabled: bool,
    sync_method: SkillSyncMethod,
) -> Result<SkillSwitchPlan, SkillPrepareError> {
    prepare(
        catalog,
        skill_id,
        runtime,
        app,
        Some(target_enabled),
        sync_method,
    )
}

/// Builds the same live plan using only the already-committed selection.
///
/// `catalog` has the same completeness requirement as
/// [`prepare_skill_switch`].
///
/// Catalog-backed applications converge to their database value. Pi stores
/// selection in its native directory, so reconciliation follows the visible
/// native state and only cleans or completes Core-owned artifacts.
pub fn prepare_skill_reconciliation(
    catalog: &[SkillCatalogEntry],
    skill_id: &str,
    runtime: &SkillRuntime,
    app: &AppType,
    sync_method: SkillSyncMethod,
) -> Result<SkillSwitchPlan, SkillPrepareError> {
    prepare(catalog, skill_id, runtime, app, None, sync_method)
}

fn prepare(
    catalog: &[SkillCatalogEntry],
    skill_id: &str,
    runtime: &SkillRuntime,
    app: &AppType,
    requested: Option<bool>,
    sync_method: SkillSyncMethod,
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
    state
        .enabled()
        .ok_or_else(|| SkillPrepareError::Unavailable {
            app: app.as_str().to_owned(),
            reason: state.reason(),
        })?;
    let selected = state
        .selected()
        .ok_or_else(|| SkillPrepareError::SelectionUnavailable {
            app: app.as_str().to_owned(),
        })?;
    let target_enabled = requested.unwrap_or(selected);

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

    let descriptor = builtin_app_registry().for_app(app);
    let contract = descriptor
        .skill_contract()
        .expect("Skill runtimes require an application contract");
    let direct_discovery_noop =
        state.reason() == Some(SkillControlReason::DirectUnifiedDiscovery) && target_enabled;
    let deployment = (!direct_discovery_noop).then(|| {
        SkillDeploymentPlan::new(
            entry.id(),
            runtime.source_root(),
            app_runtime.native_root(),
            entry.directory(),
            target_enabled,
            sync_method,
            selected,
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

    let catalog_change = match contract.selection_store() {
        SkillSelectionStore::NativeDirectory => None,
        SkillSelectionStore::CatalogColumn(_) if selected == target_enabled => None,
        SkillSelectionStore::CatalogColumn(column) => Some(SkillCatalogChange {
            skill_id: entry.id().to_owned(),
            column,
            expected: selected,
            replacement: target_enabled,
        }),
    };

    Ok(SkillSwitchPlan {
        app: app.clone(),
        target_enabled,
        write_order: if target_enabled {
            SkillWriteOrder::DeploymentThenConfiguration
        } else {
            SkillWriteOrder::ConfigurationThenDeployment
        },
        catalog_guard: SkillCatalogGuard {
            skill_id: entry.id().to_owned(),
            expected_name: entry.name().to_owned(),
            expected_directory: entry.directory().to_owned(),
            expected_selection: contract
                .selection_store()
                .catalog_column()
                .map(|column| (column, selected)),
        },
        deployment,
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
        SkillRuntime::try_new(
            source,
            unified,
            [SkillAppRuntime::try_new(app, native, config).unwrap()],
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
            SkillSyncMethod::Symlink,
        )
        .unwrap();

        assert_eq!(
            plan.write_order(),
            SkillWriteOrder::DeploymentThenConfiguration
        );
        assert!(plan.configuration().is_none());
        assert!(plan.deployment().is_some());
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
            SkillSyncMethod::Symlink,
        )
        .unwrap();

        assert!(plan.target_enabled());
        assert!(plan.deployment().is_some());
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
        let unified = temporary.path().join("unified");
        let native = temporary.path().join("native");
        write_skill(&source);
        let runtime = runtime(
            &source,
            &unified,
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
            SkillSyncMethod::Copy,
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
            SkillWriteOrder::ConfigurationThenDeployment
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
                SkillSyncMethod::Auto,
            ),
            Err(SkillPrepareError::Constrained { .. })
        ));
    }

    #[test]
    fn pi_switches_do_not_invent_a_catalog_column() {
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
            SkillSyncMethod::Symlink,
        )
        .unwrap();

        assert!(plan.catalog_change().is_none());
        assert_eq!(plan.catalog_guard().skill_id(), "owner/repo:demo");
        assert_eq!(plan.catalog_guard().expected_selection(), None);
        assert!(plan.deployment().is_some());
    }

    #[test]
    fn a_late_config_conflict_rolls_back_an_enabled_deployment() {
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
            SkillSyncMethod::Symlink,
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
    fn a_late_deployment_conflict_rolls_back_disabled_configuration() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let unified = temporary.path().join("unified");
        let native = temporary.path().join("native");
        write_skill(&source);
        let original = b"{ theme: 'dark' }";
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
            &[entry(true)],
            "owner/repo:demo",
            &runtime,
            &AppType::Gemini,
            false,
            SkillSyncMethod::Copy,
        )
        .unwrap();
        fs::create_dir_all(native.join("demo")).unwrap();
        fs::write(native.join("demo/SKILL.md"), "external").unwrap();
        let mut host = MemoryHost::default();
        host.documents
            .insert(LogicalTarget::GeminiSettings, original.to_vec());

        let error = execute_skill_live_plan(&plan, &mut host).unwrap_err();

        assert!(matches!(error.failure(), SkillLiveFailure::Deployment(_)));
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
    fn managed_copies_round_trip_through_the_read_snapshot() {
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
            SkillSyncMethod::Copy,
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
}
