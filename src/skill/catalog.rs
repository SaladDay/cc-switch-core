use std::{collections::HashMap, path::Path};

use thiserror::Error;

use crate::{builtin_app_registry, AppType, SkillCatalogColumn};

const MAX_SKILL_ID_BYTES: usize = 1024;
const MAX_SKILL_NAME_BYTES: usize = 256;
const MAX_SKILL_DIRECTORY_BYTES: usize = 255;

/// One installed Skill loaded from the shared `skills` table.
///
/// Repository and update metadata remain host-owned because switching does not
/// consume them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCatalogEntry {
    id: String,
    name: String,
    description: Option<String>,
    directory: String,
    selections: Vec<(SkillCatalogColumn, bool)>,
}

impl SkillCatalogEntry {
    /// Builds one complete row using the columns declared by the Core registry.
    pub fn try_new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: Option<String>,
        directory: impl Into<String>,
        selections: impl IntoIterator<Item = (SkillCatalogColumn, bool)>,
    ) -> Result<Self, SkillCatalogEntryError> {
        let id = id.into();
        validate_text("id", &id, MAX_SKILL_ID_BYTES)?;
        let name = name.into();
        validate_text("name", &name, MAX_SKILL_NAME_BYTES)?;
        let directory = directory.into();
        validate_directory(&directory)?;

        let mut supplied = HashMap::new();
        for (column, selected) in selections {
            if supplied.insert(column, selected).is_some() {
                return Err(SkillCatalogEntryError::DuplicateColumn {
                    column: column.as_str(),
                });
            }
        }

        let mut ordered = Vec::new();
        for column in skill_catalog_columns() {
            let selected =
                supplied
                    .remove(&column)
                    .ok_or(SkillCatalogEntryError::MissingColumn {
                        column: column.as_str(),
                    })?;
            ordered.push((column, selected));
        }
        debug_assert!(supplied.is_empty(), "all columns originate in the registry");

        Ok(Self {
            id,
            name,
            description,
            directory,
            selections: ordered,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn directory(&self) -> &str {
        &self.directory
    }

    /// Returns the requested catalog selection for an application.
    ///
    /// `None` means that the application stores selection outside the catalog.
    pub fn selected_for(&self, app: &AppType) -> Option<bool> {
        let column = builtin_app_registry()
            .for_app(app)
            .skill_contract()?
            .selection_store()
            .catalog_column()?;
        self.selections
            .iter()
            .find_map(|(candidate, selected)| (*candidate == column).then_some(*selected))
    }

    pub fn selections(
        &self,
    ) -> impl ExactSizeIterator<Item = (SkillCatalogColumn, bool)> + DoubleEndedIterator + Clone + '_
    {
        self.selections.iter().copied()
    }
}

/// Returns every shared `skills.enabled_*` column in registry order.
///
/// Database adapters can use this to pair query results with Core-owned
/// identifiers without duplicating the App-to-column mapping.
pub fn skill_catalog_columns() -> impl Iterator<Item = SkillCatalogColumn> + Clone {
    builtin_app_registry()
        .descriptors()
        .filter_map(|descriptor| {
            descriptor
                .skill_contract()?
                .selection_store()
                .catalog_column()
        })
}

fn validate_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), SkillCatalogEntryError> {
    if value.is_empty()
        || value.trim() != value
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
    {
        return Err(SkillCatalogEntryError::InvalidText { field });
    }
    Ok(())
}

fn validate_directory(directory: &str) -> Result<(), SkillCatalogEntryError> {
    let mut components = Path::new(directory).components();
    let valid_component = matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(name)), None) if name.to_str() == Some(directory)
    );
    if !valid_component
        || directory.trim() != directory
        || directory.starts_with('.')
        || directory.contains(['/', '\\'])
        || directory.len() > MAX_SKILL_DIRECTORY_BYTES
        || directory.chars().any(char::is_control)
    {
        return Err(SkillCatalogEntryError::InvalidDirectory {
            directory: directory.to_owned(),
        });
    }
    Ok(())
}

/// A malformed shared-catalog row rejected before any path is joined.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SkillCatalogEntryError {
    #[error("invalid Skill catalog field: {field}")]
    InvalidText { field: &'static str },
    #[error("invalid Skill directory: {directory:?}")]
    InvalidDirectory { directory: String },
    #[error("duplicate Skill catalog column: {column}")]
    DuplicateColumn { column: &'static str },
    #[error("missing Skill catalog column: {column}")]
    MissingColumn { column: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selections() -> Vec<(SkillCatalogColumn, bool)> {
        skill_catalog_columns()
            .enumerate()
            .map(|(index, column)| (column, index % 2 == 0))
            .collect()
    }

    #[test]
    fn catalog_entries_follow_registry_columns_in_stable_order() {
        let entry = SkillCatalogEntry::try_new(
            "owner/repo:demo",
            "Demo",
            Some("Description".to_owned()),
            "demo",
            selections().into_iter().rev(),
        )
        .expect("valid catalog row");

        let actual = entry
            .selections()
            .map(|(column, selected)| (column.as_str(), selected))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            [
                ("enabled_claude", true),
                ("enabled_codex", false),
                ("enabled_gemini", true),
                ("enabled_grokbuild", false),
                ("enabled_opencode", true),
                ("enabled_hermes", false),
            ]
        );
        assert_eq!(entry.selected_for(&AppType::Claude), Some(true));
        assert_eq!(entry.selected_for(&AppType::Pi), None);
    }

    #[test]
    fn catalog_entries_require_every_column_exactly_once() {
        let mut missing = selections();
        missing.pop();
        assert!(matches!(
            SkillCatalogEntry::try_new("id", "name", None, "demo", missing),
            Err(SkillCatalogEntryError::MissingColumn { .. })
        ));

        let mut duplicate = selections();
        duplicate.push(duplicate[0]);
        assert!(matches!(
            SkillCatalogEntry::try_new("id", "name", None, "demo", duplicate),
            Err(SkillCatalogEntryError::DuplicateColumn { .. })
        ));
    }

    #[test]
    fn unsafe_directories_are_rejected_before_path_use() {
        for directory in ["", ".hidden", "..", "../escape", "a/b", "a\\b", " demo"] {
            assert!(matches!(
                SkillCatalogEntry::try_new("id", "name", None, directory, selections()),
                Err(SkillCatalogEntryError::InvalidDirectory { .. })
            ));
        }
    }
}
