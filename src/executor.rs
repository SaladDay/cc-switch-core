//! Host-neutral execution for native live-configuration plans.
//!
//! Core owns validation, compare-and-swap checks, write ordering, and guarded
//! rollback. Hosts retain ownership of resource resolution, bounded exact I/O,
//! platform security, conditional replacement, and locking.

use std::{error::Error, fmt};

use thiserror::Error;

use crate::{
    ContentExpectation, LogicalTarget, McpConfigTarget, OperationPlan, OperationPlanError,
    MAX_OPERATION_CONTENT_BYTES,
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
/// The default target type retains the provider-plan API. MCP publication uses
/// `OperationHost<McpConfigTarget>` to resolve its own document resource.
pub trait OperationHost<Target = LogicalTarget> {
    type Resource: Eq;
    type Error;

    /// Resolves a logical target to a stable, host-owned resource identity.
    fn resolve(&mut self, target: Target) -> Result<Self::Resource, Self::Error>;

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
pub enum OperationFailure<E, Target = LogicalTarget> {
    #[error("operation plan is invalid: {0}")]
    InvalidPlan(#[source] OperationPlanError),
    #[error("target {target:?} has a malformed content expectation")]
    InvalidExpectation { target: Target },
    #[error("planned target {target:?} exceeds the {limit}-byte content limit")]
    PlannedContentTooLarge { target: Target, limit: usize },
    #[error("failed to resolve logical target {target:?}: {source}")]
    Resolve {
        target: Target,
        #[source]
        source: E,
    },
    #[error("logical targets {first:?} and {second:?} resolve to the same resource")]
    AliasedTargets { first: Target, second: Target },
    #[error("failed to read logical target {target:?}: {source}")]
    Read {
        target: Target,
        #[source]
        source: E,
    },
    #[error("logical target {target:?} exceeds the {limit}-byte observation limit")]
    ObservedContentTooLarge { target: Target, limit: usize },
    #[error("logical target {target:?} changed while the operation was being prepared")]
    Conflict { target: Target },
    #[error("failed to write logical target {target:?}: {source}")]
    Write {
        target: Target,
        #[source]
        source: E,
    },
}

/// One target that could not be safely restored.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OperationRollbackFailure<E, Target = LogicalTarget> {
    #[error("failed to read logical target {target:?} during rollback: {source}")]
    Read {
        target: Target,
        #[source]
        source: E,
    },
    #[error(
        "logical target {target:?} exceeds the {limit}-byte observation limit during rollback"
    )]
    ObservedContentTooLarge { target: Target, limit: usize },
    #[error("logical target {target:?} changed after the operation wrote it; external contents were preserved")]
    Changed { target: Target },
    #[error("failed to restore logical target {target:?}: {source}")]
    Write {
        target: Target,
        #[source]
        source: E,
    },
    #[error(
        "logical target {target:?} was not restored because dependency {dependency:?} could not be confirmed restored"
    )]
    Blocked { target: Target, dependency: Target },
}

/// An operation failure together with any incomplete rollback work.
#[derive(Debug)]
pub struct OperationExecutionError<E, Target = LogicalTarget> {
    failure: OperationFailure<E, Target>,
    rollback_failures: Vec<OperationRollbackFailure<E, Target>>,
}

impl<E, Target> OperationExecutionError<E, Target> {
    pub fn failure(&self) -> &OperationFailure<E, Target> {
        &self.failure
    }

    pub fn rollback_failures(&self) -> &[OperationRollbackFailure<E, Target>] {
        &self.rollback_failures
    }

    pub fn into_parts(
        self,
    ) -> (
        OperationFailure<E, Target>,
        Vec<OperationRollbackFailure<E, Target>>,
    ) {
        (self.failure, self.rollback_failures)
    }
}

impl<E: fmt::Display, Target: fmt::Debug> fmt::Display for OperationExecutionError<E, Target> {
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

impl<E: Error + 'static, Target: fmt::Debug + 'static> Error
    for OperationExecutionError<E, Target>
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.failure)
    }
}

/// Failures encountered while explicitly rolling back a completed operation.
#[derive(Debug)]
pub struct OperationRollbackError<E, Target = LogicalTarget> {
    failures: Vec<OperationRollbackFailure<E, Target>>,
}

impl<E, Target> OperationRollbackError<E, Target> {
    pub fn failures(&self) -> &[OperationRollbackFailure<E, Target>] {
        &self.failures
    }

    pub fn into_failures(self) -> Vec<OperationRollbackFailure<E, Target>> {
        self.failures
    }
}

impl<E, Target> fmt::Display for OperationRollbackError<E, Target> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "rollback was incomplete for {} target(s)",
            self.failures.len()
        )
    }
}

impl<E: Error + 'static, Target: fmt::Debug> Error for OperationRollbackError<E, Target> {}

struct WriteView<'a, Target> {
    target: Target,
    expected: &'a ContentExpectation,
    contents: Option<&'a str>,
}

struct PreparedWrite<'a, R, Target> {
    write: &'a WriteView<'a, Target>,
    resource: R,
    original: Option<Vec<u8>>,
}

struct AppliedWrite<R, Target> {
    target: Target,
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
pub struct OperationReceipt<R, Target = LogicalTarget> {
    applied: Vec<AppliedWrite<R, Target>>,
    rollback_behavior: RollbackBehavior,
    maximum_content_bytes: usize,
}

impl<R, Target: Copy + Eq> OperationReceipt<R, Target> {
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }

    /// Combines a consecutive single-write follow-up on this receipt's last target.
    ///
    /// Both target and resource must match, and the follow-up's original bytes
    /// must equal our last written bytes. The earliest original and latest written
    /// contents are retained; intermediate versions are released. Rollback goes
    /// directly to that original, keeping this receipt's dependency order and the
    /// larger content bound. This does not authorize an intervening external edit.
    /// Only combine writes that share a recovery boundary: intermediate versions
    /// can no longer be restored individually after a successful combination.
    ///
    /// Empty follow-ups are harmless. Other follow-ups are returned intact, with
    /// this receipt unchanged, so the host can still recover them separately.
    /// Hold the same host synchronization boundary throughout both operations.
    pub fn try_coalesce_last_write(&mut self, mut followup: Self) -> Result<(), Self>
    where
        R: Eq,
    {
        if followup.is_empty() {
            return Ok(());
        }
        let [next] = followup.applied.as_slice() else {
            return Err(followup);
        };
        let Some(previous) = self.applied.last_mut() else {
            return Err(followup);
        };
        if previous.target != next.target
            || previous.resource != next.resource
            || previous.written != next.original
        {
            return Err(followup);
        }
        previous.written = followup.applied.pop().expect("one follow-up write").written;
        self.maximum_content_bytes = self
            .maximum_content_bytes
            .max(followup.maximum_content_bytes);
        Ok(())
    }

    pub fn rollback<H>(self, host: &mut H) -> Result<(), OperationRollbackError<H::Error, Target>>
    where
        H: OperationHost<Target, Resource = R>,
    {
        let failures = rollback_applied(
            host,
            &self.applied,
            self.rollback_behavior,
            None,
            self.maximum_content_bytes,
        );
        if failures.is_empty() {
            Ok(())
        } else {
            Err(OperationRollbackError { failures })
        }
    }
}

impl<R, Target: fmt::Debug> fmt::Debug for OperationReceipt<R, Target> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let targets: Vec<_> = self.applied.iter().map(|write| &write.target).collect();
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
    execute_operation_plan_with_content_limit(plan, host, MAX_OPERATION_CONTENT_BYTES)
}

/// Executes a locally constructed plan with an explicit per-document bound.
///
/// Hosts that already accept larger native files can choose a bound from their
/// observed and prepared documents. The same bound applies to validation,
/// observations, failure recovery, and receipt rollback. This does not relax
/// [`OperationPlan::decode_json`] or the default execution limits. Do not use it
/// to bypass validation of an untrusted serialized plan.
pub fn execute_operation_plan_with_content_limit<H>(
    plan: &OperationPlan,
    host: &mut H,
    maximum_content_bytes: usize,
) -> Result<OperationReceipt<H::Resource>, OperationExecutionError<H::Error>>
where
    H: OperationHost,
{
    execute_operation_plan_with_rollback(
        plan,
        host,
        RollbackBehavior::BestEffort,
        maximum_content_bytes,
    )
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
    execute_dependency_ordered_plan_with_content_limit(plan, host, MAX_OPERATION_CONTENT_BYTES)
}

/// Executes dependent native writes with a host-selected document bound.
///
/// Like [`execute_operation_plan_with_content_limit`], this is for locally
/// constructed plans, not for bypassing wire validation. The bound also applies
/// to recovery and retained receipts; dependency ordering is unchanged.
pub fn execute_dependency_ordered_plan_with_content_limit<H>(
    plan: &OperationPlan,
    host: &mut H,
    maximum_content_bytes: usize,
) -> Result<OperationReceipt<H::Resource>, OperationExecutionError<H::Error>>
where
    H: OperationHost,
{
    execute_operation_plan_with_rollback(
        plan,
        host,
        RollbackBehavior::DependencyOrdered,
        maximum_content_bytes,
    )
}

fn execute_operation_plan_with_rollback<H>(
    plan: &OperationPlan,
    host: &mut H,
    rollback_behavior: RollbackBehavior,
    maximum_content_bytes: usize,
) -> Result<OperationReceipt<H::Resource>, OperationExecutionError<H::Error>>
where
    H: OperationHost,
{
    plan.validate_with_content_limit(maximum_content_bytes)
        .map_err(|error| execution_error(OperationFailure::InvalidPlan(error)))?;

    let writes = plan
        .writes
        .iter()
        .map(|write| WriteView {
            target: write.target,
            expected: &write.expected,
            contents: write.contents.as_deref(),
        })
        .collect::<Vec<_>>();
    execute_writes(&writes, host, rollback_behavior, maximum_content_bytes)
}

/// Publishes one locally projected MCP document with guarded recovery.
///
/// The host resolves the MCP resource through its application's contract,
/// including host-defined resources such as Claude's MCP file. This does not
/// add MCP files to provider targets or change the serialized plan contract.
/// Hosts must validate document syntax and retain their path, permission and
/// synchronization policies. Hold those protections until the receipt is
/// committed by the host or rolled back, including database failure recovery.
///
/// The explicit bound applies to replacement bytes, preflight observations and
/// rollback. Hosts accepting larger files may derive it from their observed and
/// prepared documents. This is not an untrusted serialized-plan entry point.
/// An MCP collection may be empty, but this API does not delete its containing
/// native document. Rollback can remove a file that was originally absent.
pub fn execute_mcp_write_with_content_limit<H>(
    target: McpConfigTarget,
    expected: &ContentExpectation,
    contents: &str,
    host: &mut H,
    maximum_content_bytes: usize,
) -> Result<
    OperationReceipt<H::Resource, McpConfigTarget>,
    OperationExecutionError<H::Error, McpConfigTarget>,
>
where
    H: OperationHost<McpConfigTarget>,
{
    if let ContentExpectation::Sha256 { digest } = expected {
        if !crate::operation::valid_sha256(digest) {
            return Err(execution_error(OperationFailure::InvalidExpectation {
                target,
            }));
        }
    }
    if contents.len() > maximum_content_bytes {
        return Err(execution_error(OperationFailure::PlannedContentTooLarge {
            target,
            limit: maximum_content_bytes,
        }));
    }
    execute_writes(
        &[WriteView {
            target,
            expected,
            contents: Some(contents),
        }],
        host,
        RollbackBehavior::BestEffort,
        maximum_content_bytes,
    )
}

fn execute_writes<H, Target: Copy + Eq>(
    writes: &[WriteView<'_, Target>],
    host: &mut H,
    rollback_behavior: RollbackBehavior,
    maximum_content_bytes: usize,
) -> Result<OperationReceipt<H::Resource, Target>, OperationExecutionError<H::Error, Target>>
where
    H: OperationHost<Target>,
{
    let mut prepared = Vec::with_capacity(writes.len());
    for write in writes {
        let resource = host.resolve(write.target).map_err(|source| {
            execution_error(OperationFailure::Resolve {
                target: write.target,
                source,
            })
        })?;
        if let Some(existing) =
            prepared
                .iter()
                .find(|existing: &&PreparedWrite<'_, H::Resource, Target>| {
                    existing.resource == resource
                })
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
        let original = read_for_execution(
            host,
            &prepared_write.resource,
            prepared_write.write.target,
            maximum_content_bytes,
        )
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
                    maximum_content_bytes,
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
                    maximum_content_bytes,
                ));
            }
        }
    }

    Ok(OperationReceipt {
        applied,
        rollback_behavior,
        maximum_content_bytes,
    })
}

fn read_for_execution<H, Target>(
    host: &mut H,
    resource: &H::Resource,
    target: Target,
    maximum_content_bytes: usize,
) -> Result<Option<Vec<u8>>, OperationFailure<H::Error, Target>>
where
    H: OperationHost<Target>,
    Target: Copy,
{
    match host
        .read(resource, maximum_content_bytes)
        .map_err(|source| OperationFailure::Read { target, source })?
    {
        OperationRead::Missing => Ok(None),
        OperationRead::Contents(contents) if contents.len() <= maximum_content_bytes => {
            Ok(Some(contents))
        }
        OperationRead::Contents(_) | OperationRead::TooLarge => {
            Err(OperationFailure::ObservedContentTooLarge {
                target,
                limit: maximum_content_bytes,
            })
        }
    }
}

fn execution_error<E, Target>(
    failure: OperationFailure<E, Target>,
) -> OperationExecutionError<E, Target> {
    OperationExecutionError {
        failure,
        rollback_failures: Vec::new(),
    }
}

fn failure_with_rollback<H, Target: Copy + Eq>(
    host: &mut H,
    failure: OperationFailure<H::Error, Target>,
    applied: &[AppliedWrite<H::Resource, Target>],
    rollback_behavior: RollbackBehavior,
    blocked_by: Option<Target>,
    maximum_content_bytes: usize,
) -> OperationExecutionError<H::Error, Target>
where
    H: OperationHost<Target>,
{
    OperationExecutionError {
        failure,
        rollback_failures: rollback_applied(
            host,
            applied,
            rollback_behavior,
            blocked_by,
            maximum_content_bytes,
        ),
    }
}

fn rollback_applied<H, Target: Copy + Eq>(
    host: &mut H,
    applied: &[AppliedWrite<H::Resource, Target>],
    behavior: RollbackBehavior,
    mut blocked_by: Option<Target>,
    maximum_content_bytes: usize,
) -> Vec<OperationRollbackFailure<H::Error, Target>>
where
    H: OperationHost<Target>,
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
                match host.read(&applied_write.resource, maximum_content_bytes) {
                    Ok(OperationRead::Missing) if applied_write.original.is_none() => {}
                    Ok(OperationRead::Contents(contents))
                        if contents.len() <= maximum_content_bytes
                            && applied_write.original.as_deref() == Some(contents.as_slice()) => {}
                    Ok(OperationRead::Contents(contents))
                        if contents.len() > maximum_content_bytes =>
                    {
                        failures.push(OperationRollbackFailure::ObservedContentTooLarge {
                            target: applied_write.target,
                            limit: maximum_content_bytes,
                        });
                    }
                    Ok(OperationRead::TooLarge) => {
                        failures.push(OperationRollbackFailure::ObservedContentTooLarge {
                            target: applied_write.target,
                            limit: maximum_content_bytes,
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
    fn coalesced_followups_keep_bounded_records_and_dependency_recovery() {
        let original = "o".repeat(MAX_OPERATION_CONTENT_BYTES + 1);
        let bound = original.len() + 8;
        for external_edit in [false, true] {
            let mut host = FakeHost::default()
                .with_document(LogicalTarget::CodexAuth, b"old-auth")
                .with_document(LogicalTarget::CodexConfig, original.as_bytes());
            let plan = codex_plan(&[
                (LogicalTarget::CodexAuth, b"old-auth", "next-auth"),
                (
                    LogicalTarget::CodexConfig,
                    original.as_bytes(),
                    "next-config",
                ),
            ]);
            let mut receipt =
                execute_dependency_ordered_plan_with_content_limit(&plan, &mut host, bound)
                    .unwrap();
            for index in 0..64 {
                let before = host.document(LogicalTarget::CodexConfig).unwrap().to_vec();
                let next = format!("{original}-{index}");
                let plan = codex_plan(&[(LogicalTarget::CodexConfig, &before, &next)]);
                let followup =
                    execute_operation_plan_with_content_limit(&plan, &mut host, bound).unwrap();
                receipt.try_coalesce_last_write(followup).unwrap();
                assert_eq!(receipt.applied.len(), 2);
                let config = receipt.applied.last().unwrap();
                assert_eq!(config.original.as_deref(), Some(original.as_bytes()));
                assert_eq!(config.written.as_deref(), Some(next.as_bytes()));
                let retained: usize = receipt
                    .applied
                    .iter()
                    .map(|write| {
                        write.original.as_ref().map_or(0, Vec::len)
                            + write.written.as_ref().map_or(0, Vec::len)
                    })
                    .sum();
                assert!(
                    retained < 2 * bound + 32,
                    "intermediate documents must not accumulate"
                );
            }
            if external_edit {
                host.set_document(LogicalTarget::CodexConfig, b"external");
                let error = receipt.rollback(&mut host).unwrap_err();
                assert!(matches!(
                    error.failures()[1],
                    OperationRollbackFailure::Blocked {
                        target: LogicalTarget::CodexAuth,
                        ..
                    }
                ));
                assert_eq!(
                    host.document(LogicalTarget::CodexConfig),
                    Some(&b"external"[..])
                );
                assert_eq!(
                    host.document(LogicalTarget::CodexAuth),
                    Some(&b"next-auth"[..])
                );
            } else {
                receipt.rollback(&mut host).unwrap();
                assert_eq!(
                    host.document(LogicalTarget::CodexConfig),
                    Some(original.as_bytes())
                );
                assert_eq!(
                    host.document(LogicalTarget::CodexAuth),
                    Some(&b"old-auth"[..])
                );
            }
        }
    }

    #[test]
    fn coalescing_rejects_non_tail_resources_targets_and_discontinuous_bytes() {
        for case in 0..5 {
            let mut host = FakeHost::default()
                .with_document(LogicalTarget::CodexAuth, b"old-auth")
                .with_document(LogicalTarget::CodexConfig, b"old-config");
            let mut receipt = execute_dependency_ordered_plan(
                &codex_plan(&[
                    (LogicalTarget::CodexAuth, b"old-auth", "next-auth"),
                    (LogicalTarget::CodexConfig, b"old-config", "next-config"),
                ]),
                &mut host,
            )
            .unwrap();
            let mut plan =
                codex_plan(&[(LogicalTarget::CodexConfig, b"next-config", "final-config")]);
            match case {
                0 => plan = codex_plan(&[(LogicalTarget::CodexAuth, b"next-auth", "final-auth")]),
                1 => {
                    host.resources.insert(LogicalTarget::CodexConfig, 99);
                    host.documents.insert(99, b"next-config".to_vec());
                }
                2 => {
                    host.resources.insert(
                        LogicalTarget::GeminiSettings,
                        resource_for(LogicalTarget::CodexConfig),
                    );
                    plan.app_id = "gemini".into();
                    plan.writes[0].target = LogicalTarget::GeminiSettings;
                }
                3 => {
                    host.set_document(LogicalTarget::CodexConfig, b"external");
                    plan.writes[0].expected = ContentExpectation::for_contents(Some(b"external"));
                }
                _ => {
                    plan = codex_plan(&[
                        (LogicalTarget::CodexAuth, b"next-auth", "final-auth"),
                        (LogicalTarget::CodexConfig, b"next-config", "final-config"),
                    ])
                }
            }
            let followup = execute_operation_plan(&plan, &mut host).unwrap();
            let rejected = receipt.try_coalesce_last_write(followup).unwrap_err();
            assert_eq!(receipt.applied.len(), 2);
            assert_eq!(
                receipt.applied[1].written.as_deref(),
                Some(&b"next-config"[..])
            );
            rejected.rollback(&mut host).unwrap();
            if case == 3 {
                assert!(receipt.rollback(&mut host).is_err());
                assert_eq!(
                    host.document(LogicalTarget::CodexConfig),
                    Some(&b"external"[..])
                );
            } else {
                receipt.rollback(&mut host).unwrap();
                assert_eq!(
                    host.documents[&resource_for(LogicalTarget::CodexConfig)],
                    b"old-config"
                );
                assert_eq!(
                    host.document(LogicalTarget::CodexAuth),
                    Some(&b"old-auth"[..])
                );
            }
        }
    }

    #[test]
    fn coalescing_preserves_missing_contents_and_the_largest_bound() {
        let mut host = FakeHost::default();
        let mut plan = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "codex".into(),
            writes: vec![PlannedWrite {
                target: LogicalTarget::CodexAuth,
                expected: ContentExpectation::Missing,
                contents: Some("short".into()),
            }],
        };
        let mut receipt = execute_operation_plan_with_content_limit(&plan, &mut host, 5).unwrap();
        plan.writes[0].expected = ContentExpectation::for_contents(Some(b"short"));
        plan.writes[0].contents = None;
        let followup = execute_operation_plan_with_content_limit(&plan, &mut host, 5).unwrap();
        receipt.try_coalesce_last_write(followup).unwrap();
        plan.writes[0].expected = ContentExpectation::Missing;
        let empty = execute_operation_plan_with_content_limit(&plan, &mut host, 5).unwrap();
        receipt.try_coalesce_last_write(empty).unwrap();
        plan.writes[0].contents = Some("longer contents".into());
        let followup = execute_operation_plan_with_content_limit(&plan, &mut host, 15).unwrap();
        receipt.try_coalesce_last_write(followup).unwrap();
        assert_eq!(receipt.maximum_content_bytes, 15);
        assert_eq!(receipt.applied.len(), 1);
        assert!(receipt.applied[0].original.is_none());
        receipt.rollback(&mut host).unwrap();
        assert!(host.document(LogicalTarget::CodexAuth).is_none());
    }

    #[test]
    fn dependent_explicit_bound_recovers_large_native_documents() {
        let original = "o".repeat(MAX_OPERATION_CONTENT_BYTES + 1);
        let replacement = "n".repeat(MAX_OPERATION_CONTENT_BYTES + 2);
        let plan = codex_plan(&[
            (LogicalTarget::CodexAuth, original.as_bytes(), &replacement),
            (
                LogicalTarget::CodexConfig,
                original.as_bytes(),
                &replacement,
            ),
        ]);
        let host = || {
            FakeHost::default()
                .with_document(LogicalTarget::CodexAuth, original.as_bytes())
                .with_document(LogicalTarget::CodexConfig, original.as_bytes())
        };
        let mut too_small = host();
        assert!(execute_dependency_ordered_plan(&plan, &mut too_small).is_err());
        assert_eq!(too_small.exchanges, 0);
        assert!(execute_dependency_ordered_plan_with_content_limit(
            &plan,
            &mut too_small,
            replacement.len() - 1,
        )
        .is_err());
        assert_eq!(too_small.exchanges, 0);

        let mut complete = host();
        let receipt = execute_dependency_ordered_plan_with_content_limit(
            &plan,
            &mut complete,
            replacement.len(),
        )
        .unwrap();
        receipt.rollback(&mut complete).unwrap();
        for published_before_error in [false, true] {
            let mut failed = host();
            failed.fail_exchange = Some(2);
            failed.apply_failed_exchange = published_before_error;
            let error = execute_dependency_ordered_plan_with_content_limit(
                &plan,
                &mut failed,
                replacement.len(),
            )
            .unwrap_err();
            assert!(matches!(error.failure(), OperationFailure::Write { .. }));
            assert!(error.rollback_failures().is_empty());
            for target in [LogicalTarget::CodexAuth, LogicalTarget::CodexConfig] {
                assert_eq!(failed.document(target), Some(original.as_bytes()));
                assert_eq!(complete.document(target), Some(original.as_bytes()));
            }
        }
    }

    #[test]
    fn dependent_explicit_bound_preserves_dependency_and_recovery_limits() {
        let original = "o".repeat(MAX_OPERATION_CONTENT_BYTES + 1);
        let plan = codex_plan(&[
            (LogicalTarget::CodexAuth, b"auth", "next-auth"),
            (
                LogicalTarget::CodexConfig,
                original.as_bytes(),
                "next-config",
            ),
        ]);
        for external in [b"external".to_vec(), vec![b'x'; original.len() + 1]] {
            let mut host = FakeHost::default()
                .with_document(LogicalTarget::CodexAuth, b"auth")
                .with_document(LogicalTarget::CodexConfig, original.as_bytes());
            let receipt = execute_dependency_ordered_plan_with_content_limit(
                &plan,
                &mut host,
                original.len(),
            )
            .unwrap();
            host.set_document(LogicalTarget::CodexConfig, &external);
            let error = receipt.rollback(&mut host).unwrap_err();
            assert!(matches!(
                error.failures().last(),
                Some(OperationRollbackFailure::Blocked {
                    target: LogicalTarget::CodexAuth,
                    dependency: LogicalTarget::CodexConfig,
                })
            ));
            if external.len() > original.len() {
                assert!(
                    matches!(error.failures()[0], OperationRollbackFailure::ObservedContentTooLarge { limit, .. } if limit == original.len())
                );
            }
            assert_eq!(
                host.document(LogicalTarget::CodexAuth),
                Some(&b"next-auth"[..])
            );
            assert_eq!(
                host.document(LogicalTarget::CodexConfig),
                Some(external.as_slice())
            );
        }
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
    fn explicit_content_limit_preserves_default_and_wire_bounds() {
        let large = "x".repeat(MAX_OPERATION_CONTENT_BYTES + 1);
        let plan = codex_plan(&[(LogicalTarget::CodexConfig, b"old", &large)]);
        let mut host = FakeHost::default().with_document(LogicalTarget::CodexConfig, b"old");
        assert!(execute_operation_plan(&plan, &mut host).is_err());
        assert!(OperationPlan::decode_json(&serde_json::to_vec(&plan).unwrap()).is_err());
        assert_eq!(host.exchanges, 0);

        let receipt = execute_operation_plan_with_content_limit(&plan, &mut host, large.len())
            .expect("host accepts larger native files");
        assert_eq!(
            host.document(LogicalTarget::CodexConfig),
            Some(large.as_bytes())
        );
        receipt.rollback(&mut host).unwrap();
        assert_eq!(
            host.document(LogicalTarget::CodexConfig),
            Some(b"old".as_slice())
        );
    }

    #[test]
    fn explicit_content_limit_checks_prepared_and_observed_bytes_before_writing() {
        let plan = codex_plan(&[(LogicalTarget::CodexConfig, b"long-old", "new")]);
        let mut host = FakeHost::default().with_document(LogicalTarget::CodexConfig, b"long-old");
        let error = execute_operation_plan_with_content_limit(&plan, &mut host, 2).unwrap_err();
        assert!(matches!(
            error.failure(),
            OperationFailure::InvalidPlan(OperationPlanError::ContentTooLarge { limit: 2, .. })
        ));
        let error = execute_operation_plan_with_content_limit(&plan, &mut host, 3).unwrap_err();
        assert!(matches!(
            error.failure(),
            OperationFailure::ObservedContentTooLarge { limit: 3, .. }
        ));
        assert_eq!(host.exchanges, 0);
    }

    #[test]
    fn explicit_content_limit_is_retained_for_failure_and_receipt_recovery() {
        let large = "x".repeat(MAX_OPERATION_CONTENT_BYTES + 1);
        let plan = codex_plan(&[
            (LogicalTarget::CodexAuth, large.as_bytes(), "new-auth"),
            (LogicalTarget::CodexConfig, large.as_bytes(), "new-config"),
        ]);
        let mut host = FakeHost::default()
            .with_document(LogicalTarget::CodexAuth, large.as_bytes())
            .with_document(LogicalTarget::CodexConfig, large.as_bytes());
        host.fail_exchange = Some(2);
        let error =
            execute_operation_plan_with_content_limit(&plan, &mut host, large.len()).unwrap_err();
        assert!(
            error.rollback_failures().is_empty(),
            "unchanged large config counts as restored"
        );
        assert_eq!(
            host.document(LogicalTarget::CodexAuth),
            Some(large.as_bytes())
        );

        host.fail_exchange = None;
        let receipt =
            execute_operation_plan_with_content_limit(&plan, &mut host, large.len()).unwrap();
        host.set_document(LogicalTarget::CodexConfig, large.as_bytes());
        receipt
            .rollback(&mut host)
            .expect("already restored large config is accepted");
        assert_eq!(
            host.document(LogicalTarget::CodexAuth),
            Some(large.as_bytes())
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
