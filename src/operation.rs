//! Product-neutral live-configuration operation contracts.
//!
//! This module describes intended writes and their preconditions. The shared
//! executor sequences those writes through host-owned resources; consumers
//! retain ownership of paths, raw-plan syntax validation, exact I/O, platform
//! security, and locking.

use std::{collections::HashSet, fmt};

use serde::{
    de::{IgnoredAny, SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{builtin_app_adapter, builtin_app_registry, AppType};

/// Current major version of the serialized operation-plan contract.
pub const OPERATION_CONTRACT_MAJOR: u32 = 1;

/// Maximum writes accepted in one built-in application plan.
pub const MAX_OPERATION_WRITES: usize = 4;

/// Maximum UTF-8 payload accepted for one planned write.
pub const MAX_OPERATION_CONTENT_BYTES: usize = 1024 * 1024;

/// Maximum accepted JSON frame for a serialized operation plan.
///
/// Six bytes per decoded content byte covers JSON's longest single-character
/// escape, with additional space for framing fields.
pub const MAX_OPERATION_PLAN_WIRE_BYTES: usize =
    MAX_OPERATION_WRITES * (MAX_OPERATION_CONTENT_BYTES * 6 + 4096);

/// A logical native-configuration document owned by a built-in app adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogicalTarget {
    ClaudeSettings,
    ClaudeDesktopNormalConfig,
    ClaudeDesktopThreepConfig,
    ClaudeDesktopProfile,
    ClaudeDesktopMeta,
    CodexAuth,
    CodexConfig,
    CodexModelCatalog,
    GeminiEnv,
    GeminiSettings,
    GrokConfig,
    OpenCodeConfig,
    OpenClawConfig,
    HermesConfig,
    PiModels,
}

impl LogicalTarget {
    /// Every target in the version-one wire contract.
    pub const ALL: [Self; 15] = [
        Self::ClaudeSettings,
        Self::ClaudeDesktopNormalConfig,
        Self::ClaudeDesktopThreepConfig,
        Self::ClaudeDesktopProfile,
        Self::ClaudeDesktopMeta,
        Self::CodexAuth,
        Self::CodexConfig,
        Self::CodexModelCatalog,
        Self::GeminiEnv,
        Self::GeminiSettings,
        Self::GrokConfig,
        Self::OpenCodeConfig,
        Self::OpenClawConfig,
        Self::HermesConfig,
        Self::PiModels,
    ];

    /// Returns the application that owns this target.
    pub fn app(self) -> AppType {
        match self {
            Self::ClaudeSettings => AppType::Claude,
            Self::ClaudeDesktopNormalConfig
            | Self::ClaudeDesktopThreepConfig
            | Self::ClaudeDesktopProfile
            | Self::ClaudeDesktopMeta => AppType::ClaudeDesktop,
            Self::CodexAuth | Self::CodexConfig | Self::CodexModelCatalog => AppType::Codex,
            Self::GeminiEnv | Self::GeminiSettings => AppType::Gemini,
            Self::GrokConfig => AppType::GrokBuild,
            Self::OpenCodeConfig => AppType::OpenCode,
            Self::OpenClawConfig => AppType::OpenClaw,
            Self::HermesConfig => AppType::Hermes,
            Self::PiModels => AppType::Pi,
        }
    }

    /// Returns the syntax expected for the complete target document.
    pub fn format(self) -> ConfigFormat {
        match self {
            Self::ClaudeSettings
            | Self::ClaudeDesktopNormalConfig
            | Self::ClaudeDesktopThreepConfig
            | Self::ClaudeDesktopProfile
            | Self::ClaudeDesktopMeta
            | Self::CodexAuth
            | Self::CodexModelCatalog
            | Self::GeminiSettings
            | Self::OpenCodeConfig
            | Self::PiModels => ConfigFormat::Json,
            Self::CodexConfig | Self::GrokConfig => ConfigFormat::Toml,
            Self::GeminiEnv => ConfigFormat::Env,
            Self::OpenClawConfig => ConfigFormat::Json5,
            Self::HermesConfig => ConfigFormat::Yaml,
        }
    }

    /// Returns whether the shared contract permits deleting this target.
    pub fn allows_removal(self) -> bool {
        matches!(
            self,
            Self::ClaudeDesktopProfile | Self::CodexAuth | Self::CodexModelCatalog
        )
    }
}

/// Syntax of a complete logical configuration target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigFormat {
    Json,
    Json5,
    Toml,
    Env,
    Yaml,
}

/// Expected state of a target before a planned write is applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum ContentExpectation {
    Missing,
    Sha256 { digest: String },
}

impl<'de> Deserialize<'de> for ContentExpectation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "state", rename_all = "camelCase")]
        enum Wire {
            Missing {
                #[serde(flatten)]
                extra: std::collections::BTreeMap<String, serde_json::Value>,
            },
            Sha256 {
                digest: String,
                #[serde(flatten)]
                extra: std::collections::BTreeMap<String, serde_json::Value>,
            },
        }

        match Wire::deserialize(deserializer)? {
            Wire::Missing { extra } if extra.is_empty() => Ok(Self::Missing),
            Wire::Sha256 { digest, extra } if extra.is_empty() => Ok(Self::Sha256 { digest }),
            Wire::Missing { .. } | Wire::Sha256 { .. } => Err(serde::de::Error::custom(
                "unknown content expectation field",
            )),
        }
    }
}

impl ContentExpectation {
    /// Builds a missing-or-digest expectation from an observed target.
    pub fn for_contents(contents: Option<&[u8]>) -> Self {
        match contents {
            Some(contents) => Self::Sha256 {
                digest: sha256(contents),
            },
            None => Self::Missing,
        }
    }

    /// Returns whether target contents still satisfy this expectation.
    pub fn matches(&self, contents: Option<&[u8]>) -> bool {
        match (self, contents) {
            (Self::Missing, None) => true,
            (Self::Sha256 { digest }, Some(contents)) => *digest == sha256(contents),
            _ => false,
        }
    }
}

/// One complete-document write in a live operation plan.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlannedWrite {
    pub target: LogicalTarget,
    pub expected: ContentExpectation,
    /// `None` requests removal and is accepted only for removable targets.
    pub contents: Option<String>,
}

impl fmt::Debug for PlannedWrite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlannedWrite")
            .field("target", &self.target)
            .field("expected", &self.expected)
            .field("contents", &self.contents.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Serializable, compare-and-swap plan for one built-in application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationPlan {
    pub contract_major: u32,
    pub app_id: String,
    pub writes: Vec<PlannedWrite>,
}

impl OperationPlan {
    /// Decodes and validates a JSON plan under a fixed wire-size bound.
    pub fn decode_json(input: &[u8]) -> Result<Self, OperationPlanDecodeError> {
        if input.len() > MAX_OPERATION_PLAN_WIRE_BYTES {
            return Err(OperationPlanDecodeError::FrameTooLarge {
                limit: MAX_OPERATION_PLAN_WIRE_BYTES,
            });
        }
        let wire: OperationPlanWire = serde_json::from_slice(input)?;
        let plan = Self {
            contract_major: wire.contract_major,
            app_id: wire.app_id,
            writes: wire
                .writes
                .0
                .into_iter()
                .map(|write| PlannedWrite {
                    target: write.target,
                    expected: write.expected,
                    contents: write.contents.0,
                })
                .collect(),
        };
        plan.validate()?;
        Ok(plan)
    }

    /// Validates the shared structural and ownership invariants.
    pub fn validate(&self) -> Result<(), OperationPlanError> {
        let descriptor = builtin_app_registry()
            .descriptors()
            .find(|descriptor| descriptor.id() == self.app_id)
            .ok_or_else(|| OperationPlanError::UnknownApp {
                app_id: self.app_id.clone(),
            })?;
        builtin_app_adapter(descriptor.app()).validate_plan(self)
    }

    /// Validates this plan for a specific built-in adapter.
    pub(crate) fn validate_for(&self, app: &AppType) -> Result<(), OperationPlanError> {
        if self.contract_major != OPERATION_CONTRACT_MAJOR {
            return Err(OperationPlanError::UnsupportedContract {
                actual: self.contract_major,
            });
        }
        if self.app_id != app.as_str() {
            return Err(OperationPlanError::WrongApp {
                expected: app.as_str().to_owned(),
                actual: self.app_id.clone(),
            });
        }
        if self.writes.is_empty() {
            return Err(OperationPlanError::Empty);
        }
        if self.writes.len() > MAX_OPERATION_WRITES {
            return Err(OperationPlanError::TooManyWrites {
                maximum: MAX_OPERATION_WRITES,
            });
        }

        let mut targets = HashSet::new();
        for write in &self.writes {
            if write.target.app() != *app {
                return Err(OperationPlanError::CrossAppTarget {
                    target: write.target,
                });
            }
            if !targets.insert(write.target) {
                return Err(OperationPlanError::DuplicateTarget {
                    target: write.target,
                });
            }
            if write
                .contents
                .as_ref()
                .is_some_and(|contents| contents.len() > MAX_OPERATION_CONTENT_BYTES)
            {
                return Err(OperationPlanError::ContentTooLarge {
                    target: write.target,
                    limit: MAX_OPERATION_CONTENT_BYTES,
                });
            }
            if write.contents.is_none() && !write.target.allows_removal() {
                return Err(OperationPlanError::RemovalNotAllowed {
                    target: write.target,
                });
            }
            if let ContentExpectation::Sha256 { digest } = &write.expected {
                if !valid_sha256(digest) {
                    return Err(OperationPlanError::MalformedDigest {
                        target: write.target,
                    });
                }
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlannedWriteWire {
    target: LogicalTarget,
    expected: ContentExpectation,
    contents: RequiredContents,
}

#[derive(Deserialize)]
struct RequiredContents(Option<String>);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OperationPlanWire {
    contract_major: u32,
    app_id: String,
    writes: BoundedWrites,
}

struct BoundedWrites(Vec<PlannedWriteWire>);

impl<'de> Deserialize<'de> for BoundedWrites {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedWritesVisitor;

        impl<'de> Visitor<'de> for BoundedWritesVisitor {
            type Value = BoundedWrites;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "an operation write list with at most {MAX_OPERATION_WRITES} entries"
                )
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut writes =
                    Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(MAX_OPERATION_WRITES));
                while writes.len() < MAX_OPERATION_WRITES {
                    match sequence.next_element()? {
                        Some(write) => writes.push(write),
                        None => return Ok(BoundedWrites(writes)),
                    }
                }
                if sequence.next_element::<IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::invalid_length(
                        MAX_OPERATION_WRITES + 1,
                        &self,
                    ));
                }
                Ok(BoundedWrites(writes))
            }
        }

        deserializer.deserialize_seq(BoundedWritesVisitor)
    }
}

/// Rejection reason while decoding a bounded JSON operation plan.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OperationPlanDecodeError {
    #[error("operation plan JSON exceeds the {limit}-byte wire limit")]
    FrameTooLarge { limit: usize },
    #[error("operation plan JSON is malformed: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error(transparent)]
    Invalid(#[from] OperationPlanError),
}

/// Rejection reason for a product-neutral operation plan.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum OperationPlanError {
    #[error("unsupported operation contract major {actual}")]
    UnsupportedContract { actual: u32 },
    #[error("unknown application id '{app_id}'")]
    UnknownApp { app_id: String },
    #[error("operation plan is for '{actual}', expected '{expected}'")]
    WrongApp { expected: String, actual: String },
    #[error("operation plan contains no writes")]
    Empty,
    #[error("operation plan exceeds the {maximum}-write limit")]
    TooManyWrites { maximum: usize },
    #[error("logical target {target:?} belongs to another application")]
    CrossAppTarget { target: LogicalTarget },
    #[error("logical target {target:?} appears more than once")]
    DuplicateTarget { target: LogicalTarget },
    #[error("logical target {target:?} is not declared by the adapter")]
    UndeclaredTarget { target: LogicalTarget },
    #[error("logical target {target:?} exceeds the {limit}-byte limit")]
    ContentTooLarge { target: LogicalTarget, limit: usize },
    #[error("logical target {target:?} may not be removed")]
    RemovalNotAllowed { target: LogicalTarget },
    #[error("logical target {target:?} has a malformed SHA-256 precondition")]
    MalformedDigest { target: LogicalTarget },
}

fn sha256(contents: &[u8]) -> String {
    let digest = Sha256::digest(contents);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn valid_sha256(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn write(target: LogicalTarget) -> PlannedWrite {
        PlannedWrite {
            target,
            expected: ContentExpectation::for_contents(Some(b"original")),
            contents: Some("replacement".to_owned()),
        }
    }

    #[test]
    fn expectation_hashes_and_matches_observed_contents() {
        let expectation = ContentExpectation::for_contents(Some(b"original"));

        assert_eq!(
            expectation,
            ContentExpectation::Sha256 {
                digest: "0682c5f2076f099c34cfdd15a9e063849ed437a49677e6fcc5b4198c76575be5"
                    .to_owned()
            }
        );
        assert!(expectation.matches(Some(b"original")));
        assert!(!expectation.matches(Some(b"changed")));
        assert!(!expectation.matches(None));
        assert!(ContentExpectation::Missing.matches(None));
    }

    #[test]
    fn plan_wire_contract_is_stable_and_redacts_debug_output() {
        let plan = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "claude".to_owned(),
            writes: vec![PlannedWrite {
                target: LogicalTarget::ClaudeSettings,
                expected: ContentExpectation::Missing,
                contents: Some("secret-token".to_owned()),
            }],
        };

        assert_eq!(
            serde_json::to_value(&plan).expect("serialize operation plan"),
            json!({
                "contractMajor": 1,
                "appId": "claude",
                "writes": [{
                    "target": "claudeSettings",
                    "expected": {"state": "missing"},
                    "contents": "secret-token"
                }]
            })
        );
        let debug = format!("{plan:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-token"));

        let encoded = serde_json::to_vec(&plan).expect("serialize operation plan");
        assert_eq!(
            OperationPlan::decode_json(&encoded).expect("decode operation plan"),
            plan
        );
    }

    #[test]
    fn validation_rejects_aliases_cross_app_targets_and_unsafe_removals() {
        let alias = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "grok".to_owned(),
            writes: vec![write(LogicalTarget::GrokConfig)],
        };
        assert!(matches!(
            alias.validate(),
            Err(OperationPlanError::UnknownApp { .. })
        ));

        let cross_app = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "claude".to_owned(),
            writes: vec![write(LogicalTarget::CodexConfig)],
        };
        assert!(matches!(
            cross_app.validate(),
            Err(OperationPlanError::CrossAppTarget { .. })
        ));

        let removal = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "claude".to_owned(),
            writes: vec![PlannedWrite {
                target: LogicalTarget::ClaudeSettings,
                expected: ContentExpectation::Missing,
                contents: None,
            }],
        };
        assert!(matches!(
            removal.validate(),
            Err(OperationPlanError::RemovalNotAllowed { .. })
        ));
    }

    #[test]
    fn validation_rejects_duplicate_oversized_and_malformed_writes() {
        let duplicate = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "claude".to_owned(),
            writes: vec![
                write(LogicalTarget::ClaudeSettings),
                write(LogicalTarget::ClaudeSettings),
            ],
        };
        assert!(matches!(
            duplicate.validate(),
            Err(OperationPlanError::DuplicateTarget { .. })
        ));

        let oversized = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "claude".to_owned(),
            writes: vec![PlannedWrite {
                target: LogicalTarget::ClaudeSettings,
                expected: ContentExpectation::Missing,
                contents: Some("x".repeat(MAX_OPERATION_CONTENT_BYTES + 1)),
            }],
        };
        assert!(matches!(
            oversized.validate(),
            Err(OperationPlanError::ContentTooLarge { .. })
        ));

        let malformed = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "claude".to_owned(),
            writes: vec![PlannedWrite {
                target: LogicalTarget::ClaudeSettings,
                expected: ContentExpectation::Sha256 {
                    digest: "ABC".to_owned(),
                },
                contents: Some("replacement".to_owned()),
            }],
        };
        assert!(matches!(
            malformed.validate(),
            Err(OperationPlanError::MalformedDigest { .. })
        ));
    }

    #[test]
    fn validation_rejects_unsupported_empty_and_overfull_plans() {
        let unsupported = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR + 1,
            app_id: "claude".to_owned(),
            writes: vec![write(LogicalTarget::ClaudeSettings)],
        };
        assert!(matches!(
            unsupported.validate(),
            Err(OperationPlanError::UnsupportedContract { .. })
        ));

        let empty = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "claude".to_owned(),
            writes: Vec::new(),
        };
        assert_eq!(empty.validate(), Err(OperationPlanError::Empty));

        let overfull = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "claude".to_owned(),
            writes: vec![write(LogicalTarget::ClaudeSettings); MAX_OPERATION_WRITES + 1],
        };
        assert!(matches!(
            overfull.validate(),
            Err(OperationPlanError::TooManyWrites { .. })
        ));
    }

    #[test]
    fn expectation_deserialization_rejects_unknown_fields() {
        let result = serde_json::from_value::<ContentExpectation>(json!({
            "state": "missing",
            "digest": "unexpected"
        }));

        assert!(result.is_err());
    }

    #[test]
    fn bounded_decoder_rejects_oversized_frames_before_parsing() {
        let input = vec![b' '; MAX_OPERATION_PLAN_WIRE_BYTES + 1];

        assert!(matches!(
            OperationPlan::decode_json(&input),
            Err(OperationPlanDecodeError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn bounded_decoder_requires_explicit_contents_and_caps_writes() {
        let missing_contents = br#"{
            "contractMajor": 1,
            "appId": "codex",
            "writes": [{
                "target": "codexAuth",
                "expected": {"state": "missing"}
            }]
        }"#;
        assert!(matches!(
            OperationPlan::decode_json(missing_contents),
            Err(OperationPlanDecodeError::Malformed(_))
        ));

        let explicit_removal = br#"{
            "contractMajor": 1,
            "appId": "codex",
            "writes": [{
                "target": "codexAuth",
                "expected": {"state": "missing"},
                "contents": null
            }]
        }"#;
        let decoded = OperationPlan::decode_json(explicit_removal)
            .expect("an explicit null keeps the existing removal wire encoding");
        assert_eq!(decoded.writes[0].contents, None);

        let overfull = OperationPlan {
            contract_major: OPERATION_CONTRACT_MAJOR,
            app_id: "claude".to_owned(),
            writes: vec![write(LogicalTarget::ClaudeSettings); MAX_OPERATION_WRITES + 1],
        };
        let encoded = serde_json::to_vec(&overfull).expect("serialize overfull plan");
        assert!(matches!(
            OperationPlan::decode_json(&encoded),
            Err(OperationPlanDecodeError::Malformed(_))
        ));
    }
}
