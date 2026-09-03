//! Host-neutral execution for native live-configuration plans.
//!
//! Core owns validation, compare-and-swap checks, write ordering, and guarded
//! rollback. Hosts retain ownership of resource resolution, bounded exact I/O,
//! platform security, conditional replacement, and locking.

use std::{error::Error, fmt};

use thiserror::Error;

use crate::{
    LogicalTarget, OperationPlan, OperationPlanError, PlannedWrite, MAX_OPERATION_CONTENT_BYTES,
};

/// Product-owned resource access used by the shared operation executor.
///
/// A host must resolve equal physical resources to equal `Resource` values and
/// validate any plan contents that did not come from a built-in Core
/// projection. Reads must stop after `maximum + 1` bytes. Conditional exchanges
/// must compare and replace under the same host synchronization primitive.
///
/// Filesystems generally cannot exclude programs that ignore that primitive.
/// The host must document that platform limit and hold its application lock for
/// the complete plan/receipt lifecycle.
pub trait OperationHost {
    type Resource: Eq;
    type Error;

    /// Resolves a logical target to a stable, host-owned resource identity.
    fn resolve(&mut self, target: LogicalTarget) -> Result<Self::Resource, Self::Error>;

    /// Reads exact bytes under an allocation and I/O bound.
    fn read(
        &mut self,
        resource: &Self::Resource,
        maximum: usize,
    ) -> Result<OperationRead, Self::Error>;

    /// Replaces `expected` with `replacement` as one conditional host action.
    ///
    /// `Conflict` must leave the resource unchanged. Comparison should inspect
    /// at most `expected.len() + 1` bytes. An error may have happened before or
    /// after replacement, so the executor treats its outcome as uncertain and
    /// runs guarded rollback.
    fn compare_exchange(
        &mut self,
        resource: &Self::Resource,
        expected: Option<&[u8]>,
        replacement: Option<&[u8]>,
    ) -> Result<CompareExchangeOutcome, Self::Error>;
}

/// Result of one host-bounded exact read.
pub enum OperationRead {
    Missing,
    Contents(Vec<u8>),
    TooLarge,
}

impl fmt::Debug for OperationRead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => formatter.write_str("Missing"),
            Self::Contents(contents) => formatter
                .debug_struct("Contents")
                .field("bytes", &contents.len())
                .field("value", &"<redacted>")
                .finish(),
            Self::TooLarge => formatter.write_str("TooLarge"),
        }
    }
}

/// Result of a host-owned conditional replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareExchangeOutcome {
    Applied,
    Conflict,
}

/// The primary reason an operation could not complete.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OperationFailure<E> {
    #[error("operation plan is invalid: {0}")]
    InvalidPlan(#[source] OperationPlanError),
    #[error("failed to resolve logical target {target:?}: {source}")]
    Resolve {
        target: LogicalTarget,
        #[source]
        source: E,
    },
    #[error("logical targets {first:?} and {second:?} resolve to the same resource")]
    AliasedTargets {
        first: LogicalTarget,
        second: LogicalTarget,
    },
    #[error("failed to read logical target {target:?}: {source}")]
    Read {
        target: LogicalTarget,
        #[source]
        source: E,
    },
    #[error("logical target {target:?} exceeds the {limit}-byte observation limit")]
    ObservedContentTooLarge { target: LogicalTarget, limit: usize },
    #[error("logical target {target:?} changed while the operation was being prepared")]
    Conflict { target: LogicalTarget },
    #[error("failed to write logical target {target:?}: {source}")]
    Write {
        target: LogicalTarget,
        #[source]
        source: E,
    },
}

/// One target that could not be safely restored.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OperationRollbackFailure<E> {
    #[error("failed to read logical target {target:?} during rollback: {source}")]
    Read {
        target: LogicalTarget,
        #[source]
        source: E,
    },
    #[error(
        "logical target {target:?} exceeds the {limit}-byte observation limit during rollback"
    )]
    ObservedContentTooLarge { target: LogicalTarget, limit: usize },
    #[error("logical target {target:?} changed after the operation wrote it; external contents were preserved")]
    Changed { target: LogicalTarget },
    #[error("failed to restore logical target {target:?}: {source}")]
    Write {
        target: LogicalTarget,
        #[source]
        source: E,
    },
    #[error(
        "logical target {target:?} was not restored because dependency {dependency:?} could not be confirmed restored"
    )]
    Blocked {
        target: LogicalTarget,
        dependency: LogicalTarget,
    },
}

/// An operation failure together with any incomplete rollback work.
#[derive(Debug)]
pub struct OperationExecutionError<E> {
    failure: OperationFailure<E>,
    rollback_failures: Vec<OperationRollbackFailure<E>>,
}

impl<E> OperationExecutionError<E> {
    pub fn failure(&self) -> &OperationFailure<E> {
        &self.failure
    }

    pub fn rollback_failures(&self) -> &[OperationRollbackFailure<E>] {
        &self.rollback_failures
    }

    pub fn into_parts(self) -> (OperationFailure<E>, Vec<OperationRollbackFailure<E>>) {
        (self.failure, self.rollback_failures)
    }
}

impl<E: fmt::Display> fmt::Display for OperationExecutionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.failure)?;
        if !self.rollback_failures.is_empty() {
            write!(
                formatter,
                "; rollback was incomplete for {} target(s)",
                self.rollback_failures.len()
            )?;
        }
        Ok(())
    }
}

impl<E: Error + 'static> Error for OperationExecutionError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.failure)
    }
}

/// Failures encountered while explicitly rolling back a completed operation.
#[derive(Debug)]
pub struct OperationRollbackError<E> {
    failures: Vec<OperationRollbackFailure<E>>,
}

impl<E> OperationRollbackError<E> {
    pub fn failures(&self) -> &[OperationRollbackFailure<E>] {
        &self.failures
    }

    pub fn into_failures(self) -> Vec<OperationRollbackFailure<E>> {
        self.failures
    }
}

impl<E> fmt::Display for OperationRollbackError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "rollback was incomplete for {} target(s)",
            self.failures.len()
        )
    }
}

impl<E: Error + 'static> Error for OperationRollbackError<E> {}

struct PreparedWrite<'a, R> {
    write: &'a PlannedWrite,
    resource: R,
    original: Option<Vec<u8>>,
}

struct AppliedWrite<R> {
    target: LogicalTarget,
    resource: R,
    original: Option<Vec<u8>>,
    written: Option<Vec<u8>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RollbackBehavior {
    BestEffort,
    DependencyOrdered,
}

/// A successful operation that can still be rolled back by its host.
pub struct OperationReceipt<R> {
    applied: Vec<AppliedWrite<R>>,
    rollback_behavior: RollbackBehavior,
}

impl<R> OperationReceipt<R> {
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }

    pub fn rollback<H>(self, host: &mut H) -> Result<(), OperationRollbackError<H::Error>>
    where
        H: OperationHost<Resource = R>,
    {
        let failures = rollback_applied(host, &self.applied, self.rollback_behavior, None);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(OperationRollbackError { failures })
        }
    }
}

impl<R> fmt::Debug for OperationReceipt<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let targets: Vec<_> = self.applied.iter().map(|write| write.target).collect();
        formatter
            .debug_struct("OperationReceipt")
            .field("targets", &targets)
            .finish()
    }
}

/// Executes a validated plan through host-owned resources.
///
/// All resources and preconditions are checked before the first write. Each
/// target is conditionally checked as it is changed. A mid-operation failure
/// rolls back already attempted writes in reverse order, but every restoration
/// is itself conditional on the intended bytes still being present.
pub fn execute_operation_plan<H>(
    plan: &OperationPlan,
    host: &mut H,
) -> Result<OperationReceipt<H::Resource>, OperationExecutionError<H::Error>>
where
    H: OperationHost,
{
    execute_operation_plan_with_rollback(plan, host, RollbackBehavior::BestEffort)
}

/// Executes a plan whose earlier writes are prerequisites of later writes.
///
/// Rollback stops before an earlier write when a later write cannot be
/// confirmed restored. This prevents restoring a prerequisite into a state
/// that is unsafe with the remaining dependent document.
pub fn execute_dependency_ordered_plan<H>(
    plan: &OperationPlan,
    host: &mut H,
) -> Result<OperationReceipt<H::Resource>, OperationExecutionError<H::Error>>
where
    H: OperationHost,
{
    execute_operation_plan_with_rollback(plan, host, RollbackBehavior::DependencyOrdered)
}

fn execute_operation_plan_with_rollback<H>(
    plan: &OperationPlan,
    host: &mut H,
    rollback_behavior: RollbackBehavior,
) -> Result<OperationReceipt<H::Resource>, OperationExecutionError<H::Error>>
where
    H: OperationHost,
{
    plan.validate()
        .map_err(|error| execution_error(OperationFailure::InvalidPlan(error)))?;

    let mut prepared = Vec::with_capacity(plan.writes.len());
    for write in &plan.writes {
        let resource = host.resolve(write.target).map_err(|source| {
            execution_error(OperationFailure::Resolve {
                target: write.target,
                source,
            })
        })?;
        if let Some(existing) = prepared
            .iter()
            .find(|existing: &&PreparedWrite<'_, H::Resource>| existing.resource == resource)
        {
            return Err(execution_error(OperationFailure::AliasedTargets {
                first: existing.write.target,
                second: write.target,
            }));
        }
        prepared.push(PreparedWrite {
            write,
            resource,
            original: None,
        });
    }

    for prepared_write in &mut prepared {
        let original =
            read_for_execution(host, &prepared_write.resource, prepared_write.write.target)
                .map_err(execution_error)?;
        if !prepared_write.write.expected.matches(original.as_deref()) {
            return Err(execution_error(OperationFailure::Conflict {
                target: prepared_write.write.target,
            }));
        }
        prepared_write.original = original;
    }

    let mut applied = Vec::with_capacity(prepared.len());
    for prepared_write in prepared {
        let target = prepared_write.write.target;
        let written = prepared_write
            .write
            .contents
            .as_deref()
            .map(str::as_bytes)
            .map(ToOwned::to_owned);
        if written.is_none() && prepared_write.original.is_none() {
            continue;
        }

        let attempted = AppliedWrite {
            target,
            resource: prepared_write.resource,
            original: prepared_write.original,
            written,
        };
        match host.compare_exchange(
            &attempted.resource,
            attempted.original.as_deref(),
            attempted.written.as_deref(),
        ) {
            Ok(CompareExchangeOutcome::Applied) => applied.push(attempted),
            Ok(CompareExchangeOutcome::Conflict) => {
                return Err(failure_with_rollback(
                    host,
                    OperationFailure::Conflict { target },
                    &applied,
                    rollback_behavior,
                    Some(target),
                ));
            }
            Err(source) => {
                applied.push(attempted);
                return Err(failure_with_rollback(
                    host,
                    OperationFailure::Write { target, source },
                    &applied,
                    rollback_behavior,
                    None,
                ));
            }
        }
    }

    Ok(OperationReceipt {
        applied,
        rollback_behavior,
    })
}

fn read_for_execution<H>(
    host: &mut H,
    resource: &H::Resource,
    target: LogicalTarget,
) -> Result<Option<Vec<u8>>, OperationFailure<H::Error>>
where
    H: OperationHost,
{
    match host
        .read(resource, MAX_OPERATION_CONTENT_BYTES)
        .map_err(|source| OperationFailure::Read { target, source })?
    {
        OperationRead::Missing => Ok(None),
        OperationRead::Contents(contents) if contents.len() <= MAX_OPERATION_CONTENT_BYTES => {
            Ok(Some(contents))
        }
        OperationRead::Contents(_) | OperationRead::TooLarge => {
            Err(OperationFailure::ObservedContentTooLarge {
                target,
                limit: MAX_OPERATION_CONTENT_BYTES,
            })
        }
    }
}

fn execution_error<E>(failure: OperationFailure<E>) -> OperationExecutionError<E> {
    OperationExecutionError {
        failure,
        rollback_failures: Vec::new(),
    }
}

fn failure_with_rollback<H>(
    host: &mut H,
    failure: OperationFailure<H::Error>,
    applied: &[AppliedWrite<H::Resource>],
    rollback_behavior: RollbackBehavior,
    blocked_by: Option<LogicalTarget>,
) -> OperationExecutionError<H::Error>
where
    H: OperationHost,
{
    OperationExecutionError {
        failure,
        rollback_failures: rollback_applied(host, applied, rollback_behavior, blocked_by),
    }
}

fn rollback_applied<H>(
    host: &mut H,
    applied: &[AppliedWrite<H::Resource>],
    behavior: RollbackBehavior,
    mut blocked_by: Option<LogicalTarget>,
) -> Vec<OperationRollbackFailure<H::Error>>
where
    H: OperationHost,
{
    let mut failures = Vec::new();
    for applied_write in applied.iter().rev() {
        if behavior == RollbackBehavior::DependencyOrdered {
            if let Some(dependency) = blocked_by {
                failures.push(OperationRollbackFailure::Blocked {
                    target: applied_write.target,
                    dependency,
                });
                continue;
            }
        }
        let failure_count = failures.len();
        match host.compare_exchange(
            &applied_write.resource,
            applied_write.written.as_deref(),
            applied_write.original.as_deref(),
        ) {
            Ok(CompareExchangeOutcome::Applied) => {}
            Ok(CompareExchangeOutcome::Conflict) => {
                match host.read(&applied_write.resource, MAX_OPERATION_CONTENT_BYTES) {
                    Ok(OperationRead::Missing) if applied_write.original.is_none() => {}
                    Ok(OperationRead::Contents(contents))
                        if contents.len() <= MAX_OPERATION_CONTENT_BYTES
                            && applied_write.original.as_deref() == Some(contents.as_slice()) => {}
                    Ok(OperationRead::Contents(contents))
                        if contents.len() > MAX_OPERATION_CONTENT_BYTES =>
                    {
                        failures.push(OperationRollbackFailure::ObservedContentTooLarge {
                            target: applied_write.target,
                            limit: MAX_OPERATION_CONTENT_BYTES,
                        });
                    }
                    Ok(OperationRead::TooLarge) => {
                        failures.push(OperationRollbackFailure::ObservedContentTooLarge {
                            target: applied_write.target,
                            limit: MAX_OPERATION_CONTENT_BYTES,
                        });
                    }
                    Ok(OperationRead::Missing | OperationRead::Contents(_)) => {
                        failures.push(OperationRollbackFailure::Changed {
                            target: applied_write.target,
                        });
                    }
                    Err(source) => failures.push(OperationRollbackFailure::Read {
                        target: applied_write.target,
                        source,
                    }),
                }
            }
            Err(source) => failures.push(OperationRollbackFailure::Write {
                target: applied_write.target,
                source,
            }),
        }
        if behavior == RollbackBehavior::DependencyOrdered && failures.len() > failure_count {
            blocked_by = Some(applied_write.target);
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{ContentExpectation, PlannedWrite, OPERATION_CONTRACT_MAJOR};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
    enum FakeError {
        #[error("resolve failed")]
        Resolve,
        #[error("read failed")]
        Read,
        #[error("write failed")]
        Write,
    }

    #[derive(Default)]
    struct FakeHost {
        documents: HashMap<u8, Vec<u8>>,
        resources: HashMap<LogicalTarget, u8>,
        reads: HashMap<u8, usize>,
        exchanges: usize,
        exchange_order: Vec<u8>,
        fail_resolve: Option<LogicalTarget>,
        fail_read: Option<(u8, usize)>,
        fail_exchange: Option<usize>,
        apply_failed_exchange: bool,
        mutate_exchange: Option<(u8, Option<Vec<u8>>)>,
    }

    impl FakeHost {
        fn with_document(mut self, target: LogicalTarget, contents: &[u8]) -> Self {
            let resource = resource_for(target);
            self.resources.insert(target, resource);
            self.documents.insert(resource, contents.to_vec());
            self
        }

        fn document(&self, target: LogicalTarget) -> Option<&[u8]> {
            self.documents.get(&resource_for(target)).map(Vec::as_slice)
        }

        fn set_document(&mut self, target: LogicalTarget, contents: &[u8]) {
            self.documents
                .insert(resource_for(target), contents.to_vec());
        }
    }

    impl OperationHost for FakeHost {
        type Resource = u8;
        type Error = FakeError;

        fn resolve(&mut self, target: LogicalTarget) -> Result<Self::Resource, Self::Error> {
            if self.fail_resolve == Some(target) {
                return Err(FakeError::Resolve);
            }
            Ok(*self
                .resources
                .entry(target)
                .or_insert_with(|| resource_for(target)))
        }

        fn read(
            &mut self,
            resource: &Self::Resource,
            maximum: usize,
        ) -> Result<OperationRead, Self::Error> {
            let count = self.reads.entry(*resource).or_default();
            *count += 1;
            if self.fail_read == Some((*resource, *count)) {
                return Err(FakeError::Read);
            }
            match self.documents.get(resource) {
                Some(contents) if contents.len() > maximum => Ok(OperationRead::TooLarge),
                Some(contents) => Ok(OperationRead::Contents(contents.clone())),
                None => Ok(OperationRead::Missing),
            }
        }

        fn compare_exchange(
            &mut self,
            resource: &Self::Resource,
            expected: Option<&[u8]>,
            replacement: Option<&[u8]>,
        ) -> Result<CompareExchangeOutcome, Self::Error> {
            self.exchanges += 1;
            self.exchange_order.push(*resource);
            if self
                .mutate_exchange
                .as_ref()
                .is_some_and(|(target, _)| *target == *resource)
            {
                let (_, contents) = self.mutate_exchange.take().expect("matched mutation");
                match contents {
                    Some(contents) => {
                        self.documents.insert(*resource, contents);
                    }
                    None => {
                        self.documents.remove(resource);
                    }
                }
            }
            if self.documents.get(resource).map(Vec::as_slice) != expected {
                return Ok(CompareExchangeOutcome::Conflict);
            }

            let should_fail = self.fail_exchange == Some(self.exchanges);
            if !should_fail || self.apply_failed_exchange {
                match replacement {
                    Some(contents) => {
                        self.documents.insert(*resource, contents.to_vec());
                    }
                    None => {
                        self.documents.remove(resource);
                    }
                }
            }
            if should_fail {
                Err(FakeError::Write)
            } else {
                Ok(CompareExchangeOutcome::Applied)
            }
        }
    }

    fn resource_for(target: LogicalTarget) -> u8 {
        LogicalTarget::ALL
            .iter()
            .position(|candidate| *candidate == target)
            .expect("known logical target") as u8
    }

    fn codex_plan(writes: &[(LogicalTarget, &[u8], &str)]) -> OperationPlan {
        OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "codex".to_owned(),
            writes: writes
                .iter()
                .map(|(target, original, replacement)| PlannedWrite {
                    target: *target,
                    expected: ContentExpectation::for_contents(Some(original)),
                    contents: Some((*replacement).to_owned()),
                })
                .collect(),
        }
    }

    fn codex_delete_auth_plan() -> OperationPlan {
        OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "codex".to_owned(),
            writes: vec![
                PlannedWrite {
                    target: LogicalTarget::CodexAuth,
                    expected: ContentExpectation::for_contents(Some(b"old-auth")),
                    contents: None,
                },
                PlannedWrite {
                    target: LogicalTarget::CodexConfig,
                    expected: ContentExpectation::for_contents(Some(b"old-config")),
                    contents: Some("third-party-config".to_owned()),
                },
            ],
        }
    }

    #[test]
    fn invalid_plan_fails_before_resolving_resources() {
        let plan = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "codex".to_owned(),
            writes: Vec::new(),
        };
        let mut host = FakeHost {
            fail_resolve: Some(LogicalTarget::CodexAuth),
            ..FakeHost::default()
        };

        let error = execute_operation_plan(&plan, &mut host).expect_err("invalid plan");

        assert!(matches!(
            error.failure(),
            OperationFailure::InvalidPlan(OperationPlanError::Empty)
        ));
        assert!(host.reads.is_empty());
        assert_eq!(host.exchanges, 0);
    }

    #[test]
    fn aliased_resources_fail_before_any_read_or_write() {
        let plan = codex_plan(&[
            (LogicalTarget::CodexAuth, b"auth", "next-auth"),
            (LogicalTarget::CodexConfig, b"config", "next-config"),
        ]);
        let mut host = FakeHost::default();
        host.resources.insert(LogicalTarget::CodexAuth, 7);
        host.resources.insert(LogicalTarget::CodexConfig, 7);

        let error = execute_operation_plan(&plan, &mut host).expect_err("aliased resources");

        assert!(matches!(
            error.failure(),
            OperationFailure::AliasedTargets { .. }
        ));
        assert!(host.reads.is_empty());
        assert_eq!(host.exchanges, 0);
    }

    #[test]
    fn preflight_conflict_never_writes() {
        let plan = codex_plan(&[(LogicalTarget::CodexAuth, b"expected", "replacement")]);
        let mut host = FakeHost::default().with_document(LogicalTarget::CodexAuth, b"external");

        let error = execute_operation_plan(&plan, &mut host).expect_err("conflict");

        assert!(matches!(
            error.failure(),
            OperationFailure::Conflict {
                target: LogicalTarget::CodexAuth
            }
        ));
        assert!(error.rollback_failures().is_empty());
        assert_eq!(
            host.document(LogicalTarget::CodexAuth),
            Some(&b"external"[..])
        );
        assert_eq!(host.exchanges, 0);
    }

    #[test]
    fn conditional_exchange_conflict_rolls_back_earlier_writes_and_preserves_external_bytes() {
        let plan = codex_plan(&[
            (LogicalTarget::CodexAuth, b"auth", "next-auth"),
            (LogicalTarget::CodexConfig, b"config", "next-config"),
        ]);
        let config_resource = resource_for(LogicalTarget::CodexConfig);
        let mut host = FakeHost::default()
            .with_document(LogicalTarget::CodexAuth, b"auth")
            .with_document(LogicalTarget::CodexConfig, b"config");
        host.mutate_exchange = Some((config_resource, Some(b"external".to_vec())));

        let error = execute_operation_plan(&plan, &mut host).expect_err("exchange conflict");

        assert!(matches!(
            error.failure(),
            OperationFailure::Conflict {
                target: LogicalTarget::CodexConfig
            }
        ));
        assert!(error.rollback_failures().is_empty());
        assert_eq!(host.document(LogicalTarget::CodexAuth), Some(&b"auth"[..]));
        assert_eq!(
            host.document(LogicalTarget::CodexConfig),
            Some(&b"external"[..])
        );
    }

    #[test]
    fn failed_write_is_treated_as_possibly_applied_and_fully_rolled_back() {
        let plan = codex_plan(&[
            (LogicalTarget::CodexAuth, b"auth", "next-auth"),
            (LogicalTarget::CodexConfig, b"config", "next-config"),
        ]);
        let mut host = FakeHost::default()
            .with_document(LogicalTarget::CodexAuth, b"auth")
            .with_document(LogicalTarget::CodexConfig, b"config");
        host.fail_exchange = Some(2);
        host.apply_failed_exchange = true;

        let error = execute_operation_plan(&plan, &mut host).expect_err("second write fails");

        assert!(matches!(
            error.failure(),
            OperationFailure::Write {
                target: LogicalTarget::CodexConfig,
                source: FakeError::Write
            }
        ));
        assert!(error.rollback_failures().is_empty());
        assert_eq!(host.document(LogicalTarget::CodexAuth), Some(&b"auth"[..]));
        assert_eq!(
            host.document(LogicalTarget::CodexConfig),
            Some(&b"config"[..])
        );
    }

    #[test]
    fn successful_receipt_can_restore_all_originals() {
        let plan = codex_plan(&[
            (LogicalTarget::CodexAuth, b"auth", "next-auth"),
            (LogicalTarget::CodexConfig, b"config", "next-config"),
        ]);
        let mut host = FakeHost::default()
            .with_document(LogicalTarget::CodexAuth, b"auth")
            .with_document(LogicalTarget::CodexConfig, b"config");

        let receipt = execute_operation_plan(&plan, &mut host).expect("execute plan");
        assert_eq!(
            host.document(LogicalTarget::CodexAuth),
            Some(&b"next-auth"[..])
        );
        assert_eq!(
            host.document(LogicalTarget::CodexConfig),
            Some(&b"next-config"[..])
        );

        receipt.rollback(&mut host).expect("rollback receipt");
        assert_eq!(host.document(LogicalTarget::CodexAuth), Some(&b"auth"[..]));
        assert_eq!(
            host.document(LogicalTarget::CodexConfig),
            Some(&b"config"[..])
        );
    }

    #[test]
    fn receipt_rolls_back_config_before_restoring_deleted_auth() {
        let plan = codex_delete_auth_plan();
        let auth = resource_for(LogicalTarget::CodexAuth);
        let config = resource_for(LogicalTarget::CodexConfig);
        let mut host = FakeHost::default()
            .with_document(LogicalTarget::CodexAuth, b"old-auth")
            .with_document(LogicalTarget::CodexConfig, b"old-config");

        let receipt = execute_dependency_ordered_plan(&plan, &mut host).expect("execute plan");
        receipt.rollback(&mut host).expect("rollback receipt");

        assert_eq!(host.exchange_order, vec![auth, config, config, auth]);
    }

    #[test]
    fn dependent_rollback_does_not_restore_auth_after_config_apply_conflict() {
        let plan = codex_delete_auth_plan();
        let config = resource_for(LogicalTarget::CodexConfig);
        let mut host = FakeHost::default()
            .with_document(LogicalTarget::CodexAuth, b"old-auth")
            .with_document(LogicalTarget::CodexConfig, b"old-config");
        host.mutate_exchange = Some((config, Some(b"external-config".to_vec())));

        let error = execute_dependency_ordered_plan(&plan, &mut host).expect_err("config conflict");

        assert!(matches!(
            error.failure(),
            OperationFailure::Conflict {
                target: LogicalTarget::CodexConfig
            }
        ));
        assert!(matches!(
            error.rollback_failures(),
            [OperationRollbackFailure::Blocked {
                target: LogicalTarget::CodexAuth,
                dependency: LogicalTarget::CodexConfig
            }]
        ));
        assert_eq!(host.document(LogicalTarget::CodexAuth), None);
        assert_eq!(
            host.document(LogicalTarget::CodexConfig),
            Some(&b"external-config"[..])
        );
    }

    #[test]
    fn dependent_receipt_rollback_does_not_restore_auth_after_config_conflict() {
        let plan = codex_delete_auth_plan();
        let mut host = FakeHost::default()
            .with_document(LogicalTarget::CodexAuth, b"old-auth")
            .with_document(LogicalTarget::CodexConfig, b"old-config");
        let receipt = execute_dependency_ordered_plan(&plan, &mut host).expect("execute plan");
        host.set_document(LogicalTarget::CodexConfig, b"external-config");

        let error = receipt.rollback(&mut host).expect_err("config conflict");

        assert!(matches!(
            error.failures(),
            [
                OperationRollbackFailure::Changed {
                    target: LogicalTarget::CodexConfig
                },
                OperationRollbackFailure::Blocked {
                    target: LogicalTarget::CodexAuth,
                    dependency: LogicalTarget::CodexConfig
                }
            ]
        ));
        assert_eq!(host.document(LogicalTarget::CodexAuth), None);
    }

    #[test]
    fn dependent_receipt_rollback_does_not_restore_auth_after_config_error() {
        let plan = codex_delete_auth_plan();
        let mut host = FakeHost::default()
            .with_document(LogicalTarget::CodexAuth, b"old-auth")
            .with_document(LogicalTarget::CodexConfig, b"old-config");
        let receipt = execute_dependency_ordered_plan(&plan, &mut host).expect("execute plan");
        host.fail_exchange = Some(3);

        let error = receipt.rollback(&mut host).expect_err("config write error");

        assert!(matches!(
            error.failures(),
            [
                OperationRollbackFailure::Write {
                    target: LogicalTarget::CodexConfig,
                    source: FakeError::Write
                },
                OperationRollbackFailure::Blocked {
                    target: LogicalTarget::CodexAuth,
                    dependency: LogicalTarget::CodexConfig
                }
            ]
        ));
        assert_eq!(host.document(LogicalTarget::CodexAuth), None);
    }

    #[test]
    fn receipt_rollback_preserves_external_changes_and_continues_other_targets() {
        let plan = codex_plan(&[
            (LogicalTarget::CodexAuth, b"auth", "next-auth"),
            (LogicalTarget::CodexConfig, b"config", "next-config"),
        ]);
        let mut host = FakeHost::default()
            .with_document(LogicalTarget::CodexAuth, b"auth")
            .with_document(LogicalTarget::CodexConfig, b"config");
        let receipt = execute_operation_plan(&plan, &mut host).expect("execute plan");
        host.set_document(LogicalTarget::CodexAuth, b"external");

        let error = receipt
            .rollback(&mut host)
            .expect_err("external edit is preserved");

        assert!(matches!(
            error.failures(),
            [OperationRollbackFailure::Changed {
                target: LogicalTarget::CodexAuth
            }]
        ));
        assert_eq!(
            host.document(LogicalTarget::CodexAuth),
            Some(&b"external"[..])
        );
        assert_eq!(
            host.document(LogicalTarget::CodexConfig),
            Some(&b"config"[..])
        );
    }

    #[test]
    fn observed_documents_are_bounded_and_receipts_redact_bytes() {
        let plan = codex_plan(&[(LogicalTarget::CodexAuth, b"auth", "secret-replacement")]);
        let mut host = FakeHost::default().with_document(
            LogicalTarget::CodexAuth,
            &vec![b'x'; MAX_OPERATION_CONTENT_BYTES + 1],
        );

        let error = execute_operation_plan(&plan, &mut host).expect_err("oversized observation");
        assert!(matches!(
            error.failure(),
            OperationFailure::ObservedContentTooLarge {
                target: LogicalTarget::CodexAuth,
                ..
            }
        ));

        let mut host = FakeHost::default().with_document(LogicalTarget::CodexAuth, b"auth");
        let receipt = execute_operation_plan(&plan, &mut host).expect("execute plan");
        let debug = format!("{receipt:?}");
        assert!(!debug.contains("secret-replacement"));
        assert!(!debug.contains("auth"));
    }
}
