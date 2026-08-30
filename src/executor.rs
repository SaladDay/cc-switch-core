//! Host-neutral execution for native live-configuration plans.
//!
//! Core owns validation, compare-and-swap checks, write ordering, and guarded
//! rollback. Hosts retain ownership of resource resolution, exact I/O,
//! platform security, atomic replacement, and locking.

use std::{error::Error, fmt};

use thiserror::Error;

use crate::{
    LogicalTarget, OperationPlan, OperationPlanError, PlannedWrite, MAX_OPERATION_CONTENT_BYTES,
};

/// Product-owned resource access used by the shared operation executor.
///
/// A host must resolve equal physical resources to equal `Resource` values,
/// validate any plan contents that did not come from a built-in Core
/// projection, return exact bytes from `read`, and make each `write` an atomic
/// replacement or removal whenever the platform permits it. The host is also
/// responsible for holding its application lock for the complete plan/receipt
/// lifecycle.
pub trait OperationHost {
    type Resource: Eq;
    type Error;

    /// Resolves a logical target to a stable, host-owned resource identity.
    fn resolve(&mut self, target: LogicalTarget) -> Result<Self::Resource, Self::Error>;

    /// Reads exact bytes, returning `None` only when the resource is absent.
    fn read(&mut self, resource: &Self::Resource) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Replaces a resource with exact bytes, or removes it for `None`.
    fn write(
        &mut self,
        resource: &Self::Resource,
        contents: Option<&[u8]>,
    ) -> Result<(), Self::Error>;
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

/// A successful operation that can still be rolled back by its host.
pub struct OperationReceipt<R> {
    applied: Vec<AppliedWrite<R>>,
}

impl<R> OperationReceipt<R> {
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }

    pub fn rollback<H>(self, host: &mut H) -> Result<(), OperationRollbackError<H::Error>>
    where
        H: OperationHost<Resource = R>,
    {
        let failures = rollback_applied(host, &self.applied);
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
/// target is checked again immediately before it is changed. A mid-operation
/// failure rolls back already attempted writes in reverse order, but rollback
/// never overwrites bytes that differ from both the original and intended
/// contents.
pub fn execute_operation_plan<H>(
    plan: &OperationPlan,
    host: &mut H,
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
        let current = match read_for_execution(host, &prepared_write.resource, target) {
            Ok(current) => current,
            Err(failure) => return Err(failure_with_rollback(host, failure, &applied)),
        };
        if current != prepared_write.original {
            return Err(failure_with_rollback(
                host,
                OperationFailure::Conflict { target },
                &applied,
            ));
        }

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
        if let Err(source) = host.write(&attempted.resource, attempted.written.as_deref()) {
            applied.push(attempted);
            return Err(failure_with_rollback(
                host,
                OperationFailure::Write { target, source },
                &applied,
            ));
        }
        applied.push(attempted);
    }

    Ok(OperationReceipt { applied })
}

fn read_for_execution<H>(
    host: &mut H,
    resource: &H::Resource,
    target: LogicalTarget,
) -> Result<Option<Vec<u8>>, OperationFailure<H::Error>>
where
    H: OperationHost,
{
    let contents = host
        .read(resource)
        .map_err(|source| OperationFailure::Read { target, source })?;
    if contents
        .as_ref()
        .is_some_and(|contents| contents.len() > MAX_OPERATION_CONTENT_BYTES)
    {
        return Err(OperationFailure::ObservedContentTooLarge {
            target,
            limit: MAX_OPERATION_CONTENT_BYTES,
        });
    }
    Ok(contents)
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
) -> OperationExecutionError<H::Error>
where
    H: OperationHost,
{
    OperationExecutionError {
        failure,
        rollback_failures: rollback_applied(host, applied),
    }
}

fn rollback_applied<H>(
    host: &mut H,
    applied: &[AppliedWrite<H::Resource>],
) -> Vec<OperationRollbackFailure<H::Error>>
where
    H: OperationHost,
{
    let mut failures = Vec::new();
    for applied_write in applied.iter().rev() {
        let current = match host.read(&applied_write.resource) {
            Ok(contents)
                if contents
                    .as_ref()
                    .is_some_and(|contents| contents.len() > MAX_OPERATION_CONTENT_BYTES) =>
            {
                failures.push(OperationRollbackFailure::ObservedContentTooLarge {
                    target: applied_write.target,
                    limit: MAX_OPERATION_CONTENT_BYTES,
                });
                continue;
            }
            Ok(contents) => contents,
            Err(source) => {
                failures.push(OperationRollbackFailure::Read {
                    target: applied_write.target,
                    source,
                });
                continue;
            }
        };

        if current == applied_write.original {
            continue;
        }
        if current != applied_write.written {
            failures.push(OperationRollbackFailure::Changed {
                target: applied_write.target,
            });
            continue;
        }
        if let Err(source) = host.write(&applied_write.resource, applied_write.original.as_deref())
        {
            failures.push(OperationRollbackFailure::Write {
                target: applied_write.target,
                source,
            });
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
        writes: usize,
        fail_resolve: Option<LogicalTarget>,
        fail_read: Option<(u8, usize)>,
        fail_write: Option<usize>,
        apply_failed_write: bool,
        mutate_read: Option<(u8, usize, Option<Vec<u8>>)>,
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

        fn read(&mut self, resource: &Self::Resource) -> Result<Option<Vec<u8>>, Self::Error> {
            let count = self.reads.entry(*resource).or_default();
            *count += 1;
            if self.fail_read == Some((*resource, *count)) {
                return Err(FakeError::Read);
            }
            if self
                .mutate_read
                .as_ref()
                .is_some_and(|(target, read, _)| *target == *resource && *read == *count)
            {
                let (_, _, contents) = self.mutate_read.take().expect("matched mutation");
                match contents {
                    Some(contents) => {
                        self.documents.insert(*resource, contents);
                    }
                    None => {
                        self.documents.remove(resource);
                    }
                }
            }
            Ok(self.documents.get(resource).cloned())
        }

        fn write(
            &mut self,
            resource: &Self::Resource,
            contents: Option<&[u8]>,
        ) -> Result<(), Self::Error> {
            self.writes += 1;
            let should_fail = self.fail_write == Some(self.writes);
            if !should_fail || self.apply_failed_write {
                match contents {
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
                Ok(())
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
        assert_eq!(host.writes, 0);
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
        assert_eq!(host.writes, 0);
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
        assert_eq!(host.writes, 0);
    }

    #[test]
    fn recheck_conflict_rolls_back_earlier_writes_and_preserves_external_bytes() {
        let plan = codex_plan(&[
            (LogicalTarget::CodexAuth, b"auth", "next-auth"),
            (LogicalTarget::CodexConfig, b"config", "next-config"),
        ]);
        let config_resource = resource_for(LogicalTarget::CodexConfig);
        let mut host = FakeHost::default()
            .with_document(LogicalTarget::CodexAuth, b"auth")
            .with_document(LogicalTarget::CodexConfig, b"config");
        host.mutate_read = Some((config_resource, 2, Some(b"external".to_vec())));

        let error = execute_operation_plan(&plan, &mut host).expect_err("recheck conflict");

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
        host.fail_write = Some(2);
        host.apply_failed_write = true;

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
