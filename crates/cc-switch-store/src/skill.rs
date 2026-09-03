use cc_switch_core::{skill_catalog_columns, SkillCatalogEntry, SkillCatalogEntryError};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior};

use crate::{ExistingColumn, SharedStoreError};

/// Canonical installed Skill catalog shared by CC Switch products.
pub const SKILLS_TABLE: &str = "skills";

const CREATE_SKILLS_TABLE: &str = "CREATE TABLE IF NOT EXISTS main.skills (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    directory TEXT NOT NULL,
    repo_owner TEXT,
    repo_name TEXT,
    repo_branch TEXT DEFAULT 'main',
    readme_url TEXT,
    installed_at INTEGER NOT NULL DEFAULT 0,
    content_hash TEXT,
    updated_at INTEGER NOT NULL DEFAULT 0
)";

const BASE_SKILL_COLUMNS: &[SkillColumn] = &[
    SkillColumn::new("id", "TEXT", false, None, 1),
    SkillColumn::new("name", "TEXT", true, None, 0),
    SkillColumn::new("description", "TEXT", false, None, 0),
    SkillColumn::new("directory", "TEXT", true, None, 0),
];

#[derive(Clone, Copy)]
struct SkillColumn {
    name: &'static str,
    declared_type: &'static str,
    not_null: bool,
    default: Option<&'static str>,
    primary_key: i64,
}

impl SkillColumn {
    const fn new(
        name: &'static str,
        declared_type: &'static str,
        not_null: bool,
        default: Option<&'static str>,
        primary_key: i64,
    ) -> Self {
        Self {
            name,
            declared_type,
            not_null,
            default,
            primary_key,
        }
    }
}

struct RawSkill {
    id: String,
    name: String,
    description: Option<String>,
    directory: String,
    selections: Vec<i64>,
}

/// Creates or transactionally upgrades the shared installed Skill catalog.
///
/// Core-declared selection columns are added in registry order. Existing rows,
/// product metadata columns, unknown columns, indexes, and triggers are kept.
pub fn ensure_skill_schema(connection: &mut Connection) -> Result<(), SharedStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(CREATE_SKILLS_TABLE, [])?;

    let initial_columns = skill_columns(&transaction)?;
    verify_base_skill_schema(&transaction, &initial_columns)?;
    for selection in skill_catalog_columns() {
        if !initial_columns
            .iter()
            .any(|column| column.name == selection.as_str())
        {
            transaction.execute_batch(&format!(
                "ALTER TABLE main.skills ADD COLUMN \"{}\" BOOLEAN NOT NULL DEFAULT 0",
                selection.as_str()
            ))?;
        }
    }
    verify_skill_schema(&transaction)?;
    transaction.commit()?;
    Ok(())
}

/// Reads the complete Core-owned Skill selection view in stable display order.
pub fn read_skill_catalog(
    connection: &Connection,
) -> Result<Vec<SkillCatalogEntry>, SharedStoreError> {
    let selections = skill_catalog_columns().collect::<Vec<_>>();
    let selection_sql = selections
        .iter()
        .map(|column| format!("\"{}\"", column.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id, name, description, directory, {selection_sql}
         FROM main.skills ORDER BY name COLLATE BINARY, id COLLATE BINARY"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], |row| raw_skill(row, selections.len()))?;
    rows.map(|row| {
        let row = row?;
        let selected_values = row
            .selections
            .into_iter()
            .map(|selected| match selected {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(SharedStoreError::InvalidDatabase(
                    "skills selection must be 0 or 1".to_owned(),
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        SkillCatalogEntry::try_new(
            row.id,
            row.name,
            row.description,
            row.directory,
            selections.iter().copied().zip(selected_values),
        )
        .map_err(invalid_skill_row)
    })
    .collect()
}

fn raw_skill(row: &Row<'_>, selection_count: usize) -> rusqlite::Result<RawSkill> {
    let selections = (0..selection_count)
        .map(|offset| row.get(4 + offset))
        .collect::<Result<Vec<i64>, _>>()?;
    Ok(RawSkill {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        directory: row.get(3)?,
        selections,
    })
}

fn invalid_skill_row(error: SkillCatalogEntryError) -> SharedStoreError {
    SharedStoreError::InvalidDatabase(format!(
        "skills row does not match the shared contract: {error}"
    ))
}

fn skill_columns(connection: &Connection) -> Result<Vec<ExistingColumn>, SharedStoreError> {
    let object_type = connection
        .query_row(
            "SELECT type FROM main.sqlite_schema WHERE name = 'skills'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if object_type.as_deref() != Some("table") {
        return Err(SharedStoreError::InvalidDatabase(
            "skills must be a table".to_owned(),
        ));
    }

    let mut statement = connection.prepare("PRAGMA main.table_xinfo(skills)")?;
    let rows = statement.query_map([], |row| {
        Ok(ExistingColumn {
            name: row.get(1)?,
            declared_type: row.get(2)?,
            not_null: row.get::<_, i64>(3)? != 0,
            default: row.get(4)?,
            primary_key: row.get(5)?,
            hidden: row.get(6)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(SharedStoreError::from)
}

fn verify_base_skill_schema(
    connection: &Connection,
    columns: &[ExistingColumn],
) -> Result<(), SharedStoreError> {
    for expected in BASE_SKILL_COLUMNS {
        verify_skill_column(columns, expected)?;
    }
    verify_skill_primary_key(connection, columns)
}

fn verify_skill_schema(connection: &Connection) -> Result<(), SharedStoreError> {
    let columns = skill_columns(connection)?;
    verify_base_skill_schema(connection, &columns)?;
    for selection in skill_catalog_columns() {
        verify_skill_column(
            &columns,
            &SkillColumn::new(selection.as_str(), "BOOLEAN", true, Some("0"), 0),
        )?;
    }
    Ok(())
}

fn verify_skill_column(
    columns: &[ExistingColumn],
    expected: &SkillColumn,
) -> Result<(), SharedStoreError> {
    let actual = columns
        .iter()
        .find(|column| column.name == expected.name)
        .ok_or_else(|| {
            SharedStoreError::InvalidDatabase(format!(
                "skills table is missing required column '{}'",
                expected.name
            ))
        })?;
    let default_matches = match (&actual.default, expected.default) {
        (None, None) => true,
        (Some(actual), Some(expected)) => actual.trim_matches(['(', ')']) == expected,
        _ => false,
    };
    if actual.hidden != 0
        || !actual
            .declared_type
            .eq_ignore_ascii_case(expected.declared_type)
        || actual.not_null != expected.not_null
        || actual.primary_key != expected.primary_key
        || !default_matches
    {
        return Err(SharedStoreError::InvalidDatabase(format!(
            "skills column '{}' does not match the shared contract",
            expected.name
        )));
    }
    Ok(())
}

fn verify_skill_primary_key(
    connection: &Connection,
    columns: &[ExistingColumn],
) -> Result<(), SharedStoreError> {
    let primary_key = columns
        .iter()
        .filter(|column| column.primary_key > 0)
        .collect::<Vec<_>>();
    if primary_key.len() != 1 || primary_key[0].name != "id" || primary_key[0].primary_key != 1 {
        return Err(SharedStoreError::InvalidDatabase(
            "skills primary key must be exactly (id)".to_owned(),
        ));
    }

    let primary_key_index = connection
        .query_row(
            "SELECT name FROM pragma_index_list('skills', 'main')
             WHERE origin = 'pk' AND partial = 0",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            SharedStoreError::InvalidDatabase(
                "skills table has no canonical primary key index".to_owned(),
            )
        })?;
    let mut statement = connection.prepare(
        "SELECT name, \"desc\", coll FROM pragma_index_xinfo(?1, 'main')
         WHERE key = 1 ORDER BY seqno",
    )?;
    let indexed_columns = statement
        .query_map([primary_key_index], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let binary_id = indexed_columns.as_slice();
    if binary_id.len() != 1
        || binary_id[0].0.as_deref() != Some("id")
        || binary_id[0].1 != 0
        || !binary_id[0].2.eq_ignore_ascii_case("BINARY")
    {
        return Err(SharedStoreError::InvalidDatabase(
            "skills primary key must use binary id ordering".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SharedDatabase;

    fn test_database() -> (tempfile::TempDir, SharedDatabase) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        (directory, database)
    }

    #[test]
    fn creates_registry_catalog_with_compatible_product_metadata_columns() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let connection = database.connect().expect("connect shared database");
        let columns = skill_columns(&connection).expect("read skill columns");

        for required in [
            "repo_owner",
            "repo_name",
            "repo_branch",
            "readme_url",
            "installed_at",
            "content_hash",
            "updated_at",
        ] {
            assert!(columns.iter().any(|column| column.name == required));
        }
        for selection in skill_catalog_columns() {
            assert!(columns
                .iter()
                .any(|column| column.name == selection.as_str()));
        }
        assert!(read_skill_catalog(&connection)
            .expect("read empty catalog")
            .is_empty());
    }

    #[test]
    fn upgrades_selection_columns_and_preserves_host_extensions() {
        let (_directory, database) = test_database();
        let connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "CREATE TABLE skills (
                    id TEXT COLLATE binary PRIMARY KEY,
                    name TEXT NOT NULL,
                    description TEXT,
                    directory TEXT NOT NULL,
                    enabled_claude BOOLEAN NOT NULL DEFAULT 0,
                    host_note TEXT
                 );
                 CREATE INDEX host_skill_name ON skills(host_note);
                 CREATE TABLE host_skill_events (id TEXT);
                 CREATE TRIGGER host_skill_insert AFTER INSERT ON skills BEGIN
                    INSERT INTO host_skill_events VALUES (NEW.id);
                 END;
                 INSERT INTO skills (id, name, description, directory, host_note)
                 VALUES ('z', 'Same', NULL, 'z-dir', 'keep'),
                        ('A', 'Same', 'first', 'a-dir', 'keep');",
            )
            .expect("create compatible host schema");

        database.ensure_skill_schema().expect("upgrade skills");
        let connection = database.connect().expect("reconnect shared database");
        let catalog = read_skill_catalog(&connection).expect("read upgraded catalog");

        assert_eq!(
            catalog.iter().map(|entry| entry.id()).collect::<Vec<_>>(),
            ["A", "z"]
        );
        assert!(catalog
            .iter()
            .all(|entry| entry.selections().all(|(_, selected)| !selected)));
        assert_eq!(
            connection
                .query_row("SELECT host_note FROM skills WHERE id = 'A'", [], |row| {
                    row.get::<_, String>(0)
                })
                .expect("read host column"),
            "keep"
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM host_skill_events", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("read host trigger data"),
            2
        );
        assert!(connection
            .query_row(
                "SELECT 1 FROM main.sqlite_schema
                 WHERE type = 'index' AND name = 'host_skill_name'",
                [],
                |_| Ok(()),
            )
            .is_ok());
        connection
            .execute(
                "INSERT INTO skills (id, name, directory) VALUES ('after', 'After', 'after')",
                [],
            )
            .expect("insert through preserved trigger");
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM host_skill_events", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("read host trigger data"),
            3
        );
    }

    #[test]
    fn rejects_incompatible_identity_before_partial_upgrade() {
        let (_directory, database) = test_database();
        let connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "CREATE TABLE skills (
                    id TEXT PRIMARY KEY COLLATE NOCASE,
                    name TEXT NOT NULL,
                    description TEXT,
                    directory TEXT NOT NULL
                 );",
            )
            .expect("create incompatible schema");

        let error = database
            .ensure_skill_schema()
            .expect_err("reject incompatible identity");
        assert!(matches!(
            error,
            SharedStoreError::InvalidDatabase(message)
                if message == "skills primary key must use binary id ordering"
        ));
        let connection = database.connect().expect("reconnect shared database");
        let columns = skill_columns(&connection).expect("read unchanged columns");
        assert_eq!(columns.len(), 4);
    }

    #[test]
    fn rejects_primary_key_with_additional_key_terms() {
        let (_directory, database) = test_database();
        let connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "CREATE TABLE skills (
                    id TEXT,
                    name TEXT NOT NULL,
                    description TEXT,
                    directory TEXT NOT NULL,
                    PRIMARY KEY(id COLLATE BINARY, id COLLATE NOCASE)
                 );",
            )
            .expect("create incompatible schema");

        let error = database
            .ensure_skill_schema()
            .expect_err("reject additional primary-key term");
        assert!(matches!(
            error,
            SharedStoreError::InvalidDatabase(message)
                if message == "skills primary key must use binary id ordering"
        ));
        let connection = database.connect().expect("reconnect shared database");
        let columns = skill_columns(&connection).expect("read unchanged columns");
        assert_eq!(columns.len(), 4);
    }

    #[test]
    fn failed_final_validation_rolls_back_added_columns() {
        let (_directory, database) = test_database();
        let connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "CREATE TABLE skills (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    description TEXT,
                    directory TEXT NOT NULL,
                    enabled_pi TEXT NOT NULL DEFAULT 0
                 );",
            )
            .expect("create incompatible schema");

        database
            .ensure_skill_schema()
            .expect_err("reject incompatible selection");
        let connection = database.connect().expect("reconnect shared database");
        let columns = skill_columns(&connection).expect("read unchanged columns");
        assert_eq!(columns.len(), 5);
        assert!(columns.iter().any(|column| column.name == "enabled_pi"));
    }

    #[test]
    fn rejects_invalid_catalog_rows() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let connection = database.connect().expect("connect shared database");
        connection
            .execute(
                "INSERT INTO skills (id, name, directory) VALUES ('demo', ' Demo ', 'demo')",
                [],
            )
            .expect("insert invalid row");

        let error = read_skill_catalog(&connection).expect_err("reject invalid row");
        assert!(matches!(error, SharedStoreError::InvalidDatabase(_)));
    }

    #[test]
    fn rejects_non_boolean_selection_values() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let connection = database.connect().expect("connect shared database");
        connection
            .execute(
                "INSERT INTO skills (id, name, directory, enabled_claude)
                 VALUES ('demo', 'Demo', 'demo', 2)",
                [],
            )
            .expect("insert invalid selection");

        let error = read_skill_catalog(&connection).expect_err("reject invalid selection");
        assert!(matches!(
            error,
            SharedStoreError::InvalidDatabase(message)
                if message == "skills selection must be 0 or 1"
        ));
    }
}
