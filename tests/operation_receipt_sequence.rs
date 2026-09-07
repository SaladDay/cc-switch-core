//! Synthetic host contracts, not compatibility tests for a product workflow.

use std::collections::HashMap;

use cc_switch_core::{
    execute_dependency_ordered_plan, execute_operation_plan, CompareExchangeOutcome,
    ContentExpectation, LogicalTarget, OperationFailure, OperationHost, OperationPlan,
    OperationRead, OperationRollbackFailure, PlannedWrite, OPERATION_CONTRACT_MAJOR,
};

const ENV: LogicalTarget = LogicalTarget::GeminiEnv;
const SETTINGS: LogicalTarget = LogicalTarget::GeminiSettings;
const OLD_ENV: &str = "# original\nGEMINI_API_KEY=old-fake\n";
const OLD_SETTINGS: &str =
    "{\"opaque\":{\"keep\":true},\"security\":{\"auth\":{\"selectedType\":\"oauth-personal\"}}}\n";
const PROVIDER_ENV: &str = "GEMINI_API_KEY=new-fake";
const PROVIDER_SETTINGS: &str =
    "{\"opaque\":{\"keep\":true},\"security\":{\"auth\":{\"selectedType\":\"gemini-api-key\"}}}";
const MCP_SETTINGS: &str = "{\"opaque\":{\"keep\":true},\"security\":{\"auth\":{\"selectedType\":\"gemini-api-key\"}},\"mcpServers\":{\"tool\":{\"command\":\"fake-not-executed\"}}}";
const NEXT_MCP_SETTINGS: &str = "{\"opaque\":{\"keep\":true},\"security\":{\"auth\":{\"selectedType\":\"gemini-api-key\"}},\"mcpServers\":{\"tool\":{\"command\":\"fake-not-executed\"},\"next\":{\"command\":\"also-not-executed\"}}}";

struct Host {
    documents: HashMap<LogicalTarget, Vec<u8>>,
    attempts: Vec<LogicalTarget>,
    // Exchange attempt -> whether to publish before returning the injected error.
    failures: HashMap<usize, bool>,
}

impl Host {
    fn seeded() -> Self {
        Self {
            documents: HashMap::from([
                (ENV, OLD_ENV.as_bytes().to_vec()),
                (SETTINGS, OLD_SETTINGS.as_bytes().to_vec()),
            ]),
            attempts: Vec::new(),
            failures: HashMap::new(),
        }
    }

    fn contents(&self, target: LogicalTarget) -> &[u8] {
        self.documents.get(&target).unwrap()
    }
}

impl OperationHost for Host {
    type Resource = LogicalTarget;
    type Error = &'static str;

    fn resolve(&mut self, target: LogicalTarget) -> Result<Self::Resource, Self::Error> {
        Ok(target)
    }

    fn read(
        &mut self,
        target: &Self::Resource,
        maximum: usize,
    ) -> Result<OperationRead, Self::Error> {
        Ok(match self.documents.get(target) {
            Some(bytes) if bytes.len() > maximum => OperationRead::TooLarge,
            Some(bytes) => OperationRead::Contents(bytes.clone()),
            None => OperationRead::Missing,
        })
    }

    fn compare_exchange(
        &mut self,
        target: &Self::Resource,
        expected: Option<&[u8]>,
        replacement: Option<&[u8]>,
    ) -> Result<CompareExchangeOutcome, Self::Error> {
        self.attempts.push(*target);
        if self.documents.get(target).map(Vec::as_slice) != expected {
            return Ok(CompareExchangeOutcome::Conflict);
        }
        let publish_before_failure = self.failures.get(&self.attempts.len()).copied();
        if publish_before_failure != Some(false) {
            match replacement {
                Some(bytes) => {
                    self.documents.insert(*target, bytes.to_vec());
                }
                None => {
                    self.documents.remove(target);
                }
            }
        }
        if publish_before_failure.is_some() {
            Err("injected write failure")
        } else {
            Ok(CompareExchangeOutcome::Applied)
        }
    }
}

fn plan(writes: &[(LogicalTarget, &str, &str)]) -> OperationPlan {
    OperationPlan {
        contract_major: OPERATION_CONTRACT_MAJOR,
        app_id: "gemini".into(),
        writes: writes
            .iter()
            .map(|(target, before, after)| PlannedWrite {
                target: *target,
                expected: ContentExpectation::for_contents(Some(before.as_bytes())),
                contents: Some((*after).into()),
            })
            .collect(),
    }
}

fn provider_plan() -> OperationPlan {
    plan(&[
        (ENV, OLD_ENV, PROVIDER_ENV),
        (SETTINGS, OLD_SETTINGS, PROVIDER_SETTINGS),
    ])
}

fn mcp_plan() -> OperationPlan {
    plan(&[(SETTINGS, PROVIDER_SETTINGS, MCP_SETTINGS)])
}

#[test]
fn reverse_receipts_restore_exact_bytes_after_three_owned_writes_to_one_resource() {
    let mut host = Host::seeded();
    let provider = execute_dependency_ordered_plan(&provider_plan(), &mut host).unwrap();
    let mcp = execute_operation_plan(&mcp_plan(), &mut host).unwrap();
    let next_mcp = execute_operation_plan(
        &plan(&[(SETTINGS, MCP_SETTINGS, NEXT_MCP_SETTINGS)]),
        &mut host,
    )
    .unwrap();
    assert_eq!(host.contents(SETTINGS), NEXT_MCP_SETTINGS.as_bytes());

    next_mcp.rollback(&mut host).unwrap();
    assert_eq!(host.contents(SETTINGS), MCP_SETTINGS.as_bytes());

    mcp.rollback(&mut host).unwrap();
    assert_eq!(host.contents(SETTINGS), PROVIDER_SETTINGS.as_bytes());
    provider.rollback(&mut host).unwrap();

    assert_eq!(host.contents(ENV), OLD_ENV.as_bytes());
    assert_eq!(host.contents(SETTINGS), OLD_SETTINGS.as_bytes());
    assert_eq!(
        host.attempts,
        [ENV, SETTINGS, SETTINGS, SETTINGS, SETTINGS, SETTINGS, SETTINGS, ENV]
    );
}

#[test]
fn a_failed_followup_recovers_before_the_earlier_receipt_is_restored() {
    for publish_before_failure in [false, true] {
        let mut host = Host::seeded();
        let provider = execute_dependency_ordered_plan(&provider_plan(), &mut host).unwrap();
        host.failures.insert(3, publish_before_failure);

        let failure = execute_operation_plan(&mcp_plan(), &mut host).unwrap_err();
        assert!(matches!(
            failure.failure(),
            OperationFailure::Write {
                target: SETTINGS,
                ..
            }
        ));
        assert!(failure.rollback_failures().is_empty());
        assert_eq!(host.contents(SETTINGS), PROVIDER_SETTINGS.as_bytes());

        provider.rollback(&mut host).unwrap();
        assert_eq!(host.contents(ENV), OLD_ENV.as_bytes());
        assert_eq!(host.contents(SETTINGS), OLD_SETTINGS.as_bytes());
    }
}

#[test]
fn coalesced_receipt_survives_later_publication_and_recovery_failures() {
    for publish_before_failure in [false, true] {
        for recovery_failure in [None, Some(false), Some(true)] {
            let mut host = Host::seeded();
            let mut combined =
                execute_dependency_ordered_plan(&provider_plan(), &mut host).unwrap();
            let mcp = execute_operation_plan(&mcp_plan(), &mut host).unwrap();
            combined.try_coalesce_last_write(mcp).unwrap();
            host.failures.insert(4, publish_before_failure);
            if let Some(publish_before_error) = recovery_failure {
                host.failures.insert(5, publish_before_error);
            }
            let failure = execute_operation_plan(
                &plan(&[(SETTINGS, MCP_SETTINGS, NEXT_MCP_SETTINGS)]),
                &mut host,
            )
            .unwrap_err();
            assert!(matches!(
                failure.failure(),
                OperationFailure::Write {
                    target: SETTINGS,
                    ..
                }
            ));
            assert_eq!(
                !failure.rollback_failures().is_empty(),
                publish_before_failure && recovery_failure.is_some()
            );
            if publish_before_failure && recovery_failure == Some(false) {
                let error = combined.rollback(&mut host).unwrap_err();
                assert!(matches!(
                    error.failures(),
                    [
                        OperationRollbackFailure::Changed { target: SETTINGS },
                        OperationRollbackFailure::Blocked {
                            target: ENV,
                            dependency: SETTINGS
                        },
                    ]
                ));
                assert_eq!(host.contents(ENV), PROVIDER_ENV.as_bytes());
                assert_eq!(host.contents(SETTINGS), NEXT_MCP_SETTINGS.as_bytes());
            } else {
                combined.rollback(&mut host).unwrap();
                assert_eq!(host.contents(ENV), OLD_ENV.as_bytes());
                assert_eq!(host.contents(SETTINGS), OLD_SETTINGS.as_bytes());
            }
        }
    }
}

#[test]
fn failed_followup_recovery_keeps_both_errors_and_blocks_incompatible_restoration() {
    let mut host = Host::seeded();
    let provider = execute_dependency_ordered_plan(&provider_plan(), &mut host).unwrap();
    let first_mcp = execute_operation_plan(&mcp_plan(), &mut host).unwrap();
    host.failures.insert(4, true);
    host.failures.insert(5, false);

    let failure = execute_operation_plan(
        &plan(&[(SETTINGS, MCP_SETTINGS, NEXT_MCP_SETTINGS)]),
        &mut host,
    )
    .unwrap_err();
    assert!(matches!(
        failure.failure(),
        OperationFailure::Write {
            target: SETTINGS,
            ..
        }
    ));
    assert!(matches!(
        failure.rollback_failures(),
        [OperationRollbackFailure::Write {
            target: SETTINGS,
            ..
        }]
    ));
    let previous_mcp_rollback = first_mcp.rollback(&mut host).unwrap_err();
    assert!(matches!(
        previous_mcp_rollback.failures(),
        [OperationRollbackFailure::Changed { target: SETTINGS }]
    ));
    let earlier_rollback = provider.rollback(&mut host).unwrap_err();
    assert!(matches!(
        earlier_rollback.failures(),
        [
            OperationRollbackFailure::Changed { target: SETTINGS },
            OperationRollbackFailure::Blocked {
                target: ENV,
                dependency: SETTINGS
            }
        ]
    ));
    assert_eq!(host.contents(SETTINGS), NEXT_MCP_SETTINGS.as_bytes());
    assert_eq!(host.contents(ENV), PROVIDER_ENV.as_bytes());
}

#[test]
fn an_external_edit_between_plans_is_not_adopted_as_a_new_precondition() {
    let mut host = Host::seeded();
    let provider = execute_dependency_ordered_plan(&provider_plan(), &mut host).unwrap();
    let external = br#"{"external":"keep"}"#.to_vec();
    host.documents.insert(SETTINGS, external.clone());

    let failure = execute_operation_plan(&mcp_plan(), &mut host).unwrap_err();
    assert!(matches!(
        failure.failure(),
        OperationFailure::Conflict { target: SETTINGS }
    ));
    assert_eq!(host.attempts, [ENV, SETTINGS]);
    let rollback = provider.rollback(&mut host).unwrap_err();
    assert!(matches!(
        rollback.failures(),
        [
            OperationRollbackFailure::Changed { target: SETTINGS },
            OperationRollbackFailure::Blocked {
                target: ENV,
                dependency: SETTINGS
            }
        ]
    ));
    assert_eq!(host.contents(SETTINGS), external);
    assert_eq!(host.contents(ENV), PROVIDER_ENV.as_bytes());
}

#[test]
fn an_external_edit_after_followup_blocks_recovery_of_the_dependent_pair() {
    let mut host = Host::seeded();
    let provider = execute_dependency_ordered_plan(&provider_plan(), &mut host).unwrap();
    let mcp = execute_operation_plan(&mcp_plan(), &mut host).unwrap();
    let external = br#"{"external":"after-mcp"}"#.to_vec();
    host.documents.insert(SETTINGS, external.clone());

    let rollback = mcp.rollback(&mut host).unwrap_err();
    assert!(matches!(
        rollback.failures(),
        [OperationRollbackFailure::Changed { target: SETTINGS }]
    ));
    let rollback = provider.rollback(&mut host).unwrap_err();
    assert!(matches!(
        rollback.failures(),
        [
            OperationRollbackFailure::Changed { target: SETTINGS },
            OperationRollbackFailure::Blocked {
                target: ENV,
                dependency: SETTINGS
            }
        ]
    ));
    assert_eq!(host.contents(SETTINGS), external);
    assert_eq!(host.contents(ENV), PROVIDER_ENV.as_bytes());
}

#[test]
fn a_provider_receipt_alone_does_not_own_a_later_untracked_write() {
    let mut host = Host::seeded();
    let provider = execute_dependency_ordered_plan(&provider_plan(), &mut host).unwrap();
    // A host writer that bypasses Core provides no receipt to reverse first.
    host.documents
        .insert(SETTINGS, MCP_SETTINGS.as_bytes().to_vec());

    let rollback = provider.rollback(&mut host).unwrap_err();
    assert!(matches!(
        rollback.failures(),
        [
            OperationRollbackFailure::Changed { target: SETTINGS },
            OperationRollbackFailure::Blocked {
                target: ENV,
                dependency: SETTINGS
            }
        ]
    ));
    assert_eq!(host.contents(SETTINGS), MCP_SETTINGS.as_bytes());
    assert_eq!(host.contents(ENV), PROVIDER_ENV.as_bytes());
}
