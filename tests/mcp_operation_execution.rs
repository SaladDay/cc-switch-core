use std::collections::HashMap;

use cc_switch_core::{
    builtin_app_adapter, builtin_app_registry, execute_mcp_write_with_content_limit,
    CompareExchangeOutcome, ContentExpectation, McpConfigTarget, McpServerProjection,
    OperationFailure, OperationHost, OperationRead, OperationRollbackFailure,
    MAX_OPERATION_CONTENT_BYTES,
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("fixture I/O failure")]
struct FixtureError;

#[derive(Default)]
struct Host {
    documents: HashMap<McpConfigTarget, Vec<u8>>,
    resolved: Vec<McpConfigTarget>,
    exchanges: usize,
    fail_exchange: Option<(usize, bool)>,
    mutate_exchange: Option<(usize, Vec<u8>)>,
    fail_resolve: bool,
    fail_read: bool,
    unbounded_read: bool,
}

impl OperationHost<McpConfigTarget> for Host {
    type Resource = McpConfigTarget;
    type Error = FixtureError;

    fn resolve(&mut self, target: McpConfigTarget) -> Result<Self::Resource, Self::Error> {
        self.resolved.push(target);
        if self.fail_resolve {
            Err(FixtureError)
        } else {
            Ok(target)
        }
    }

    fn read(
        &mut self,
        target: &Self::Resource,
        maximum: usize,
    ) -> Result<OperationRead, Self::Error> {
        if self.fail_read {
            return Err(FixtureError);
        }
        Ok(match self.documents.get(target) {
            Some(bytes) if !self.unbounded_read && bytes.len() > maximum => OperationRead::TooLarge,
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
        self.exchanges += 1;
        if self
            .mutate_exchange
            .as_ref()
            .is_some_and(|(call, _)| *call == self.exchanges)
        {
            let (_, value) = self.mutate_exchange.take().unwrap();
            self.documents.insert(*target, value);
        }
        if self.documents.get(target).map(Vec::as_slice) != expected {
            return Ok(CompareExchangeOutcome::Conflict);
        }
        let fail = self
            .fail_exchange
            .filter(|(call, _)| *call == self.exchanges);
        if fail.is_none() || fail.is_some_and(|(_, publish)| publish) {
            match replacement {
                Some(bytes) => {
                    self.documents.insert(*target, bytes.to_vec());
                }
                None => {
                    self.documents.remove(target);
                }
            }
        }
        if fail.is_some() {
            Err(FixtureError)
        } else {
            Ok(CompareExchangeOutcome::Applied)
        }
    }
}

#[test]
fn every_registered_mcp_target_uses_projection_and_the_same_recovery_engine() {
    let mut count = 0;
    for descriptor in builtin_app_registry().descriptors() {
        let Some(contract) = descriptor.mcp_contract() else {
            continue;
        };
        count += 1;
        let target = contract.target();
        let adapter = builtin_app_adapter(descriptor.app());
        let old = adapter
            .project_mcp_server(
                None,
                "fixture",
                McpServerProjection::Enable(&json!({"command":"old-not-executed"})),
            )
            .unwrap()
            .unwrap();
        let new = adapter
            .project_mcp_server(
                Some(old.as_bytes()),
                "fixture",
                McpServerProjection::Enable(&json!({"command":"new-not-executed"})),
            )
            .unwrap()
            .unwrap();
        for original in [None, Some(old.as_bytes())] {
            let mut host = Host::default();
            if let Some(bytes) = original {
                host.documents.insert(target, bytes.to_vec());
            }
            let expected = ContentExpectation::for_contents(original);
            let receipt = execute_mcp_write_with_content_limit(
                target,
                &expected,
                &new,
                &mut host,
                MAX_OPERATION_CONTENT_BYTES,
            )
            .unwrap();
            assert_eq!(host.resolved, [target]);
            assert_eq!(host.documents[&target], new.as_bytes());
            assert!(!format!("{receipt:?}").contains("new-not-executed"));
            receipt.rollback(&mut host).unwrap();
            assert_eq!(host.documents.get(&target).map(Vec::as_slice), original);
        }
    }
    assert!(count > 0, "the registry must exercise MCP execution");
}

#[test]
fn invalid_or_oversized_requests_do_not_resolve_resources() {
    let mut host = Host::default();
    let target = McpConfigTarget::Claude;
    let invalid = ContentExpectation::Sha256 {
        digest: "private-invalid-digest".into(),
    };
    let error =
        execute_mcp_write_with_content_limit(target, &invalid, "{}", &mut host, 2).unwrap_err();
    assert!(matches!(
        error.failure(),
        OperationFailure::InvalidExpectation {
            target: McpConfigTarget::Claude
        }
    ));
    assert!(!format!("{error:?}").contains("private-invalid-digest"));
    let error = execute_mcp_write_with_content_limit(
        target,
        &ContentExpectation::Missing,
        "{}",
        &mut host,
        1,
    )
    .unwrap_err();
    assert!(matches!(
        error.failure(),
        OperationFailure::PlannedContentTooLarge { limit: 1, .. }
    ));
    assert!(host.resolved.is_empty());
    assert_eq!(host.exchanges, 0);
}

#[test]
fn read_errors_and_stale_or_oversized_observations_never_write() {
    let target = McpConfigTarget::Hermes;
    for failure in ["resolve", "read", "stale", "bounded", "unbounded"] {
        let mut host = Host {
            fail_resolve: failure == "resolve",
            fail_read: failure == "read",
            unbounded_read: failure == "unbounded",
            ..Host::default()
        };
        host.documents.insert(target, b"fixture: old".to_vec());
        let expected = ContentExpectation::for_contents(Some(b"fixture: old"));
        let expected = if failure == "stale" {
            ContentExpectation::Missing
        } else {
            expected
        };
        let limit = if ["bounded", "unbounded"].contains(&failure) {
            2
        } else {
            100
        };
        let error = execute_mcp_write_with_content_limit(target, &expected, "{}", &mut host, limit)
            .unwrap_err();
        match failure {
            "resolve" => assert!(matches!(error.failure(), OperationFailure::Resolve { .. })),
            "read" => assert!(matches!(error.failure(), OperationFailure::Read { .. })),
            "stale" => assert!(matches!(error.failure(), OperationFailure::Conflict { .. })),
            _ => assert!(matches!(
                error.failure(),
                OperationFailure::ObservedContentTooLarge { .. }
            )),
        }
        assert_eq!(host.documents[&target], b"fixture: old");
        assert_eq!(host.exchanges, 0);
    }
}

#[test]
fn uncertain_publication_recovers_present_and_missing_files() {
    let target = McpConfigTarget::Codex;
    for original in [None, Some(b"fixture='old'".as_slice())] {
        for publish in [false, true] {
            let mut host = Host {
                fail_exchange: Some((1, publish)),
                ..Host::default()
            };
            if let Some(bytes) = original {
                host.documents.insert(target, bytes.to_vec());
            }
            let error = execute_mcp_write_with_content_limit(
                target,
                &ContentExpectation::for_contents(original),
                "fixture='new'",
                &mut host,
                100,
            )
            .unwrap_err();
            assert!(matches!(error.failure(), OperationFailure::Write { .. }));
            assert!(error.rollback_failures().is_empty());
            assert_eq!(host.documents.get(&target).map(Vec::as_slice), original);
        }
    }
}

#[test]
fn conflicts_preserve_external_writes_at_publication_and_recovery() {
    let target = McpConfigTarget::OpenCode;
    for at in [1, 2] {
        let mut host = Host {
            mutate_exchange: Some((at, b"{\"external\":true}".to_vec())),
            fail_exchange: (at == 2).then_some((1, true)),
            ..Host::default()
        };
        let error = execute_mcp_write_with_content_limit(
            target,
            &ContentExpectation::Missing,
            "{}",
            &mut host,
            100,
        )
        .unwrap_err();
        assert_eq!(host.documents[&target], b"{\"external\":true}");
        if at == 1 {
            assert!(matches!(error.failure(), OperationFailure::Conflict { .. }));
            assert!(error.rollback_failures().is_empty());
        } else {
            assert!(matches!(error.failure(), OperationFailure::Write { .. }));
            assert!(matches!(
                error.rollback_failures(),
                [OperationRollbackFailure::Changed { .. }]
            ));
        }
    }
}

#[test]
fn receipts_retain_explicit_limits_and_report_recovery_conflicts() {
    let target = McpConfigTarget::Gemini;
    let large = json!({"private": "x".repeat(MAX_OPERATION_CONTENT_BYTES + 1)}).to_string();
    for changed in [false, true] {
        let mut host = Host::default();
        host.documents.insert(target, large.as_bytes().to_vec());
        let receipt = execute_mcp_write_with_content_limit(
            target,
            &ContentExpectation::for_contents(Some(large.as_bytes())),
            "{}",
            &mut host,
            large.len(),
        )
        .unwrap();
        if changed {
            host.documents
                .insert(target, b"{\"external\":true}".to_vec());
        }
        let result = receipt.rollback(&mut host);
        if changed {
            assert!(matches!(
                result.unwrap_err().failures(),
                [OperationRollbackFailure::Changed { .. }]
            ));
            assert_eq!(host.documents[&target], b"{\"external\":true}");
        } else {
            result.unwrap();
            assert_eq!(host.documents[&target], large.as_bytes());
        }
    }
}
