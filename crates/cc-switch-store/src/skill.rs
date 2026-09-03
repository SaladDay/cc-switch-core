use std::sync::atomic::{AtomicU64, Ordering};

use cc_switch_core::{
    skill_catalog_columns, SkillCatalogColumn, SkillCatalogEntry, SkillCatalogEntryError,
    SkillSwitchPlan,
};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};

use crate::{source_fingerprint, ExistingColumn, SharedStoreError};

/// Canonical installed Skill catalog shared by CC Switch products.
pub const SKILLS_TABLE: &str = "skills";

static NEXT_SKILL_WRITE_GUARD: AtomicU64 = AtomicU64::new(0);

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
    host_fingerprint: [u8; 32],
}

struct StoredSkill {
    entry: SkillCatalogEntry,
    host_fingerprint: [u8; 32],
}

struct SkillProjection {
    selections: Vec<SkillCatalogColumn>,
    select_list: String,
    host_offset: usize,
}

struct SkillWriteGuard {
    table: String,
    triggers: Vec<String>,
}

impl SkillWriteGuard {
    fn install(connection: &Connection) -> rusqlite::Result<Self> {
        let id = NEXT_SKILL_WRITE_GUARD.fetch_add(1, Ordering::Relaxed);
        let table = format!("cc_switch_skill_write_guard_{id}");
        let mut triggers = Vec::new();
        connection.execute_batch(&format!(
            "CREATE TEMP TABLE \"{}\" (
                updates INTEGER NOT NULL,
                violations INTEGER NOT NULL
             );
             INSERT INTO \"{}\" VALUES (0, 0);",
            table, table,
        ))?;

        let update_trigger = format!("cc_switch_skill_update_guard_{id}");
        connection.execute_batch(&format!(
            "CREATE TEMP TRIGGER \"{}\" BEFORE UPDATE ON main.skills BEGIN
                UPDATE \"{}\"
                   SET violations = violations + (updates != 0),
                       updates = updates + 1;
                SELECT CASE WHEN (SELECT updates FROM \"{}\") > 1
                    THEN RAISE(IGNORE) END;
             END;",
            update_trigger, table, table,
        ))?;
        triggers.push(update_trigger);

        for operation in ["INSERT", "DELETE"] {
            install_blocking_skill_trigger(
                connection,
                &table,
                &mut triggers,
                id,
                SKILLS_TABLE,
                operation,
            )?;
        }
        for related in skill_related_tables(connection)? {
            for operation in ["UPDATE", "INSERT", "DELETE"] {
                install_blocking_skill_trigger(
                    connection,
                    &table,
                    &mut triggers,
                    id,
                    &related,
                    operation,
                )?;
            }
        }
        Ok(Self { table, triggers })
    }

    fn accepted(&self, connection: &Connection) -> rusqlite::Result<bool> {
        connection.query_row(
            &format!(
                "SELECT updates = 1 AND violations = 0 FROM temp.\"{}\"",
                self.table
            ),
            [],
            |row| row.get(0),
        )
    }

    fn remove(self, connection: &Connection) -> rusqlite::Result<()> {
        for trigger in self.triggers.into_iter().rev() {
            connection.execute_batch(&format!("DROP TRIGGER temp.\"{trigger}\";"))?;
        }
        connection.execute_batch(&format!("DROP TABLE temp.\"{}\";", self.table))
    }
}

fn install_blocking_skill_trigger(
    connection: &Connection,
    guard_table: &str,
    triggers: &mut Vec<String>,
    guard_id: u64,
    target_table: &str,
    operation: &str,
) -> rusqlite::Result<()> {
    let trigger = format!("cc_switch_skill_block_{guard_id}_{}", triggers.len());
    connection.execute_batch(&format!(
        "CREATE TEMP TRIGGER \"{trigger}\" BEFORE {operation} ON main.{} BEGIN
            UPDATE \"{guard_table}\" SET violations = violations + 1;
            SELECT RAISE(IGNORE);
         END;",
        quoted_identifier(target_table),
    ))?;
    triggers.push(trigger);
    Ok(())
}

fn skill_related_tables(connection: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut tables = connection.prepare(
        "SELECT name FROM main.sqlite_schema
         WHERE type = 'table' ORDER BY name COLLATE BINARY",
    )?;
    let table_names = tables
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut relationships = Vec::new();
    for table_name in &table_names {
        let mut foreign_keys =
            connection.prepare("SELECT \"table\" FROM pragma_foreign_key_list(?1, 'main')")?;
        for parent in foreign_keys.query_map([table_name], |row| row.get::<_, String>(0))? {
            let parent = parent?;
            if let Some(parent) = table_names
                .iter()
                .find(|candidate| candidate.eq_ignore_ascii_case(&parent))
            {
                relationships.push((table_name.clone(), parent.clone()));
            }
        }
    }
    let mut protected = vec![SKILLS_TABLE.to_owned()];
    loop {
        let mut added = false;
        for (child, parent) in &relationships {
            let child_protected = protected
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(child));
            let parent_protected = protected
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(parent));
            if parent_protected && !child_protected {
                protected.push(child.clone());
                added = true;
            } else if child_protected && !parent_protected {
                protected.push(parent.clone());
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    protected.remove(0);
    Ok(protected)
}

fn quoted_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Result of applying the catalog part of a Core Skill switch plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum SkillCatalogWriteOutcome {
    /// The plan's catalog guard matched and its optional change was accepted.
    Applied,
    /// The guarded row was absent, stale, suppressed, or rewritten.
    NotApplied,
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
    let projection = core_skill_projection();
    Ok(read_skill_rows(connection, &projection)?
        .into_iter()
        .map(|skill| skill.entry)
        .collect())
}

/// Reads one Core-owned Skill selection row by its binary identifier.
pub fn read_skill_catalog_entry(
    connection: &Connection,
    id: &str,
) -> Result<Option<SkillCatalogEntry>, SharedStoreError> {
    let projection = core_skill_projection();
    let sql = format!(
        "SELECT {} FROM main.skills AS skills WHERE id COLLATE BINARY = ?1",
        projection.select_list
    );
    connection
        .query_row(&sql, [id], |row| raw_skill(row, &projection))
        .optional()?
        .map(|row| skill_from_raw(row, &projection.selections).map(|skill| skill.entry))
        .transpose()
}

/// Applies only the shared-catalog part of a Core Skill switch plan.
///
/// The caller owns the surrounding transaction and all live file work. This
/// function changes at most one registry-declared selection column. Product
/// metadata and unknown host columns are never assigned.
pub fn apply_skill_catalog_plan(
    transaction: &mut Transaction<'_>,
    plan: &SkillSwitchPlan,
) -> Result<SkillCatalogWriteOutcome, SharedStoreError> {
    prepare_skill_catalog_write(transaction)?;
    let before = read_skill_catalog_state(transaction).map_err(|error| {
        redact_skill_catalog_post_write_error(error, transaction.is_autocommit())
    })?;
    let guard = plan.catalog_guard();
    let Some(current) = before
        .iter()
        .find(|skill| skill.entry.id() == guard.skill_id())
    else {
        return Ok(SkillCatalogWriteOutcome::NotApplied);
    };
    if !guard.matches(&current.entry) {
        return Ok(SkillCatalogWriteOutcome::NotApplied);
    }
    let Some(change) = plan.catalog_change() else {
        return Ok(SkillCatalogWriteOutcome::Applied);
    };
    verify_skill_foreign_key_write_contract(transaction, change.column())
        .map_err(|error| redact_skill_catalog_store_error(error, transaction.is_autocommit()))?;

    let sql = format!(
        "UPDATE main.skills SET \"{}\" = ?1
         WHERE id COLLATE BINARY = ?2
           AND name COLLATE BINARY = ?3
           AND directory COLLATE BINARY = ?4
           AND \"{}\" = ?5",
        change.column().as_str(),
        change.column().as_str()
    );
    execute_skill_catalog_write(
        transaction,
        &sql,
        params![
            change.replacement(),
            change.skill_id(),
            guard.expected_name(),
            guard.expected_directory(),
            change.expected(),
        ],
        |connection| {
            let after = read_skill_catalog_state(connection)?;
            Ok(skill_catalog_matches_change(
                &before,
                &after,
                change.skill_id(),
                change.column(),
                change.replacement(),
            ))
        },
    )
}

fn read_skill_catalog_state(connection: &Connection) -> Result<Vec<StoredSkill>, SharedStoreError> {
    let projection = skill_write_projection(connection)?;
    read_skill_rows(connection, &projection)
}

fn read_skill_rows(
    connection: &Connection,
    projection: &SkillProjection,
) -> Result<Vec<StoredSkill>, SharedStoreError> {
    let sql = format!(
        "SELECT {} FROM main.skills AS skills
         ORDER BY name COLLATE BINARY, id COLLATE BINARY",
        projection.select_list
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], |row| raw_skill(row, projection))?;
    rows.map(|row| skill_from_raw(row?, &projection.selections))
        .collect()
}

fn core_skill_projection() -> SkillProjection {
    let selections = skill_catalog_columns().collect::<Vec<_>>();
    let mut fields = vec![
        "id".to_owned(),
        "name".to_owned(),
        "description".to_owned(),
        "directory".to_owned(),
    ];
    fields.extend(
        selections
            .iter()
            .map(|column| quoted_skill_column(column.as_str())),
    );
    let host_offset = fields.len();
    SkillProjection {
        selections,
        select_list: fields.join(", "),
        host_offset,
    }
}

fn skill_write_projection(connection: &Connection) -> Result<SkillProjection, SharedStoreError> {
    let mut projection = core_skill_projection();
    let host_columns = skill_columns(connection)?
        .into_iter()
        .filter(|column| {
            !BASE_SKILL_COLUMNS
                .iter()
                .any(|base| base.name == column.name)
                && !projection
                    .selections
                    .iter()
                    .any(|selection| selection.as_str() == column.name)
        })
        .map(|column| quoted_skill_column(&column.name))
        .collect::<Vec<_>>();
    if !host_columns.is_empty() {
        projection.select_list.push_str(", ");
        projection.select_list.push_str(&host_columns.join(", "));
    }
    Ok(projection)
}

fn quoted_skill_column(column: &str) -> String {
    format!("skills.\"{}\"", column.replace('"', "\"\""))
}

fn raw_skill(row: &Row<'_>, projection: &SkillProjection) -> rusqlite::Result<RawSkill> {
    let selections = (0..projection.selections.len())
        .map(|offset| row.get(4 + offset))
        .collect::<Result<Vec<i64>, _>>()?;
    Ok(RawSkill {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        directory: row.get(3)?,
        selections,
        host_fingerprint: source_fingerprint(row, projection.host_offset)?,
    })
}

fn skill_from_raw(
    row: RawSkill,
    selections: &[SkillCatalogColumn],
) -> Result<StoredSkill, SharedStoreError> {
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
    let entry = SkillCatalogEntry::try_new(
        row.id,
        row.name,
        row.description,
        row.directory,
        selections.iter().copied().zip(selected_values),
    )
    .map_err(invalid_skill_row)?;
    Ok(StoredSkill {
        entry,
        host_fingerprint: row.host_fingerprint,
    })
}

fn skill_catalog_matches_change(
    before: &[StoredSkill],
    after: &[StoredSkill],
    changed_id: &str,
    changed_column: SkillCatalogColumn,
    replacement: bool,
) -> bool {
    before.len() == after.len()
        && before.iter().zip(after).all(|(before, after)| {
            before.host_fingerprint == after.host_fingerprint
                && before.entry.id() == after.entry.id()
                && before.entry.name() == after.entry.name()
                && before.entry.description() == after.entry.description()
                && before.entry.directory() == after.entry.directory()
                && before.entry.selections().zip(after.entry.selections()).all(
                    |((before_column, before_selected), (after_column, after_selected))| {
                        before_column == after_column
                            && after_selected
                                == if before.entry.id() == changed_id
                                    && before_column == changed_column
                                {
                                    replacement
                                } else {
                                    before_selected
                                }
                    },
                )
        })
}

fn prepare_skill_catalog_write(transaction: &Transaction<'_>) -> Result<(), SharedStoreError> {
    if transaction.is_autocommit() {
        return Err(SharedStoreError::SkillCatalogWrite {
            code: None,
            extended_code: None,
            transaction_aborted: true,
        });
    }
    verify_skill_schema(transaction)
        .map_err(|error| redact_skill_catalog_store_error(error, transaction.is_autocommit()))
}

fn verify_skill_foreign_key_write_contract(
    connection: &Connection,
    changed_column: SkillCatalogColumn,
) -> Result<(), SharedStoreError> {
    let mut tables = connection.prepare(
        "SELECT name FROM main.sqlite_schema
         WHERE type = 'table' ORDER BY name COLLATE BINARY",
    )?;
    let table_names = tables
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for table_name in table_names {
        let mut foreign_keys = connection.prepare(
            "SELECT \"table\", \"to\", on_update
             FROM pragma_foreign_key_list(?1, 'main')",
        )?;
        let rows = foreign_keys.query_map([table_name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (parent, parent_column, on_update) = row?;
            if parent.eq_ignore_ascii_case(SKILLS_TABLE)
                && parent_column
                    .as_deref()
                    .is_some_and(|column| column.eq_ignore_ascii_case(changed_column.as_str()))
                && !on_update.eq_ignore_ascii_case("NO ACTION")
                && !on_update.eq_ignore_ascii_case("RESTRICT")
            {
                return Err(SharedStoreError::InvalidDatabase(
                    "skills selection foreign keys must not update child rows".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn execute_skill_catalog_write<P, F>(
    transaction: &mut Transaction<'_>,
    sql: &str,
    params: P,
    postcondition: F,
) -> Result<SkillCatalogWriteOutcome, SharedStoreError>
where
    P: rusqlite::Params,
    F: FnOnce(&Connection) -> Result<bool, SharedStoreError>,
{
    let transaction_aborted = transaction.is_autocommit();
    let savepoint = transaction
        .savepoint()
        .map_err(|error| redact_skill_catalog_write_error(error, transaction_aborted))?;
    let result = (|| {
        let guard = SkillWriteGuard::install(&savepoint)?;
        let changed = {
            let mut statement = savepoint.prepare(sql)?;
            let mut rows = statement.query(params)?;
            while rows.next()?.is_some() {}
            savepoint.changes() as usize
        };
        let postcondition_matches =
            changed == 1 && guard.accepted(&savepoint)? && postcondition(&savepoint)?;
        guard.remove(&savepoint)?;
        Ok::<_, SharedStoreError>((changed, postcondition_matches))
    })();
    let (changed, postcondition_matches) = match result {
        Ok(result) => result,
        Err(error) => {
            let _ = savepoint.finish();
            return Err(redact_skill_catalog_post_write_error(
                error,
                transaction.is_autocommit(),
            ));
        }
    };
    if changed > 1 {
        savepoint.finish().map_err(|error| {
            redact_skill_catalog_write_error(error, transaction.is_autocommit())
        })?;
        return Err(SharedStoreError::InvalidDatabase(
            "Skill catalog write affected multiple rows".to_owned(),
        ));
    }
    if !postcondition_matches {
        savepoint.finish().map_err(|error| {
            redact_skill_catalog_write_error(error, transaction.is_autocommit())
        })?;
        return Ok(SkillCatalogWriteOutcome::NotApplied);
    }
    savepoint
        .commit()
        .map_err(|error| redact_skill_catalog_write_error(error, transaction.is_autocommit()))?;
    Ok(SkillCatalogWriteOutcome::Applied)
}

fn redact_skill_catalog_store_error(
    error: SharedStoreError,
    transaction_aborted: bool,
) -> SharedStoreError {
    match error {
        SharedStoreError::Database(error) => {
            redact_skill_catalog_write_error(error, transaction_aborted)
        }
        error => error,
    }
}

fn redact_skill_catalog_post_write_error(
    error: SharedStoreError,
    transaction_aborted: bool,
) -> SharedStoreError {
    match error {
        SharedStoreError::Database(error) => {
            redact_skill_catalog_write_error(error, transaction_aborted)
        }
        error @ SharedStoreError::SkillCatalogWrite { .. } => error,
        _ => SharedStoreError::SkillCatalogWrite {
            code: None,
            extended_code: None,
            transaction_aborted,
        },
    }
}

fn redact_skill_catalog_write_error(
    error: rusqlite::Error,
    transaction_aborted: bool,
) -> SharedStoreError {
    let extended_code = match &error {
        rusqlite::Error::SqliteFailure(error, _) => Some(error.extended_code),
        _ => None,
    };
    SharedStoreError::SkillCatalogWrite {
        code: error.sqlite_error_code(),
        extended_code,
        transaction_aborted,
    }
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
    use cc_switch_core::{
        prepare_skill_reconciliation, prepare_skill_switch, AppType, SkillAppRuntime, SkillRuntime,
    };
    use std::fs;

    fn test_database() -> (tempfile::TempDir, SharedDatabase) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        (directory, database)
    }

    fn insert_skill(connection: &Connection, id: &str, name: &str, directory: &str) {
        connection
            .execute(
                "INSERT INTO skills (id, name, directory) VALUES (?1, ?2, ?3)",
                params![id, name, directory],
            )
            .expect("insert skill");
    }

    fn skill_plan(
        catalog: &[SkillCatalogEntry],
        requested: Option<bool>,
    ) -> (tempfile::TempDir, SkillSwitchPlan) {
        let directory = tempfile::tempdir().expect("temporary Skill roots");
        let source = directory.path().join("source");
        let native = directory.path().join("native");
        let state = directory.path().join("state");
        fs::create_dir_all(source.join("demo")).expect("create source Skill");
        fs::create_dir_all(&native).expect("create native root");
        fs::create_dir_all(&state).expect("create state root");
        fs::write(source.join("demo/SKILL.md"), "# Demo\n").expect("write Skill manifest");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&state, fs::Permissions::from_mode(0o700))
                .expect("make state root private");
        }
        let runtime = SkillRuntime::try_new(
            &source,
            directory.path().join("unified"),
            [
                SkillAppRuntime::try_new(AppType::Claude, native, state, None)
                    .expect("build application runtime"),
            ],
        )
        .expect("build Skill runtime");
        let plan = match requested {
            Some(enabled) => {
                prepare_skill_switch(catalog, "demo", &runtime, &AppType::Claude, enabled)
                    .expect("prepare Skill switch")
            }
            None => prepare_skill_reconciliation(catalog, "demo", &runtime, &AppType::Claude)
                .expect("prepare Skill reconciliation"),
        };
        (directory, plan)
    }

    fn selected(entry: &SkillCatalogEntry, column: &str) -> bool {
        entry
            .selections()
            .find_map(|(candidate, selected)| (candidate.as_str() == column).then_some(selected))
            .expect("selection column")
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

    #[test]
    fn applies_core_catalog_plans_with_binary_compare_and_swap() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute("ALTER TABLE skills ADD COLUMN host_note TEXT", [])
            .expect("add host column");
        insert_skill(&connection, "demo", "Demo", "demo");
        connection
            .execute_batch(
                "UPDATE skills SET host_note = 'keep' WHERE id = 'demo';
                 CREATE TABLE skill_audit (id TEXT);
                 CREATE TRIGGER audit_skill_update
                 AFTER UPDATE OF enabled_claude ON skills BEGIN
                    INSERT INTO skill_audit VALUES (NEW.id);
                 END;",
            )
            .expect("set up host data");
        let catalog = read_skill_catalog(&connection).expect("read catalog");
        let (_first_roots, plan) = skill_plan(&catalog, Some(true));
        let (_stale_roots, stale_plan) = skill_plan(&catalog, Some(true));
        connection
            .pragma_update(None, "count_changes", true)
            .expect("enable count_changes");

        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin immediate transaction");
        assert_eq!(
            apply_skill_catalog_plan(&mut transaction, &plan).expect("apply catalog plan"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit catalog plan");

        let entry = read_skill_catalog_entry(&connection, "demo")
            .expect("read one Skill")
            .expect("Skill exists");
        assert!(selected(&entry, "enabled_claude"));
        assert_eq!(
            connection
                .query_row(
                    "SELECT host_note FROM skills WHERE id = 'demo'",
                    [],
                    |row| { row.get::<_, String>(0) }
                )
                .expect("read host value"),
            "keep"
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM skill_audit", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("read host audit"),
            1
        );

        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin stale transaction");
        assert_eq!(
            apply_skill_catalog_plan(&mut transaction, &stale_plan).expect("reject stale plan"),
            SkillCatalogWriteOutcome::NotApplied
        );
        transaction.commit().expect("commit unchanged transaction");

        let catalog = read_skill_catalog(&connection).expect("read committed catalog");
        let (_reconcile_roots, reconcile) = skill_plan(&catalog, None);
        assert!(reconcile.catalog_change().is_none());
        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin reconciliation transaction");
        assert_eq!(
            apply_skill_catalog_plan(&mut transaction, &reconcile)
                .expect("verify reconciliation guard"),
            SkillCatalogWriteOutcome::Applied
        );
    }

    #[test]
    fn suppressed_and_rewritten_catalog_writes_are_rolled_back() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute("ALTER TABLE skills ADD COLUMN host_note TEXT", [])
            .expect("add host column");
        insert_skill(&connection, "demo", "Demo", "demo");
        insert_skill(&connection, "other", "Other", "other");
        connection
            .execute(
                "UPDATE skills SET repo_owner = 'owner', host_note = 'keep' WHERE id = 'demo'",
                [],
            )
            .expect("set host metadata");
        let catalog = read_skill_catalog(&connection).expect("read catalog");
        let (_ignored_roots, ignored_plan) = skill_plan(&catalog, Some(true));
        let (_rewritten_roots, rewritten_plan) = skill_plan(&catalog, Some(true));
        let (_replaced_roots, replaced_plan) = skill_plan(&catalog, Some(true));

        connection
            .execute_batch(
                "CREATE TRIGGER ignore_skill_update
                 BEFORE UPDATE OF enabled_claude ON skills BEGIN
                    SELECT RAISE(IGNORE);
                 END;",
            )
            .expect("create suppressing trigger");
        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin suppressed transaction");
        assert_eq!(
            apply_skill_catalog_plan(&mut transaction, &ignored_plan)
                .expect("handle suppressed write"),
            SkillCatalogWriteOutcome::NotApplied
        );
        transaction.commit().expect("commit unchanged transaction");
        connection
            .execute_batch(
                "DROP TRIGGER ignore_skill_update;
                 CREATE TRIGGER rewrite_skill_catalog
                 AFTER UPDATE OF enabled_claude ON skills BEGIN
                    UPDATE skills SET enabled_codex = 1 WHERE id = 'other';
                 END;",
            )
            .expect("create rewriting trigger");

        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin rewritten transaction");
        assert_eq!(
            apply_skill_catalog_plan(&mut transaction, &rewritten_plan)
                .expect("handle rewritten write"),
            SkillCatalogWriteOutcome::NotApplied
        );
        transaction.commit().expect("commit rolled-back savepoint");
        let catalog = read_skill_catalog(&connection).expect("read unchanged catalog");
        assert!(catalog
            .iter()
            .all(|entry| entry.selections().all(|(_, selected)| !selected)));
        connection
            .execute_batch(
                "DROP TRIGGER rewrite_skill_catalog;
                 CREATE TABLE skill_child (
                    skill_id TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE
                 );
                 INSERT INTO skill_child VALUES ('demo');
                 CREATE TRIGGER replace_skill_row
                 AFTER UPDATE OF enabled_claude ON skills BEGIN
                    DELETE FROM skills WHERE id = NEW.id;
                    INSERT INTO skills (
                        id, name, description, directory, repo_owner, repo_name,
                        repo_branch, readme_url, installed_at, content_hash, updated_at,
                        enabled_claude, enabled_codex, enabled_gemini, enabled_grokbuild,
                        enabled_opencode, enabled_hermes, enabled_pi, host_note
                    ) VALUES (
                        NEW.id, NEW.name, NEW.description, NEW.directory, NEW.repo_owner,
                        NEW.repo_name, NEW.repo_branch, NEW.readme_url, NEW.installed_at,
                        NEW.content_hash, NEW.updated_at, NEW.enabled_claude,
                        NEW.enabled_codex, NEW.enabled_gemini, NEW.enabled_grokbuild,
                        NEW.enabled_opencode, NEW.enabled_hermes, NEW.enabled_pi, NEW.host_note
                    );
                 END;",
            )
            .expect("create replacing trigger");

        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin replaced transaction");
        assert_eq!(
            apply_skill_catalog_plan(&mut transaction, &replaced_plan)
                .expect("handle replaced row"),
            SkillCatalogWriteOutcome::NotApplied
        );
        transaction.commit().expect("commit rolled-back savepoint");
        assert_eq!(
            connection
                .query_row(
                    "SELECT repo_owner, host_note FROM skills WHERE id = 'demo'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .expect("read preserved host metadata"),
            ("owner".to_owned(), "keep".to_owned())
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM skill_child", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("read preserved child"),
            1
        );
    }

    #[test]
    fn rejects_selection_foreign_keys_that_rewrite_child_rows() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "CREATE UNIQUE INDEX skill_selection_parent
                    ON skills(id, enabled_claude);
                 CREATE TABLE skill_selection_child (
                    skill_id TEXT,
                    selected BOOLEAN,
                    FOREIGN KEY (skill_id, selected)
                        REFERENCES skills(id, enabled_claude) ON UPDATE SET NULL
                 );",
            )
            .expect("create selection foreign key");
        insert_skill(&connection, "demo", "Demo", "demo");
        connection
            .execute("INSERT INTO skill_selection_child VALUES ('demo', 0)", [])
            .expect("insert child row");
        let catalog = read_skill_catalog(&connection).expect("read catalog");
        let (_roots, plan) = skill_plan(&catalog, Some(true));

        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin immediate transaction");
        let error = apply_skill_catalog_plan(&mut transaction, &plan)
            .expect_err("reject mutating foreign key");
        assert!(matches!(
            error,
            SharedStoreError::InvalidDatabase(message)
                if message == "skills selection foreign keys must not update child rows"
        ));
        transaction.commit().expect("commit unchanged transaction");
        assert_eq!(
            connection
                .query_row(
                    "SELECT skill_id, selected FROM skill_selection_child",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .expect("read unchanged child"),
            ("demo".to_owned(), 0)
        );
        assert!(!selected(
            &read_skill_catalog_entry(&connection, "demo")
                .expect("read Skill")
                .expect("Skill exists"),
            "enabled_claude"
        ));
    }

    #[test]
    fn rolls_back_trigger_writes_to_skill_dependent_tables() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let mut connection = database.connect().expect("connect shared database");
        insert_skill(&connection, "demo", "Demo", "demo");
        connection
            .execute_batch(
                "CREATE TABLE skill_child (
                    skill_id TEXT REFERENCES skills(id),
                    note TEXT NOT NULL
                 );
                 INSERT INTO skill_child VALUES ('demo', 'keep');
                 CREATE TRIGGER rewrite_skill_child
                 AFTER UPDATE OF enabled_claude ON skills BEGIN
                    UPDATE skill_child SET note = 'changed' WHERE skill_id = NEW.id;
                 END;",
            )
            .expect("create dependent host data");
        let catalog = read_skill_catalog(&connection).expect("read catalog");
        let (_roots, plan) = skill_plan(&catalog, Some(true));

        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin immediate transaction");
        assert_eq!(
            apply_skill_catalog_plan(&mut transaction, &plan).expect("reject dependent write"),
            SkillCatalogWriteOutcome::NotApplied
        );
        transaction.commit().expect("commit unchanged transaction");
        assert_eq!(
            connection
                .query_row("SELECT note FROM skill_child", [], |row| {
                    row.get::<_, String>(0)
                })
                .expect("read dependent row"),
            "keep"
        );
        assert!(!selected(
            &read_skill_catalog_entry(&connection, "demo")
                .expect("read Skill")
                .expect("Skill exists"),
            "enabled_claude"
        ));
    }

    #[test]
    fn rolls_back_trigger_writes_to_skill_parent_tables() {
        let (_directory, database) = test_database();
        let connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "CREATE TABLE skill_parent (
                    directory TEXT PRIMARY KEY,
                    note TEXT NOT NULL
                 );
                 INSERT INTO skill_parent VALUES ('demo', 'keep');
                 CREATE TABLE skills (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    description TEXT,
                    directory TEXT NOT NULL REFERENCES skill_parent(directory)
                 );",
            )
            .expect("create parent-backed catalog");
        database.ensure_skill_schema().expect("initialize skills");
        let mut connection = database.connect().expect("reconnect shared database");
        insert_skill(&connection, "demo", "Demo", "demo");
        connection
            .execute_batch(
                "CREATE TRIGGER rewrite_skill_parent
                 AFTER UPDATE OF enabled_claude ON skills BEGIN
                    UPDATE skill_parent SET note = 'changed' WHERE directory = NEW.directory;
                 END;",
            )
            .expect("create parent rewrite");
        let catalog = read_skill_catalog(&connection).expect("read catalog");
        let (_roots, plan) = skill_plan(&catalog, Some(true));

        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin immediate transaction");
        assert_eq!(
            apply_skill_catalog_plan(&mut transaction, &plan).expect("reject parent write"),
            SkillCatalogWriteOutcome::NotApplied
        );
        transaction.commit().expect("commit unchanged transaction");
        assert_eq!(
            connection
                .query_row("SELECT note FROM skill_parent", [], |row| {
                    row.get::<_, String>(0)
                })
                .expect("read parent row"),
            "keep"
        );
        assert!(!selected(
            &read_skill_catalog_entry(&connection, "demo")
                .expect("read Skill")
                .expect("Skill exists"),
            "enabled_claude"
        ));
    }

    #[test]
    fn generated_host_columns_participate_in_write_validation() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute(
                "ALTER TABLE skills ADD COLUMN host_shadow INTEGER
                 GENERATED ALWAYS AS (enabled_claude) VIRTUAL",
                [],
            )
            .expect("add generated host column");
        insert_skill(&connection, "demo", "Demo", "demo");
        let catalog = read_skill_catalog(&connection).expect("read catalog");
        let (_roots, plan) = skill_plan(&catalog, Some(true));

        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin immediate transaction");
        assert_eq!(
            apply_skill_catalog_plan(&mut transaction, &plan).expect("reject generated change"),
            SkillCatalogWriteOutcome::NotApplied
        );
        transaction.commit().expect("commit unchanged transaction");
        assert_eq!(
            connection
                .query_row(
                    "SELECT enabled_claude, host_shadow FROM skills WHERE id = 'demo'",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .expect("read unchanged generated value"),
            (0, 0)
        );
    }

    #[test]
    fn catalog_write_errors_do_not_expose_trigger_messages() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let mut connection = database.connect().expect("connect shared database");
        insert_skill(&connection, "demo", "Demo", "demo");
        let catalog = read_skill_catalog(&connection).expect("read catalog");
        let (_roots, plan) = skill_plan(&catalog, Some(true));
        connection
            .execute_batch(
                "CREATE TRIGGER fail_skill_update
                 BEFORE UPDATE OF enabled_claude ON skills BEGIN
                    SELECT RAISE(FAIL, 'private trigger details');
                 END;",
            )
            .expect("create failing trigger");

        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin immediate transaction");
        let error =
            apply_skill_catalog_plan(&mut transaction, &plan).expect_err("redact write failure");
        assert!(matches!(error, SharedStoreError::SkillCatalogWrite { .. }));
        assert!(!format!("{error}").contains("private trigger details"));
        assert!(!format!("{error:?}").contains("private trigger details"));
        let error = redact_skill_catalog_post_write_error(
            SharedStoreError::InvalidDatabase("private/trigger-details".to_owned()),
            false,
        );
        assert!(matches!(error, SharedStoreError::SkillCatalogWrite { .. }));
        assert!(!format!("{error}").contains("private/trigger-details"));
        assert!(!format!("{error:?}").contains("private/trigger-details"));
        transaction
            .execute(
                "INSERT INTO skills (id, name, directory)
                 VALUES ('private', 'Private', 'private/secret')",
                [],
            )
            .expect("insert a sensitive invalid row");
        let error = apply_skill_catalog_plan(&mut transaction, &plan)
            .expect_err("redact catalog validation failure");
        assert!(matches!(error, SharedStoreError::SkillCatalogWrite { .. }));
        assert!(!format!("{error}").contains("private/secret"));
        assert!(!format!("{error:?}").contains("private/secret"));
        assert_eq!(
            read_skill_catalog_entry(&transaction, "demo")
                .expect("read rolled-back Skill")
                .expect("Skill exists")
                .directory(),
            "demo"
        );
    }
}
