//! Observed live documents supplied by a consumer-owned host.
//!
//! The host remains responsible for resolving paths and reading files. This
//! module gives shared projectors a complete, bounded snapshot without moving
//! filesystem ownership into the core crate.

use std::{collections::HashMap, fmt};

use thiserror::Error;

use crate::{builtin_app_adapter, AppType, LogicalTarget, MAX_OPERATION_CONTENT_BYTES};

/// One observed logical target.
///
/// The target is explicitly present, missing, or unobserved. Contents are
/// deliberately omitted from `Debug` because live documents may contain
/// credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct ObservedDocument {
    target: LogicalTarget,
    observed: bool,
    contents: Option<Vec<u8>>,
}

impl ObservedDocument {
    /// Records an existing target.
    pub fn present(target: LogicalTarget, contents: impl Into<Vec<u8>>) -> Self {
        Self {
            target,
            observed: true,
            contents: Some(contents.into()),
        }
    }

    /// Records a target that did not exist when it was observed.
    pub fn missing(target: LogicalTarget) -> Self {
        Self {
            target,
            observed: true,
            contents: None,
        }
    }

    /// Records a declared target that the host did not need to read.
    ///
    /// Projection fails if it later attempts to use this target. This lets a
    /// host avoid unrelated I/O while retaining a complete target inventory.
    pub fn unobserved(target: LogicalTarget) -> Self {
        Self {
            target,
            observed: false,
            contents: None,
        }
    }

    /// Returns the logical target represented by this observation.
    pub fn target(&self) -> LogicalTarget {
        self.target
    }

    /// Returns observed bytes, or `None` when missing or unobserved.
    pub fn contents(&self) -> Option<&[u8]> {
        self.contents.as_deref()
    }

    /// Returns whether the host actually checked this target.
    pub fn is_observed(&self) -> bool {
        self.observed
    }
}

impl fmt::Debug for ObservedDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("ObservedDocument");
        debug.field("target", &self.target);
        match (self.observed, &self.contents) {
            (false, _) => {
                debug.field("contents", &"<unobserved>");
            }
            (true, Some(contents)) => {
                debug
                    .field("contents", &"<redacted>")
                    .field("content_bytes", &contents.len());
            }
            (true, None) => {
                debug.field("contents", &"<missing>");
            }
        }
        debug.finish()
    }
}

/// A complete, app-scoped snapshot of every target declared by an adapter.
///
/// Construction rejects omitted targets so shared projection code cannot
/// confuse "the host did not observe this target" with "the target was
/// observed and did not exist". Documents are stored in adapter target order.
#[derive(Clone, PartialEq, Eq)]
pub struct LiveDocumentSet {
    app: AppType,
    documents: Vec<ObservedDocument>,
}

impl LiveDocumentSet {
    /// Builds and validates a complete snapshot for one built-in application.
    pub fn try_new(
        app: AppType,
        documents: impl IntoIterator<Item = ObservedDocument>,
    ) -> Result<Self, LiveDocumentSetError> {
        Self::try_new_with_content_limit(app, documents, MAX_OPERATION_CONTENT_BYTES)
    }

    /// Builds a local snapshot under an explicit host-selected per-document bound.
    /// Default snapshots and serialized operation plans retain their fixed limits.
    pub fn try_new_with_content_limit(
        app: AppType,
        documents: impl IntoIterator<Item = ObservedDocument>,
        maximum_content_bytes: usize,
    ) -> Result<Self, LiveDocumentSetError> {
        let targets = builtin_app_adapter(&app).targets();
        let mut supplied = HashMap::with_capacity(targets.len());

        for document in documents {
            let target = document.target();
            let actual = target.app();
            if actual != app {
                return Err(LiveDocumentSetError::WrongApp {
                    target,
                    expected: app.as_str().to_owned(),
                    actual: actual.as_str().to_owned(),
                });
            }
            if !targets.contains(&target) {
                return Err(LiveDocumentSetError::UndeclaredTarget { target });
            }
            if document
                .contents()
                .is_some_and(|contents| contents.len() > maximum_content_bytes)
            {
                return Err(LiveDocumentSetError::ContentTooLarge {
                    target,
                    limit: maximum_content_bytes,
                });
            }
            if supplied.insert(target, document).is_some() {
                return Err(LiveDocumentSetError::DuplicateTarget { target });
            }
        }

        let mut ordered = Vec::with_capacity(targets.len());
        for target in targets {
            let document = supplied
                .remove(target)
                .ok_or(LiveDocumentSetError::MissingTarget { target: *target })?;
            ordered.push(document);
        }

        Ok(Self {
            app,
            documents: ordered,
        })
    }

    /// Returns the application that owns this snapshot.
    pub fn app(&self) -> &AppType {
        &self.app
    }

    /// Returns observations in the adapter's stable target order.
    pub fn documents(
        &self,
    ) -> impl ExactSizeIterator<Item = &ObservedDocument> + DoubleEndedIterator + Clone {
        self.documents.iter()
    }

    /// Returns the observation for a target declared by this snapshot's app.
    pub fn document(&self, target: LogicalTarget) -> Option<&ObservedDocument> {
        self.documents
            .iter()
            .find(|document| document.target() == target)
    }
}

impl fmt::Debug for LiveDocumentSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveDocumentSet")
            .field("app", &self.app)
            .field("documents", &self.documents)
            .finish()
    }
}

/// Rejection reason for a host-supplied live-document snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum LiveDocumentSetError {
    #[error("logical target {target:?} belongs to '{actual}', expected '{expected}'")]
    WrongApp {
        target: LogicalTarget,
        expected: String,
        actual: String,
    },
    #[error("logical target {target:?} is not declared by the adapter")]
    UndeclaredTarget { target: LogicalTarget },
    #[error("logical target {target:?} appears more than once")]
    DuplicateTarget { target: LogicalTarget },
    #[error("logical target {target:?} was not observed by the host")]
    MissingTarget { target: LogicalTarget },
    #[error("logical target {target:?} exceeds the {limit}-byte input limit")]
    ContentTooLarge { target: LogicalTarget, limit: usize },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{builtin_app_adapters, AppAdapter};

    fn missing_documents(adapter: &dyn AppAdapter) -> Vec<ObservedDocument> {
        adapter
            .targets()
            .iter()
            .copied()
            .map(ObservedDocument::missing)
            .collect()
    }

    #[test]
    fn every_adapter_accepts_a_complete_missing_snapshot() {
        for adapter in builtin_app_adapters() {
            let app = adapter.descriptor().app().clone();
            let snapshot = LiveDocumentSet::try_new(app.clone(), missing_documents(adapter))
                .expect("complete adapter snapshot");

            assert_eq!(snapshot.app(), &app);
            assert_eq!(
                snapshot
                    .documents()
                    .map(ObservedDocument::target)
                    .collect::<Vec<_>>(),
                adapter.targets()
            );
            assert!(snapshot
                .documents()
                .all(|document| document.contents().is_none()));
        }
    }

    #[test]
    fn construction_restores_adapter_target_order() {
        let adapter = builtin_app_adapter(&AppType::Codex);
        let mut documents = missing_documents(adapter);
        documents.reverse();

        let snapshot =
            LiveDocumentSet::try_new(AppType::Codex, documents).expect("complete Codex snapshot");

        assert_eq!(
            snapshot
                .documents()
                .map(ObservedDocument::target)
                .collect::<Vec<_>>(),
            adapter.targets()
        );
    }

    #[test]
    fn distinguishes_missing_targets_from_omitted_observations() {
        let error = LiveDocumentSet::try_new(
            AppType::Gemini,
            [ObservedDocument::missing(LogicalTarget::GeminiEnv)],
        )
        .expect_err("Gemini settings observation is required");

        assert_eq!(
            error,
            LiveDocumentSetError::MissingTarget {
                target: LogicalTarget::GeminiSettings
            }
        );
    }

    #[test]
    fn keeps_unobserved_targets_distinct_from_missing_targets() {
        let snapshot = LiveDocumentSet::try_new(
            AppType::Claude,
            [ObservedDocument::unobserved(LogicalTarget::ClaudeSettings)],
        )
        .expect("complete target inventory");
        let document = snapshot
            .document(LogicalTarget::ClaudeSettings)
            .expect("declared target");

        assert!(!document.is_observed());
        assert!(document.contents().is_none());
        assert!(format!("{document:?}").contains("<unobserved>"));
    }

    #[test]
    fn rejects_duplicate_and_cross_app_targets() {
        let duplicate = LiveDocumentSet::try_new(
            AppType::Claude,
            [
                ObservedDocument::missing(LogicalTarget::ClaudeSettings),
                ObservedDocument::missing(LogicalTarget::ClaudeSettings),
            ],
        )
        .expect_err("duplicate observation");
        assert_eq!(
            duplicate,
            LiveDocumentSetError::DuplicateTarget {
                target: LogicalTarget::ClaudeSettings
            }
        );

        let cross_app = LiveDocumentSet::try_new(
            AppType::Claude,
            [ObservedDocument::missing(LogicalTarget::CodexConfig)],
        )
        .expect_err("cross-app observation");
        assert_eq!(
            cross_app,
            LiveDocumentSetError::WrongApp {
                target: LogicalTarget::CodexConfig,
                expected: "claude".to_owned(),
                actual: "codex".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_oversized_documents_before_projection() {
        let error = LiveDocumentSet::try_new(
            AppType::Claude,
            [ObservedDocument::present(
                LogicalTarget::ClaudeSettings,
                vec![b'x'; MAX_OPERATION_CONTENT_BYTES + 1],
            )],
        )
        .expect_err("oversized observation");

        assert_eq!(
            error,
            LiveDocumentSetError::ContentTooLarge {
                target: LogicalTarget::ClaudeSettings,
                limit: MAX_OPERATION_CONTENT_BYTES,
            }
        );
    }

    #[test]
    fn debug_output_never_exposes_document_contents() {
        let secret = "do-not-log-live-credential";
        let snapshot = LiveDocumentSet::try_new(
            AppType::Claude,
            [ObservedDocument::present(
                LogicalTarget::ClaudeSettings,
                secret.as_bytes(),
            )],
        )
        .expect("complete Claude snapshot");

        let debug = format!("{snapshot:?}");
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains(&secret.len().to_string()));
        assert!(!debug.contains(secret));
    }
}
