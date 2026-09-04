use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

use cc_switch_core::{
    builtin_app_registry, skill_catalog_columns, AppType, SkillCatalogColumn, SkillCatalogEntry,
    SkillCatalogEntryError, SkillSwitchPlan,
};
use rusqlite::{
    params, params_from_iter,
    types::{ToSqlOutput, Value, ValueRef},
    Connection, OptionalExtension, Row, ToSql, Transaction, TransactionBehavior,
};

use crate::{source_fingerprint, ExistingColumn, SharedStoreError};

/// Canonical installed Skill catalog shared by CC Switch products.
pub const SKILLS_TABLE: &str = "skills";

static NEXT_SKILL_WRITE_GUARD: AtomicU64 = AtomicU64::new(0);

const CREATE_SKILLS_TABLE: &str = "CREATE TABLE IF NOT EXISTS main.skills (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    directory TEXT NOT NULL
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
    host_fingerprint: [u8; 32],
    source_fingerprint: [u8; 32],
    source_columns: Vec<String>,
    source_values: Vec<RawSqlValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RawSqlValue {
    Null,
    Integer(i64),
    Real(u64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl RawSqlValue {
    fn from_value(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(value) => Self::Integer(value),
            ValueRef::Real(value) => {
                let canonical = if value == 0.0 { 0.0 } else { value };
                Self::Real(canonical.to_bits())
            }
            ValueRef::Text(value) => Self::Text(value.to_vec()),
            ValueRef::Blob(value) => Self::Blob(value.to_vec()),
        }
    }

    fn text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => std::str::from_utf8(value).ok(),
            Self::Null | Self::Integer(_) | Self::Real(_) | Self::Blob(_) => None,
        }
    }

    fn sqlite_type(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Integer(_) => "integer",
            Self::Real(_) => "real",
            Self::Text(_) => "text",
            Self::Blob(_) => "blob",
        }
    }

    fn from_owned(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Integer(value) => Self::Integer(value),
            Value::Real(value) => {
                let canonical = if value == 0.0 { 0.0 } else { value };
                Self::Real(canonical.to_bits())
            }
            Value::Text(value) => Self::Text(value.into_bytes()),
            Value::Blob(value) => Self::Blob(value),
        }
    }
}

impl ToSql for RawSqlValue {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Self::Null => ToSqlOutput::Owned(Value::Null),
            Self::Integer(value) => ToSqlOutput::Owned(Value::Integer(*value)),
            Self::Real(value) => ToSqlOutput::Owned(Value::Real(f64::from_bits(*value))),
            Self::Text(value) => ToSqlOutput::Borrowed(ValueRef::Text(value)),
            Self::Blob(value) => ToSqlOutput::Borrowed(ValueRef::Blob(value)),
        })
    }
}

struct StoredSkill {
    entry: SkillCatalogEntry,
    host_fingerprint: [u8; 32],
}

struct SkillCatalogReplacement<'value> {
    id: &'value str,
    name: &'value str,
    description: Option<&'value str>,
    directory: &'value str,
    selections: &'value [(SkillCatalogColumn, bool)],
}

/// One unvalidated row from the shared installed-Skill catalog.
///
/// The raw source stays opaque so malformed legacy rows remain removable
/// without exposing host data. [`SkillCatalogRow::values`] returns a typed
/// view only when every shared field has its canonical SQLite storage type.
#[derive(Clone, PartialEq, Eq)]
pub struct SkillCatalogRow {
    values: Option<SkillCatalogValues>,
    selections: Option<Vec<(SkillCatalogColumn, bool)>>,
    host_fingerprint: [u8; 32],
    source_fingerprint: [u8; 32],
    source_columns: Vec<String>,
    source_values: Vec<RawSqlValue>,
}

/// Typed shared fields from one raw Skill catalog row.
///
/// Values are storage-valid but deliberately not subject to product path or
/// display-text policy.
#[derive(Clone, PartialEq, Eq)]
pub struct SkillCatalogValues {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub directory: String,
    selections: Vec<(SkillCatalogColumn, bool)>,
}

impl SkillCatalogRow {
    /// Returns the typed shared fields, or `None` for a malformed legacy row.
    pub fn values(&self) -> Option<&SkillCatalogValues> {
        self.values.as_ref()
    }

    /// Returns registry selections when those individual values are valid.
    pub fn selections(
        &self,
    ) -> Option<
        impl ExactSizeIterator<Item = (SkillCatalogColumn, bool)> + DoubleEndedIterator + Clone + '_,
    > {
        self.selections
            .as_ref()
            .map(|selections| selections.iter().copied())
    }

    /// Returns one registry selection when that individual value is valid.
    ///
    /// A malformed value in another application's column does not hide this
    /// one, which lets hosts repair rows without clearing valid selections
    /// they do not expose.
    pub fn selected_for(&self, app: &AppType) -> Option<bool> {
        let column = builtin_app_registry()
            .for_app(app)
            .skill_contract()?
            .catalog_column();
        let index = self
            .source_columns
            .iter()
            .position(|candidate| candidate == column.as_str())?;
        match self.source_values.get(index)? {
            RawSqlValue::Integer(0) => Some(false),
            RawSqlValue::Integer(1) => Some(true),
            RawSqlValue::Null
            | RawSqlValue::Integer(_)
            | RawSqlValue::Real(_)
            | RawSqlValue::Text(_)
            | RawSqlValue::Blob(_) => None,
        }
    }

    /// Returns the identifier when that individual value is valid UTF-8 text.
    pub fn id(&self) -> Option<&str> {
        self.source_values.first().and_then(RawSqlValue::text)
    }

    /// Identifies the complete source row, including unknown host columns,
    /// without exposing their contents.
    pub fn source_fingerprint(&self) -> &[u8; 32] {
        &self.source_fingerprint
    }
}

impl fmt::Debug for SkillCatalogRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SkillCatalogRow")
            .field("has_valid_values", &self.values.is_some())
            .field("has_valid_selections", &self.selections.is_some())
            .field("source", &"[redacted]")
            .finish()
    }
}

impl SkillCatalogValues {
    /// Returns the complete set of registry-owned selections in registry order.
    pub fn selections(
        &self,
    ) -> impl ExactSizeIterator<Item = (SkillCatalogColumn, bool)> + DoubleEndedIterator + Clone + '_
    {
        self.selections.iter().copied()
    }

    /// Returns this row's selection for one registry application.
    pub fn selected_for(&self, app: &AppType) -> Option<bool> {
        let column = builtin_app_registry()
            .for_app(app)
            .skill_contract()?
            .catalog_column();
        self.selections
            .iter()
            .find_map(|(candidate, selected)| (*candidate == column).then_some(*selected))
    }
}

impl fmt::Debug for SkillCatalogValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SkillCatalogValues { [redacted] }")
    }
}

struct SkillProjection {
    selections: Vec<SkillCatalogColumn>,
    columns: Vec<String>,
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

#[derive(Clone, Copy)]
enum SkillRowOperation {
    Insert,
    Delete,
}

struct SkillRowWriteGuard {
    table: String,
    triggers: Vec<String>,
    duplicate_deletes_allowed: bool,
}

impl SkillRowWriteGuard {
    fn install(
        connection: &Connection,
        operation: SkillRowOperation,
        target: Option<&SkillCatalogRow>,
    ) -> rusqlite::Result<Self> {
        let id = NEXT_SKILL_WRITE_GUARD.fetch_add(1, Ordering::Relaxed);
        let table = format!("cc_switch_skill_row_guard_{id}");
        let target = match operation {
            SkillRowOperation::Insert => None,
            SkillRowOperation::Delete => Some(target.ok_or(rusqlite::Error::InvalidQuery)?),
        };
        let target_columns = target.map_or_else(Vec::new, |target| {
            (0..target.source_values.len())
                .map(|index| format!("target_{index}"))
                .collect::<Vec<_>>()
        });
        let target_definition = target_columns
            .iter()
            .map(|column| format!(", \"{column}\""))
            .collect::<String>();
        connection.execute_batch(&format!(
            "CREATE TEMP TABLE \"{table}\" (
                writes INTEGER NOT NULL,
                violations INTEGER NOT NULL
                {target_definition}
             );"
        ))?;
        let guard_sql = format!(
            "INSERT INTO temp.\"{table}\" VALUES ({})",
            (1..=2 + target_columns.len())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let zeroes = [Value::Integer(0), Value::Integer(0)];
        let mut guard_parameters = zeroes
            .iter()
            .map(|value| value as &dyn ToSql)
            .collect::<Vec<_>>();
        if let Some(target) = target {
            guard_parameters.extend(target.source_values.iter().map(|value| value as &dyn ToSql));
        }
        execute_statement_consuming_rows(
            connection,
            &guard_sql,
            params_from_iter(guard_parameters),
        )?;

        let allowed = match operation {
            SkillRowOperation::Insert => "INSERT",
            SkillRowOperation::Delete => "DELETE",
        };
        let mut triggers = Vec::new();
        let allowed_trigger = format!("cc_switch_skill_row_allow_{id}");
        let guard_body = target.map_or_else(
            || {
                format!(
                    "UPDATE \"{table}\"
                        SET violations = violations + (writes != 0),
                            writes = writes + (writes = 0);
                     SELECT CASE WHEN (SELECT violations FROM \"{table}\") != 0
                        THEN RAISE(IGNORE) END;"
                )
            },
            |target| {
                let target_matches = target
                    .source_columns
                    .iter()
                    .zip(&target_columns)
                    .map(|(source, target)| {
                        format!(
                            "typeof(OLD.{source}) = typeof((SELECT \"{target}\" FROM \"{table}\"))
                             AND OLD.{source} COLLATE BINARY IS
                                 (SELECT \"{target}\" FROM \"{table}\")",
                            source = quoted_identifier(source),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" AND ");
                format!(
                    "UPDATE \"{table}\"
                        SET violations = violations + NOT ({target_matches}),
                            writes = writes + ({target_matches});
                     SELECT CASE
                        WHEN (SELECT violations FROM \"{table}\") != 0
                          OR (SELECT writes FROM \"{table}\") > 1
                        THEN RAISE(IGNORE) END;"
                )
            },
        );
        connection.execute_batch(&format!(
            "CREATE TEMP TRIGGER \"{allowed_trigger}\" BEFORE {allowed} ON main.skills BEGIN
                {guard_body}
             END;"
        ))?;
        triggers.push(allowed_trigger);

        for blocked in ["INSERT", "UPDATE", "DELETE"] {
            if blocked == allowed {
                continue;
            }
            let trigger = format!("cc_switch_skill_row_block_{id}_{}", triggers.len());
            connection.execute_batch(&format!(
                "CREATE TEMP TRIGGER \"{trigger}\" BEFORE {blocked} ON main.skills BEGIN
                    UPDATE \"{table}\" SET violations = violations + 1;
                    SELECT RAISE(IGNORE);
                 END;"
            ))?;
            triggers.push(trigger);
        }
        Ok(Self {
            table,
            triggers,
            duplicate_deletes_allowed: target.is_some(),
        })
    }

    fn accepted(&self, connection: &Connection) -> rusqlite::Result<bool> {
        connection.query_row(
            &format!(
                "SELECT {} AND violations = 0 FROM temp.\"{}\"",
                if self.duplicate_deletes_allowed {
                    "writes >= 1"
                } else {
                    "writes = 1"
                },
                self.table,
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

fn execute_statement_consuming_rows<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    params: P,
) -> rusqlite::Result<()> {
    let mut statement = connection.prepare(sql)?;
    let mut rows = statement.query(params)?;
    while rows.next()?.is_some() {}
    Ok(())
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
/// A new table contains only the shared base fields and Core-declared
/// selection columns. Existing host columns, rows, indexes, and triggers are
/// kept; each product owns creation and migration of its metadata columns.
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
    Ok(read_stored_skill_rows(connection, &projection)?
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

/// Reads every shared catalog row without decoding failures from malformed
/// legacy values. The schema and typed values still follow the Core registry.
pub fn read_skill_catalog_rows(
    connection: &Connection,
) -> Result<Vec<SkillCatalogRow>, SharedStoreError> {
    let projection = skill_write_projection(connection)?;
    Ok(read_raw_skill_rows(connection, &projection)?
        .into_iter()
        .map(|row| skill_catalog_row_from_raw(row, &projection.selections))
        .collect())
}

/// Reads one shared catalog row by its binary identifier.
pub fn read_skill_catalog_row(
    connection: &Connection,
    id: &str,
) -> Result<Option<SkillCatalogRow>, SharedStoreError> {
    let projection = skill_write_projection(connection)?;
    let sql = format!(
        "SELECT {} FROM main.skills AS skills WHERE id COLLATE BINARY = ?1",
        projection.select_list
    );
    Ok(connection
        .query_row(&sql, [id], |row| raw_skill(row, &projection))
        .optional()?
        .map(|row| skill_catalog_row_from_raw(row, &projection.selections)))
}

/// Inserts one complete registry-owned Skill catalog row when its binary
/// identifier is absent. Identity and directory strings are stored without
/// product-level path validation; host metadata columns retain their defaults.
pub fn insert_skill_catalog_if_absent(
    transaction: &mut Transaction<'_>,
    id: &str,
    name: &str,
    description: Option<&str>,
    directory: &str,
    selections: impl IntoIterator<Item = (SkillCatalogColumn, bool)>,
) -> Result<SkillCatalogWriteOutcome, SharedStoreError> {
    prepare_skill_catalog_write(transaction)?;
    if read_skill_catalog_row(transaction, id)
        .map_err(|error| redact_skill_catalog_store_error(error, transaction.is_autocommit()))?
        .is_some()
    {
        return Ok(SkillCatalogWriteOutcome::NotApplied);
    }
    let before = read_skill_catalog_rows(transaction)
        .map_err(|error| redact_skill_catalog_store_error(error, transaction.is_autocommit()))?;
    let selections = complete_skill_selections(selections)?;
    let mut columns = vec!["id", "name", "description", "directory"];
    columns.extend(selections.iter().map(|(column, _)| column.as_str()));
    let placeholders = (1..=columns.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO main.skills ({}) VALUES ({placeholders})",
        columns
            .into_iter()
            .map(quoted_identifier)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut values = vec![
        Value::Text(id.to_owned()),
        Value::Text(name.to_owned()),
        description.map_or(Value::Null, |value| Value::Text(value.to_owned())),
        Value::Text(directory.to_owned()),
    ];
    values.extend(
        selections
            .iter()
            .map(|(_, selected)| Value::Integer(i64::from(*selected))),
    );
    execute_skill_row_write(
        transaction,
        &sql,
        params_from_iter(values.iter()),
        SkillRowOperation::Insert,
        None,
        |connection| {
            let after = read_skill_catalog_rows(connection)?;
            let inserted = after.iter().find(|row| row.id() == Some(id));
            Ok(after.len() == before.len() + 1
                && inserted.is_some_and(|row| {
                    row.values().is_some_and(|values| {
                        values.name == name
                            && values.description.as_deref() == description
                            && values.directory == directory
                            && values.selections().eq(selections.iter().copied())
                    }) && rows_without(&after, row)
                        .is_some_and(|remaining| rows_match_unordered(&remaining, &before))
                }))
        },
    )
}

/// Replaces the shared base fields and registry-owned selections only when the
/// supplied raw row is unchanged. Strings are not path-validated; product
/// metadata and unknown columns are never assigned.
pub fn update_skill_catalog_if_unchanged(
    transaction: &mut Transaction<'_>,
    current: &SkillCatalogRow,
    id: &str,
    name: &str,
    description: Option<&str>,
    directory: &str,
    replacements: impl IntoIterator<Item = (SkillCatalogColumn, bool)>,
) -> Result<SkillCatalogWriteOutcome, SharedStoreError> {
    prepare_skill_catalog_write(transaction)?;
    if !skill_snapshot_is_current(transaction, current)
        .map_err(|error| redact_skill_catalog_store_error(error, transaction.is_autocommit()))?
    {
        return Ok(SkillCatalogWriteOutcome::NotApplied);
    }
    let replacements = complete_skill_selections(replacements)?;
    let mut desired_values = vec![
        RawSqlValue::Text(id.as_bytes().to_vec()),
        RawSqlValue::Text(name.as_bytes().to_vec()),
        description.map_or(RawSqlValue::Null, |value| {
            RawSqlValue::Text(value.as_bytes().to_vec())
        }),
        RawSqlValue::Text(directory.as_bytes().to_vec()),
    ];
    desired_values.extend(
        replacements
            .iter()
            .map(|(_, selected)| RawSqlValue::Integer(i64::from(*selected))),
    );
    let changes = current
        .source_values
        .iter()
        .zip(desired_values.iter())
        .zip(current.source_columns.iter())
        .filter(|((current, desired), _)| current != desired)
        .map(|((_, desired), column)| (column.clone(), desired.clone()))
        .collect::<Vec<_>>();
    if changes.is_empty() {
        return Ok(SkillCatalogWriteOutcome::Applied);
    }
    for (column, _) in &changes {
        if let Some((selection, _)) = replacements
            .iter()
            .find(|(selection, _)| selection.as_str() == column)
        {
            verify_skill_foreign_key_write_contract(transaction, *selection).map_err(|error| {
                redact_skill_catalog_store_error(error, transaction.is_autocommit())
            })?;
        }
    }

    let before = read_skill_catalog_rows(transaction)
        .map_err(|error| redact_skill_catalog_store_error(error, transaction.is_autocommit()))?;
    let assignments = changes
        .iter()
        .enumerate()
        .map(|(index, (column, _))| format!("{} = ?{}", quoted_identifier(column), index + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let snapshot_predicate = skill_snapshot_predicate(current, changes.len() + 1);
    let sql = format!(
        "UPDATE main.skills SET {assignments}
         WHERE {snapshot_predicate}"
    );
    let mut parameters = changes
        .iter()
        .map(|(_, value)| value as &dyn ToSql)
        .collect::<Vec<_>>();
    parameters.extend(
        current
            .source_values
            .iter()
            .map(|value| value as &dyn ToSql),
    );
    let replacement = SkillCatalogReplacement {
        id,
        name,
        description,
        directory,
        selections: &replacements,
    };
    execute_skill_catalog_write(
        transaction,
        &sql,
        params_from_iter(parameters),
        |connection| {
            let after = read_skill_catalog_rows(connection)?;
            Ok(skill_rows_match_replacement(
                &before,
                &after,
                current,
                &replacement,
            ))
        },
    )
}

/// Updates product-owned columns on one unchanged Skill row.
///
/// Column names must resolve to host extensions outside Core's base fields and
/// registry selections. The guarded write rejects suppression, row
/// replacement, changes to any unlisted target column, and writes to another
/// Skill row. This lets a host compose its metadata update with Core catalog
/// writes in one outer transaction without teaching Core the host schema.
pub fn update_skill_host_fields_if_unchanged(
    transaction: &mut Transaction<'_>,
    current: &SkillCatalogRow,
    replacements: impl IntoIterator<Item = (String, Value)>,
) -> Result<SkillCatalogWriteOutcome, SharedStoreError> {
    prepare_skill_catalog_write(transaction)?;
    if !skill_snapshot_is_current(transaction, current)
        .map_err(|error| redact_skill_catalog_store_error(error, transaction.is_autocommit()))?
    {
        return Ok(SkillCatalogWriteOutcome::NotApplied);
    }

    let host_offset = 4 + skill_catalog_columns().count();
    let mut desired = Vec::new();
    for (column, value) in replacements {
        let Some(index) = current
            .source_columns
            .iter()
            .position(|candidate| candidate == &column)
        else {
            return Err(SharedStoreError::InvalidDatabase(
                "Skill host update references an absent column".to_owned(),
            ));
        };
        if index < host_offset || desired.iter().any(|(candidate, _)| *candidate == index) {
            return Err(SharedStoreError::InvalidDatabase(
                "Skill host updates must name distinct product-owned columns".to_owned(),
            ));
        }
        desired.push((index, RawSqlValue::from_owned(value)));
    }

    let changes = desired
        .into_iter()
        .filter(|(index, value)| current.source_values[*index] != *value)
        .collect::<Vec<_>>();
    if changes.is_empty() {
        return Ok(SkillCatalogWriteOutcome::Applied);
    }

    let before = read_skill_catalog_rows(transaction)
        .map_err(|error| redact_skill_catalog_store_error(error, transaction.is_autocommit()))?;
    let assignments = changes
        .iter()
        .enumerate()
        .map(|(offset, (index, _))| {
            format!(
                "{} = ?{}",
                quoted_identifier(&current.source_columns[*index]),
                offset + 1
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let snapshot_predicate = skill_snapshot_predicate(current, changes.len() + 1);
    let sql = format!("UPDATE main.skills SET {assignments} WHERE {snapshot_predicate}");
    let mut parameters = changes
        .iter()
        .map(|(_, value)| value as &dyn ToSql)
        .collect::<Vec<_>>();
    parameters.extend(
        current
            .source_values
            .iter()
            .map(|value| value as &dyn ToSql),
    );
    let mut expected_values = current.source_values.clone();
    for (index, value) in &changes {
        expected_values[*index] = value.clone();
    }

    execute_skill_catalog_write(
        transaction,
        &sql,
        params_from_iter(parameters),
        |connection| {
            let after = read_skill_catalog_rows(connection)?;
            let candidates = after
                .iter()
                .filter(|row| {
                    row.source_columns == current.source_columns
                        && row.source_values == expected_values
                })
                .collect::<Vec<_>>();
            Ok(candidates.len() == 1
                && rows_without(&before, current)
                    .zip(rows_without(&after, candidates[0]))
                    .is_some_and(|(before, after)| rows_match_unordered(&before, &after)))
        },
    )
}

/// Deletes one raw Skill catalog row only when it is unchanged. This includes
/// legacy rows whose identifier is `NULL`. Host triggers and foreign keys may
/// clean dependent rows, but they cannot mutate another Skill row.
pub fn delete_skill_catalog_if_unchanged(
    transaction: &mut Transaction<'_>,
    current: &SkillCatalogRow,
) -> Result<SkillCatalogWriteOutcome, SharedStoreError> {
    prepare_skill_catalog_write(transaction)?;
    if !skill_snapshot_is_current(transaction, current)
        .map_err(|error| redact_skill_catalog_store_error(error, transaction.is_autocommit()))?
    {
        return Ok(SkillCatalogWriteOutcome::NotApplied);
    }
    let before = read_skill_catalog_rows(transaction)
        .map_err(|error| redact_skill_catalog_store_error(error, transaction.is_autocommit()))?;
    let snapshot_predicate = skill_snapshot_predicate(current, 1);
    let sql = format!("DELETE FROM main.skills WHERE {snapshot_predicate}");
    let parameters = current
        .source_values
        .iter()
        .map(|value| value as &dyn ToSql)
        .collect::<Vec<_>>();
    execute_skill_row_write(
        transaction,
        &sql,
        params_from_iter(parameters),
        SkillRowOperation::Delete,
        Some(current),
        |connection| {
            let after = read_skill_catalog_rows(connection)?;
            Ok(rows_without(&before, current)
                .is_some_and(|remaining| rows_match_unordered(&remaining, &after)))
        },
    )
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
    read_stored_skill_rows(connection, &projection)
}

fn read_stored_skill_rows(
    connection: &Connection,
    projection: &SkillProjection,
) -> Result<Vec<StoredSkill>, SharedStoreError> {
    read_raw_skill_rows(connection, projection)?
        .into_iter()
        .map(|row| skill_from_raw(row, &projection.selections))
        .collect()
}

fn read_raw_skill_rows(
    connection: &Connection,
    projection: &SkillProjection,
) -> Result<Vec<RawSkill>, SharedStoreError> {
    let sql = format!(
        "SELECT {} FROM main.skills AS skills
         ORDER BY name COLLATE BINARY, id COLLATE BINARY",
        projection.select_list
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], |row| raw_skill(row, projection))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(SharedStoreError::from)
}

fn skill_snapshot_is_current(
    connection: &Connection,
    current: &SkillCatalogRow,
) -> Result<bool, SharedStoreError> {
    Ok(read_skill_catalog_rows(connection)?
        .iter()
        .any(|row| row == current))
}

fn skill_snapshot_predicate(current: &SkillCatalogRow, first_parameter: usize) -> String {
    current
        .source_columns
        .iter()
        .zip(&current.source_values)
        .enumerate()
        .map(|(offset, (column, value))| {
            format!(
                "typeof({}) = '{}' AND {} COLLATE BINARY IS ?{}",
                quoted_identifier(column),
                value.sqlite_type(),
                quoted_identifier(column),
                first_parameter + offset
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn core_skill_projection() -> SkillProjection {
    let selections = skill_catalog_columns().collect::<Vec<_>>();
    let mut columns = vec![
        "id".to_owned(),
        "name".to_owned(),
        "description".to_owned(),
        "directory".to_owned(),
    ];
    columns.extend(selections.iter().map(|column| column.as_str().to_owned()));
    let host_offset = columns.len();
    let select_list = columns
        .iter()
        .map(|column| quoted_skill_column(column))
        .collect::<Vec<_>>()
        .join(", ");
    SkillProjection {
        selections,
        columns,
        select_list,
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
        .map(|column| column.name)
        .collect::<Vec<_>>();
    if !host_columns.is_empty() {
        projection.columns.extend(host_columns);
        projection.select_list.push_str(", ");
        projection.select_list.push_str(
            &projection.columns[projection.host_offset..]
                .iter()
                .map(|column| quoted_skill_column(column))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    Ok(projection)
}

fn quoted_skill_column(column: &str) -> String {
    format!("skills.\"{}\"", column.replace('"', "\"\""))
}

fn raw_skill(row: &Row<'_>, projection: &SkillProjection) -> rusqlite::Result<RawSkill> {
    let source_values = (0..projection.columns.len())
        .map(|index| row.get_ref(index).map(RawSqlValue::from_value))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RawSkill {
        host_fingerprint: source_fingerprint(row, projection.host_offset)?,
        source_fingerprint: source_fingerprint(row, 0)?,
        source_columns: projection.columns.clone(),
        source_values,
    })
}

fn skill_catalog_row_from_raw(row: RawSkill, selections: &[SkillCatalogColumn]) -> SkillCatalogRow {
    let values = skill_catalog_values(&row.source_values, selections).ok();
    let selections = typed_skill_selections(&row.source_values, selections).ok();
    SkillCatalogRow {
        values,
        selections,
        host_fingerprint: row.host_fingerprint,
        source_fingerprint: row.source_fingerprint,
        source_columns: row.source_columns,
        source_values: row.source_values,
    }
}

fn skill_from_raw(
    row: RawSkill,
    selections: &[SkillCatalogColumn],
) -> Result<StoredSkill, SharedStoreError> {
    let values = skill_catalog_values(&row.source_values, selections)?;
    let entry = SkillCatalogEntry::try_new(
        values.id,
        values.name,
        values.description,
        values.directory,
        values.selections,
    )
    .map_err(invalid_skill_row)?;
    Ok(StoredSkill {
        entry,
        host_fingerprint: row.host_fingerprint,
    })
}

fn skill_catalog_values(
    source_values: &[RawSqlValue],
    selections: &[SkillCatalogColumn],
) -> Result<SkillCatalogValues, SharedStoreError> {
    if source_values.len() < 4 + selections.len() {
        return Err(invalid_skill_storage_row());
    }
    let id = source_values[0]
        .text()
        .ok_or_else(invalid_skill_storage_row)?
        .to_owned();
    let name = source_values[1]
        .text()
        .ok_or_else(invalid_skill_storage_row)?
        .to_owned();
    let description = match &source_values[2] {
        RawSqlValue::Null => None,
        value => Some(
            value
                .text()
                .ok_or_else(invalid_skill_storage_row)?
                .to_owned(),
        ),
    };
    let directory = source_values[3]
        .text()
        .ok_or_else(invalid_skill_storage_row)?
        .to_owned();
    let selections = typed_skill_selections(source_values, selections)?;
    Ok(SkillCatalogValues {
        id,
        name,
        description,
        directory,
        selections,
    })
}

fn typed_skill_selections(
    source_values: &[RawSqlValue],
    selections: &[SkillCatalogColumn],
) -> Result<Vec<(SkillCatalogColumn, bool)>, SharedStoreError> {
    if source_values.len() < 4 + selections.len() {
        return Err(invalid_skill_storage_row());
    }
    let selected_values = source_values[4..4 + selections.len()]
        .iter()
        .map(|value| match value {
            RawSqlValue::Integer(0) => Ok(false),
            RawSqlValue::Integer(1) => Ok(true),
            _ => Err(SharedStoreError::InvalidDatabase(
                "skills selection must be 0 or 1".to_owned(),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(selections.iter().copied().zip(selected_values).collect())
}

fn invalid_skill_storage_row() -> SharedStoreError {
    SharedStoreError::InvalidDatabase(
        "skills row does not match the shared storage contract".to_owned(),
    )
}

fn complete_skill_selections(
    selections: impl IntoIterator<Item = (SkillCatalogColumn, bool)>,
) -> Result<Vec<(SkillCatalogColumn, bool)>, SharedStoreError> {
    let selections = selections.into_iter().collect::<Vec<_>>();
    if !skill_catalog_columns().eq(selections.iter().map(|(column, _)| *column)) {
        return Err(SharedStoreError::InvalidDatabase(
            "Skill catalog selections must match the registry".to_owned(),
        ));
    }
    Ok(selections)
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

fn rows_without(
    rows: &[SkillCatalogRow],
    removed: &SkillCatalogRow,
) -> Option<Vec<SkillCatalogRow>> {
    let index = rows.iter().position(|row| row == removed)?;
    let mut remaining = rows.to_vec();
    remaining.remove(index);
    Some(remaining)
}

fn rows_match_unordered(left: &[SkillCatalogRow], right: &[SkillCatalogRow]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut unmatched = right.to_vec();
    for row in left {
        let Some(index) = unmatched.iter().position(|candidate| candidate == row) else {
            return false;
        };
        unmatched.remove(index);
    }
    unmatched.is_empty()
}

fn skill_rows_match_replacement(
    before: &[SkillCatalogRow],
    after: &[SkillCatalogRow],
    current: &SkillCatalogRow,
    replacement: &SkillCatalogReplacement<'_>,
) -> bool {
    let candidates = after
        .iter()
        .filter(|row| {
            row.source_values.first()
                == Some(&RawSqlValue::Text(replacement.id.as_bytes().to_vec()))
                && row.host_fingerprint == current.host_fingerprint
                && row.values().is_some_and(|values| {
                    values.id == replacement.id
                        && values.name == replacement.name
                        && values.description.as_deref() == replacement.description
                        && values.directory == replacement.directory
                        && values
                            .selections()
                            .eq(replacement.selections.iter().copied())
                })
        })
        .collect::<Vec<_>>();
    candidates.len() == 1
        && rows_without(before, current)
            .zip(rows_without(after, candidates[0]))
            .is_some_and(|(before, after)| rows_match_unordered(&before, &after))
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

fn execute_skill_row_write<P, F>(
    transaction: &mut Transaction<'_>,
    sql: &str,
    params: P,
    operation: SkillRowOperation,
    target: Option<&SkillCatalogRow>,
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
        let guard = SkillRowWriteGuard::install(&savepoint, operation, target)?;
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

    fn catalog_values(row: &SkillCatalogRow) -> &SkillCatalogValues {
        row.values().expect("storage-valid catalog row")
    }

    fn raw_text(row: &SkillCatalogRow, index: usize) -> Option<&str> {
        row.source_values.get(index)?.text()
    }

    fn catalog_entry(
        id: &str,
        name: &str,
        directory: &str,
        selected_columns: &[&str],
    ) -> SkillCatalogEntry {
        SkillCatalogEntry::try_new(
            id,
            name,
            None,
            directory,
            skill_catalog_columns().map(|column| {
                let selected = selected_columns.contains(&column.as_str());
                (column, selected)
            }),
        )
        .expect("valid Skill catalog entry")
    }

    fn insert_catalog_entry(
        transaction: &mut Transaction<'_>,
        entry: &SkillCatalogEntry,
    ) -> Result<SkillCatalogWriteOutcome, SharedStoreError> {
        insert_skill_catalog_if_absent(
            transaction,
            entry.id(),
            entry.name(),
            entry.description(),
            entry.directory(),
            entry.selections(),
        )
    }

    #[test]
    fn creates_minimal_registry_catalog_without_product_metadata_columns() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let connection = database.connect().expect("connect shared database");
        let columns = skill_columns(&connection).expect("read skill columns");

        for host_column in [
            "repo_owner",
            "repo_name",
            "repo_branch",
            "readme_url",
            "installed_at",
            "content_hash",
            "updated_at",
        ] {
            assert!(!columns.iter().any(|column| column.name == host_column));
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
    fn catalog_crud_uses_registry_complete_values_and_full_row_cas() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "ALTER TABLE skills ADD COLUMN repo_owner TEXT;
                 ALTER TABLE skills ADD COLUMN host_note TEXT;",
            )
            .expect("add host columns");
        let entry = catalog_entry("demo", "Demo", "demo", &["enabled_grokbuild", "enabled_pi"]);

        connection
            .pragma_update(None, "count_changes", true)
            .expect("enable count_changes");
        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin insert");
        assert_eq!(
            insert_catalog_entry(&mut transaction, &entry).expect("insert catalog row"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit insert");
        connection
            .pragma_update(None, "count_changes", false)
            .expect("disable count_changes");
        let inserted = read_skill_catalog_row(&connection, "demo")
            .expect("read inserted row")
            .expect("row exists");
        assert_eq!(
            catalog_values(&inserted).selected_for(&AppType::GrokBuild),
            Some(true)
        );
        assert_eq!(
            catalog_values(&inserted).selected_for(&AppType::Pi),
            Some(true)
        );

        connection
            .execute(
                "UPDATE skills SET host_note = 'changed' WHERE id = 'demo'",
                [],
            )
            .expect("change host extension");
        let replacement = catalog_entry(
            "demo-updated",
            "Demo Updated",
            "demo-next",
            &["enabled_claude", "enabled_grokbuild", "enabled_pi"],
        );
        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin stale update");
        assert_eq!(
            update_skill_catalog_if_unchanged(
                &mut transaction,
                &inserted,
                replacement.id(),
                replacement.name(),
                replacement.description(),
                replacement.directory(),
                replacement.selections()
            )
            .expect("reject stale update"),
            SkillCatalogWriteOutcome::NotApplied
        );
        transaction.commit().expect("commit stale update");

        let current = read_skill_catalog_row(&connection, "demo")
            .expect("read current row")
            .expect("row exists");
        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin fresh update");
        assert_eq!(
            update_skill_catalog_if_unchanged(
                &mut transaction,
                &current,
                replacement.id(),
                replacement.name(),
                replacement.description(),
                replacement.directory(),
                replacement.selections(),
            )
            .expect("apply fresh update"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit fresh update");
        let updated = read_skill_catalog_row(&connection, "demo-updated")
            .expect("read updated row")
            .expect("row exists");
        assert!(read_skill_catalog_row(&connection, "demo")
            .expect("read old identity")
            .is_none());
        let updated_values = catalog_values(&updated);
        assert_eq!(updated_values.selected_for(&AppType::Claude), Some(true));
        assert_eq!(updated_values.selected_for(&AppType::GrokBuild), Some(true));
        assert_eq!(updated_values.selected_for(&AppType::Pi), Some(true));
        assert_eq!(updated_values.name, "Demo Updated");
        assert_eq!(updated_values.directory, "demo-next");
        assert_eq!(
            connection
                .query_row(
                    "SELECT host_note FROM skills WHERE id = 'demo-updated'",
                    [],
                    |row| { row.get::<_, String>(0) }
                )
                .expect("read host extension"),
            "changed"
        );
    }

    #[test]
    fn host_field_update_is_guarded_without_owning_host_schema() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "ALTER TABLE skills ADD COLUMN host_note TEXT;
                 ALTER TABLE skills ADD COLUMN host_extension TEXT DEFAULT 'keep';",
            )
            .expect("add host columns");
        let entry = catalog_entry("demo", "Demo", "demo", &[]);
        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin insert");
        assert_eq!(
            insert_catalog_entry(&mut transaction, &entry).expect("insert catalog row"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit insert");

        let current = read_skill_catalog_row(&connection, "demo")
            .expect("read current row")
            .expect("row exists");
        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin host update");
        assert_eq!(
            update_skill_host_fields_if_unchanged(
                &mut transaction,
                &current,
                [("host_note".to_owned(), Value::Text("owner".to_owned()))],
            )
            .expect("update host field"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit host update");
        assert_eq!(
            connection
                .query_row(
                    "SELECT host_note, host_extension FROM skills WHERE id = 'demo'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .expect("read host fields"),
            ("owner".to_owned(), "keep".to_owned())
        );

        connection
            .execute_batch(
                "CREATE TRIGGER rewrite_unknown_host_field
                 AFTER UPDATE OF host_note ON skills
                 WHEN NEW.id = 'demo'
                 BEGIN
                    UPDATE skills SET host_extension = 'rewritten' WHERE id = NEW.id;
                 END;",
            )
            .expect("create hostile trigger");
        let current = read_skill_catalog_row(&connection, "demo")
            .expect("read guarded row")
            .expect("row exists");
        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin rejected update");
        assert_eq!(
            update_skill_host_fields_if_unchanged(
                &mut transaction,
                &current,
                [("host_note".to_owned(), Value::Text("next".to_owned()))],
            )
            .expect("reject trigger rewrite"),
            SkillCatalogWriteOutcome::NotApplied
        );
        transaction.commit().expect("commit rolled-back savepoint");
        assert_eq!(
            connection
                .query_row(
                    "SELECT host_note, host_extension FROM skills WHERE id = 'demo'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .expect("read preserved host fields"),
            ("owner".to_owned(), "keep".to_owned())
        );
    }

    #[test]
    fn raw_catalog_rows_keep_host_paths_and_null_id_removable() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "ALTER TABLE skills ADD COLUMN rowid INTEGER;
                 ALTER TABLE skills ADD COLUMN host_dynamic;
                 ALTER TABLE skills ADD COLUMN host_secret TEXT;",
            )
            .expect("add host extension columns");
        let selections = skill_catalog_columns()
            .map(|column| (column, false))
            .collect::<Vec<_>>();

        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin raw insert");
        assert_eq!(
            insert_skill_catalog_if_absent(
                &mut transaction,
                "host-skill",
                "Host Skill",
                Some("private description"),
                "../.host-accepted",
                selections.iter().copied(),
            )
            .expect("insert host path"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit raw insert");
        connection
            .execute(
                "UPDATE skills SET host_secret = 'host-token-value'
                 WHERE id = 'host-skill'",
                [],
            )
            .expect("set private host extension");
        let host_row = read_skill_catalog_row(&connection, "host-skill")
            .expect("read host Skill")
            .expect("host Skill exists");
        for debug in [
            format!("{host_row:?}"),
            format!("{:?}", catalog_values(&host_row)),
        ] {
            assert!(!debug.contains("host-token-value"));
            assert!(!debug.contains("private description"));
            assert!(!debug.contains("../.host-accepted"));
        }
        assert_eq!(catalog_values(&host_row).directory, "../.host-accepted");

        connection
            .execute_batch(
                "INSERT INTO skills (id, name, directory)
                 VALUES (NULL, 'Legacy', 'legacy');
                 INSERT INTO skills (id, name, directory)
                 VALUES (x'80', 'Blob', 'blob');
                 INSERT INTO skills (id, name, directory)
                 VALUES (CAST(x'81' AS TEXT), 'Invalid UTF-8', 'invalid-utf8');
                 INSERT INTO skills (
                     id, name, description, directory, enabled_claude
                 ) VALUES (
                     'bad-description', 'Bad Description', x'82', 'bad-description', x'82'
                 );
                 INSERT INTO skills (id, name, directory, enabled_grokbuild)
                 VALUES ('bad-name', CAST(x'83' AS TEXT), 'bad-name', 1);
                 INSERT INTO skills (id, name, directory)
                 VALUES ('bad-directory', 'Bad Directory', x'84');
                 INSERT INTO skills (id, name, directory, enabled_claude)
                 VALUES ('bad-selection', 'Bad Selection', 'bad-selection', x'85');
                 PRAGMA count_changes = ON;",
            )
            .expect("insert malformed rows and enable count_changes");
        let bad_name = read_skill_catalog_row(&connection, "bad-name")
            .expect("read malformed display row")
            .expect("malformed display row exists");
        assert!(bad_name.values().is_none());
        assert_eq!(bad_name.selected_for(&AppType::GrokBuild), Some(true));
        let bad_selection = read_skill_catalog_row(&connection, "bad-selection")
            .expect("read malformed selection row")
            .expect("malformed selection row exists");
        assert!(bad_selection.values().is_none());
        assert_eq!(bad_selection.selected_for(&AppType::Claude), None);
        assert_eq!(bad_selection.selected_for(&AppType::GrokBuild), Some(false));

        let host = read_skill_catalog_row(&connection, "host-skill")
            .expect("read host row beside malformed rows")
            .expect("host row exists");
        let host_values = catalog_values(&host);
        let mut host_selections = host_values.selections().collect::<Vec<_>>();
        host_selections
            .iter_mut()
            .find(|(column, _)| column.as_str() == "enabled_pi")
            .expect("Pi selection")
            .1 = true;
        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin valid update beside malformed rows");
        assert_eq!(
            update_skill_catalog_if_unchanged(
                &mut transaction,
                &host,
                host_values.id.as_str(),
                &host_values.name,
                host_values.description.as_deref(),
                &host_values.directory,
                host_selections,
            )
            .expect("update valid row beside malformed rows"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit valid update");

        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin valid insert beside malformed rows");
        assert_eq!(
            insert_skill_catalog_if_absent(
                &mut transaction,
                "valid-beside-malformed",
                "Valid",
                None,
                "valid",
                selections.iter().copied(),
            )
            .expect("insert beside malformed rows"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit valid insert");
        let valid = read_skill_catalog_row(&connection, "valid-beside-malformed")
            .expect("read valid row")
            .expect("valid row exists");
        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin valid delete beside malformed rows");
        assert_eq!(
            delete_skill_catalog_if_unchanged(&mut transaction, &valid)
                .expect("delete valid row beside malformed rows"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit valid delete");

        let malformed = read_skill_catalog_row(&connection, "bad-description")
            .expect("read repairable row")
            .expect("repairable row exists");
        assert!(malformed.values().is_none());
        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin malformed repair");
        assert_eq!(
            update_skill_catalog_if_unchanged(
                &mut transaction,
                &malformed,
                "bad-description",
                "Bad Description",
                None,
                "bad-description",
                selections.iter().copied(),
            )
            .expect("repair malformed shared fields"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit malformed repair");
        assert!(read_skill_catalog_row(&connection, "bad-description")
            .expect("read repaired row")
            .expect("repaired row exists")
            .values()
            .is_some());

        for id in [
            "bad-description",
            "bad-name",
            "bad-directory",
            "bad-selection",
        ] {
            let malformed = read_skill_catalog_row(&connection, id)
                .expect("read malformed row")
                .expect("malformed row exists");
            let mut transaction = crate::begin_immediate_transaction(&mut connection)
                .expect("begin malformed delete");
            assert_eq!(
                delete_skill_catalog_if_unchanged(&mut transaction, &malformed)
                    .expect("delete malformed row"),
                SkillCatalogWriteOutcome::Applied
            );
            transaction.commit().expect("commit malformed delete");
        }
        for _ in 0..3 {
            let legacy = read_skill_catalog_rows(&connection)
                .expect("read raw catalog")
                .into_iter()
                .find(|row| row.values().is_none())
                .expect("legacy row exists");
            assert!(legacy.id().is_none());
            let mut transaction =
                crate::begin_immediate_transaction(&mut connection).expect("begin legacy delete");
            assert_eq!(
                delete_skill_catalog_if_unchanged(&mut transaction, &legacy)
                    .expect("delete legacy row"),
                SkillCatalogWriteOutcome::Applied
            );
            transaction.commit().expect("commit legacy delete");
        }
        connection
            .pragma_update(None, "count_changes", false)
            .expect("disable count_changes");
        let remaining = read_skill_catalog_rows(&connection).expect("read remaining catalog");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id(), Some("host-skill"));

        connection
            .execute_batch(
                "INSERT INTO skills (id, name, directory) VALUES (NULL, 'Null A', 'null-a');
                 INSERT INTO skills (id, name, directory) VALUES (NULL, 'Null B', 'null-b');",
            )
            .expect("insert ambiguous NULL identities");
        let null_row = read_skill_catalog_rows(&connection)
            .expect("read ambiguous rows")
            .into_iter()
            .find(|row| raw_text(row, 1) == Some("Null A"))
            .expect("NULL row exists");
        let host = read_skill_catalog_row(&connection, "host-skill")
            .expect("read valid row beside NULL identities")
            .expect("valid row exists");
        let host_values = catalog_values(&host);
        let mut replacements = host_values.selections().collect::<Vec<_>>();
        replacements
            .iter_mut()
            .find(|(column, _)| column.as_str() == "enabled_claude")
            .expect("Claude selection")
            .1 = true;
        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin valid update beside NULL identities");
        assert_eq!(
            update_skill_catalog_if_unchanged(
                &mut transaction,
                &host,
                host.id().expect("valid host id"),
                &host_values.name,
                host_values.description.as_deref(),
                &host_values.directory,
                replacements,
            )
            .expect("update valid row"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit valid update");

        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin ambiguous delete");
        assert_eq!(
            delete_skill_catalog_if_unchanged(&mut transaction, &null_row)
                .expect("delete one NULL row by its complete snapshot"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit snapshot delete");
        assert_eq!(
            read_skill_catalog_rows(&connection)
                .expect("read remaining NULL row")
                .into_iter()
                .filter(|row| row.values().is_none())
                .count(),
            1
        );

        connection
            .execute(
                "INSERT INTO skills (id, name, directory) VALUES (NULL, 'Null B', 'null-b')",
                [],
            )
            .expect("insert physically indistinguishable NULL row");
        let duplicate = read_skill_catalog_rows(&connection)
            .expect("read duplicate NULL rows")
            .into_iter()
            .find(|row| raw_text(row, 1) == Some("Null B"))
            .expect("duplicate snapshot exists");
        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin duplicate snapshot delete");
        assert_eq!(
            delete_skill_catalog_if_unchanged(&mut transaction, &duplicate)
                .expect("delete exactly one indistinguishable row"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit duplicate delete");
        assert_eq!(
            read_skill_catalog_rows(&connection)
                .expect("read one remaining duplicate")
                .into_iter()
                .filter(|row| raw_text(row, 1) == Some("Null B"))
                .count(),
            1
        );

        connection
            .execute_batch(
                "INSERT INTO skills (id, name, directory, host_dynamic)
                 VALUES (NULL, 'Dynamic', 'dynamic', 1);
                 INSERT INTO skills (id, name, directory, host_dynamic)
                 VALUES (NULL, 'Dynamic', 'dynamic', 1.0);",
            )
            .expect("insert rows distinguished by SQLite storage class");
        let dynamic = read_skill_catalog_rows(&connection)
            .expect("read storage-class rows")
            .into_iter()
            .find(|row| raw_text(row, 1) == Some("Dynamic"))
            .expect("storage-class row exists");
        let mut transaction = crate::begin_immediate_transaction(&mut connection)
            .expect("begin storage-class delete");
        assert_eq!(
            delete_skill_catalog_if_unchanged(&mut transaction, &dynamic)
                .expect("delete exactly one storage-class row"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit storage-class delete");
        assert_eq!(
            read_skill_catalog_rows(&connection)
                .expect("read remaining storage-class row")
                .into_iter()
                .filter(|row| raw_text(row, 1) == Some("Dynamic"))
                .count(),
            1
        );

        connection
            .execute_batch(
                "INSERT INTO skills (id, name, directory, host_dynamic)
                 VALUES (NULL, 'Signed Zero', 'signed-zero', -0.0);
                 INSERT INTO skills (id, name, directory, host_dynamic)
                 VALUES (NULL, 'Signed Zero', 'signed-zero', 0.0);",
            )
            .expect("insert SQLite-equivalent real rows");
        let signed_zero = read_skill_catalog_rows(&connection)
            .expect("read signed-zero rows")
            .into_iter()
            .filter(|row| raw_text(row, 1) == Some("Signed Zero"))
            .nth(1)
            .expect("second signed-zero row exists");
        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin signed-zero delete");
        assert_eq!(
            delete_skill_catalog_if_unchanged(&mut transaction, &signed_zero)
                .expect("delete one SQLite-equivalent real row"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit signed-zero delete");
        assert_eq!(
            read_skill_catalog_rows(&connection)
                .expect("read remaining signed-zero row")
                .into_iter()
                .filter(|row| raw_text(row, 1) == Some("Signed Zero"))
                .count(),
            1
        );
    }

    #[test]
    fn raw_catalog_cas_uses_binary_comparison_with_nocase_columns() {
        let (_directory, database) = test_database();
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "CREATE TABLE skills (
                    id TEXT COLLATE NOCASE,
                    name TEXT NOT NULL,
                    description TEXT,
                    directory TEXT NOT NULL,
                    PRIMARY KEY (id COLLATE BINARY)
                 );",
            )
            .expect("create host schema");
        ensure_skill_schema(&mut connection).expect("upgrade host schema");
        connection
            .execute_batch(
                "INSERT INTO skills (id, name, directory) VALUES ('A', 'Upper', 'upper');
                 INSERT INTO skills (id, name, directory) VALUES ('a', 'Lower', 'lower');",
            )
            .expect("insert binary-distinct rows");

        let upper = read_skill_catalog_row(&connection, "A")
            .expect("read upper row")
            .expect("upper row exists");
        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin binary delete");
        assert_eq!(
            delete_skill_catalog_if_unchanged(&mut transaction, &upper)
                .expect("delete exact binary row"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit binary delete");
        assert!(read_skill_catalog_row(&connection, "A")
            .expect("read removed upper row")
            .is_none());
        assert!(read_skill_catalog_row(&connection, "a")
            .expect("read preserved lower row")
            .is_some());
    }

    #[test]
    fn catalog_delete_allows_host_cleanup_and_rejects_other_skill_writes() {
        let (_directory, database) = test_database();
        database.ensure_skill_schema().expect("initialize skills");
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "CREATE TABLE host_skill_bindings (skill_id TEXT NOT NULL);
                 CREATE TRIGGER clean_host_skill_binding AFTER DELETE ON skills BEGIN
                    DELETE FROM host_skill_bindings WHERE skill_id = OLD.id;
                 END;",
            )
            .expect("create cleanup contract");

        for id in ["demo", "other"] {
            let entry = catalog_entry(id, id, id, &[]);
            let mut transaction =
                crate::begin_immediate_transaction(&mut connection).expect("begin insert");
            assert_eq!(
                insert_catalog_entry(&mut transaction, &entry).expect("insert Skill"),
                SkillCatalogWriteOutcome::Applied
            );
            transaction.commit().expect("commit insert");
        }
        connection
            .execute("INSERT INTO host_skill_bindings VALUES ('demo')", [])
            .expect("insert host binding");
        let demo = read_skill_catalog_row(&connection, "demo")
            .expect("read Skill")
            .expect("Skill exists");
        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin cleanup delete");
        assert_eq!(
            delete_skill_catalog_if_unchanged(&mut transaction, &demo).expect("delete Skill"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit cleanup delete");
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM host_skill_bindings", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("count host bindings"),
            0
        );

        let entry = catalog_entry("demo", "demo", "demo", &[]);
        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin reinsert");
        assert_eq!(
            insert_catalog_entry(&mut transaction, &entry).expect("reinsert Skill"),
            SkillCatalogWriteOutcome::Applied
        );
        transaction.commit().expect("commit reinsert");
        connection
            .execute_batch(
                "INSERT INTO host_skill_bindings VALUES ('demo');
                 CREATE TRIGGER rewrite_other_skill AFTER DELETE ON skills
                 WHEN OLD.id = 'demo' BEGIN
                    UPDATE skills SET name = 'rewritten' WHERE id = 'other';
                 END;",
            )
            .expect("create invalid catalog trigger");
        let demo = read_skill_catalog_row(&connection, "demo")
            .expect("read Skill")
            .expect("Skill exists");
        let mut transaction =
            crate::begin_immediate_transaction(&mut connection).expect("begin rejected delete");
        assert_eq!(
            delete_skill_catalog_if_unchanged(&mut transaction, &demo)
                .expect("reject other-row rewrite"),
            SkillCatalogWriteOutcome::NotApplied
        );
        transaction.commit().expect("commit rejected delete");
        assert!(read_skill_catalog_row(&connection, "demo")
            .expect("read preserved Skill")
            .is_some());
        assert_eq!(
            read_skill_catalog_row(&connection, "other")
                .expect("read other Skill")
                .expect("other Skill exists")
                .values()
                .expect("valid other Skill")
                .name,
            "other"
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM host_skill_bindings", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("count preserved bindings"),
            1
        );
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
            .execute_batch(
                "ALTER TABLE skills ADD COLUMN repo_owner TEXT;
                 ALTER TABLE skills ADD COLUMN host_note TEXT;",
            )
            .expect("add host columns");
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
                        id, name, description, directory, repo_owner,
                        enabled_claude, enabled_codex, enabled_gemini, enabled_grokbuild,
                        enabled_opencode, enabled_hermes, enabled_pi, host_note
                    ) VALUES (
                        NEW.id, NEW.name, NEW.description, NEW.directory, NEW.repo_owner,
                        NEW.enabled_claude, NEW.enabled_codex, NEW.enabled_gemini, NEW.enabled_grokbuild,
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
