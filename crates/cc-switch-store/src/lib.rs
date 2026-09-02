//! Shared SQLite foundations for CC Switch hosts.
//!
//! Hosts choose the database path and retain ownership of product migrations,
//! extension tables, and business rules. This crate safely opens that path and
//! can initialize the canonical `providers` table needed by native products.

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior};
use thiserror::Error;

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Canonical table shared by CC Switch products.
pub const PROVIDERS_TABLE: &str = "providers";

const SELECT_PROVIDER_COLUMNS: &str = "id, app_type, name, settings_config,
    website_url, category, created_at, sort_index, notes, icon, icon_color,
    meta, is_current, in_failover_queue";

/// An unparsed row from the shared `providers` table.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderRow {
    pub id: String,
    pub app_type: String,
    pub name: String,
    pub settings_config: String,
    pub website_url: Option<String>,
    pub category: Option<String>,
    pub created_at: Option<i64>,
    pub sort_index: Option<i64>,
    pub notes: Option<String>,
    pub icon: Option<String>,
    pub icon_color: Option<String>,
    pub meta: String,
    pub is_current: i64,
    pub in_failover_queue: i64,
}

impl fmt::Debug for ProviderRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRow")
            .field("id", &self.id)
            .field("app_type", &self.app_type)
            .field("name", &self.name)
            .field("settings_config", &"<redacted>")
            .field("website_url", &self.website_url)
            .field("category", &self.category)
            .field("created_at", &self.created_at)
            .field("sort_index", &self.sort_index)
            .field("notes", &self.notes)
            .field("icon", &self.icon)
            .field("icon_color", &self.icon_color)
            .field("meta", &"<redacted>")
            .field("is_current", &self.is_current)
            .field("in_failover_queue", &self.in_failover_queue)
            .finish()
    }
}

const CREATE_PROVIDERS_TABLE: &str = "CREATE TABLE IF NOT EXISTS providers (
    id TEXT NOT NULL,
    app_type TEXT NOT NULL,
    name TEXT NOT NULL,
    settings_config TEXT NOT NULL,
    website_url TEXT,
    category TEXT,
    created_at INTEGER,
    sort_index INTEGER,
    notes TEXT,
    icon TEXT,
    icon_color TEXT,
    meta TEXT NOT NULL DEFAULT '{}',
    is_current BOOLEAN NOT NULL DEFAULT 0,
    in_failover_queue BOOLEAN NOT NULL DEFAULT 0,
    PRIMARY KEY (id, app_type)
)";

const BASE_PROVIDER_COLUMNS: &[&str] = &["id", "app_type", "name", "settings_config"];

const PROVIDER_COLUMNS: &[ProviderColumn] = &[
    ProviderColumn::required_text("id", 1),
    ProviderColumn::required_text("app_type", 2),
    ProviderColumn::required_text("name", 0),
    ProviderColumn::required_text("settings_config", 0),
    ProviderColumn::optional("website_url", "TEXT"),
    ProviderColumn::optional("category", "TEXT"),
    ProviderColumn::optional("created_at", "INTEGER"),
    ProviderColumn::optional("sort_index", "INTEGER"),
    ProviderColumn::optional("notes", "TEXT"),
    ProviderColumn::optional("icon", "TEXT"),
    ProviderColumn::optional("icon_color", "TEXT"),
    ProviderColumn::defaulted("meta", "TEXT", "TEXT NOT NULL DEFAULT '{}'", "'{}'"),
    ProviderColumn::defaulted("is_current", "BOOLEAN", "BOOLEAN NOT NULL DEFAULT 0", "0"),
    ProviderColumn::defaulted(
        "in_failover_queue",
        "BOOLEAN",
        "BOOLEAN NOT NULL DEFAULT 0",
        "0",
    ),
];

#[derive(Clone, Copy)]
struct ProviderColumn {
    name: &'static str,
    declared_type: &'static str,
    declaration: &'static str,
    not_null: bool,
    default: Option<&'static str>,
    primary_key: i64,
}

impl ProviderColumn {
    const fn required_text(name: &'static str, primary_key: i64) -> Self {
        Self {
            name,
            declared_type: "TEXT",
            declaration: "TEXT NOT NULL",
            not_null: true,
            default: None,
            primary_key,
        }
    }

    const fn optional(name: &'static str, declared_type: &'static str) -> Self {
        Self {
            name,
            declared_type,
            declaration: declared_type,
            not_null: false,
            default: None,
            primary_key: 0,
        }
    }

    const fn defaulted(
        name: &'static str,
        declared_type: &'static str,
        declaration: &'static str,
        default: &'static str,
    ) -> Self {
        Self {
            name,
            declared_type,
            declaration,
            not_null: true,
            default: Some(default),
            primary_key: 0,
        }
    }
}

struct ExistingColumn {
    name: String,
    declared_type: String,
    not_null: bool,
    default: Option<String>,
    primary_key: i64,
    hidden: i64,
}

#[cfg(windows)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct WindowsFileIdentity {
    volume: u32,
    index: u64,
}

/// Failure while opening or validating the shared database.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SharedStoreError {
    #[error("shared database I/O failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("shared database failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("shared database is invalid: {0}")]
    InvalidDatabase(String),
}

/// A shared database path whose file safety is checked before each connection.
#[derive(Debug, Clone)]
pub struct SharedDatabase {
    path: PathBuf,
}

impl SharedDatabase {
    /// Prepares a database file without choosing a product-specific path or
    /// changing its schema.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SharedStoreError> {
        let path = resolve_database_path(path.into())?;
        let database = Self { path };
        drop(database.prepare_regular_file(true)?);
        Ok(database)
    }

    /// Returns the database path with its existing parent directory resolved.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opens a new configured connection to the prepared database.
    ///
    /// The file is checked immediately before and after SQLite opens it. This
    /// does not isolate a host-selected directory from another process that is
    /// already allowed to replace files in that directory.
    pub fn connect(&self) -> Result<Connection, SharedStoreError> {
        let prepared_file = self.prepare_regular_file(false)?;
        let connection = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        self.verify_prepared_file(&prepared_file)?;
        connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(connection)
    }

    /// Creates or transactionally upgrades the shared `providers` contract.
    ///
    /// Product migrations may run through [`Self::connect`] before calling this
    /// method. Unknown tables, columns, and rows are retained. Additional host
    /// constraints, indexes, triggers, and conflict policies are not interpreted.
    pub fn ensure_provider_schema(&self) -> Result<(), SharedStoreError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(CREATE_PROVIDERS_TABLE, [])?;

        let initial_columns = provider_columns(&transaction)?;
        verify_base_provider_schema(&initial_columns)?;
        for expected in PROVIDER_COLUMNS {
            if !initial_columns
                .iter()
                .any(|column| column.name == expected.name)
            {
                transaction.execute_batch(&format!(
                    "ALTER TABLE providers ADD COLUMN {} {}",
                    expected.name, expected.declaration
                ))?;
            }
        }
        verify_provider_schema(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    #[cfg(unix)]
    fn prepare_regular_file(&self, create: bool) -> Result<File, SharedStoreError> {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(SharedStoreError::InvalidDatabase(
                    "database must not be a symbolic link".to_owned(),
                ));
            }
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(SharedStoreError::InvalidDatabase(
                    "database must be a regular file".to_owned(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {}
            Err(source) => {
                return Err(SharedStoreError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        }

        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.path)
            .map_err(|source| SharedStoreError::Io {
                path: self.path.clone(),
                source,
            })?;
        let metadata = file.metadata().map_err(|source| SharedStoreError::Io {
            path: self.path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(SharedStoreError::InvalidDatabase(
                "database must be a regular file".to_owned(),
            ));
        }
        if metadata.nlink() != 1 {
            return Err(SharedStoreError::InvalidDatabase(
                "database must not have additional hard links".to_owned(),
            ));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| SharedStoreError::Io {
                path: self.path.clone(),
                source,
            })?;
        Ok(file)
    }

    #[cfg(not(unix))]
    fn prepare_regular_file(&self, create: bool) -> Result<File, SharedStoreError> {
        let file = match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(SharedStoreError::InvalidDatabase(
                    "database must not be a symbolic link".to_owned(),
                ));
            }
            Ok(metadata) if metadata.is_file() => OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.path)
                .map_err(|source| SharedStoreError::Io {
                    path: self.path.clone(),
                    source,
                })?,
            Ok(_) => {
                return Err(SharedStoreError::InvalidDatabase(
                    "database must be a regular file".to_owned(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&self.path)
                {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        return self.prepare_regular_file(false);
                    }
                    Err(source) => {
                        return Err(SharedStoreError::Io {
                            path: self.path.clone(),
                            source,
                        });
                    }
                }
            }
            Err(source) => {
                return Err(SharedStoreError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        if !file
            .metadata()
            .map_err(|source| SharedStoreError::Io {
                path: self.path.clone(),
                source,
            })?
            .is_file()
        {
            return Err(SharedStoreError::InvalidDatabase(
                "database must be a regular file".to_owned(),
            ));
        }
        #[cfg(windows)]
        self.windows_file_identity(&file)?;
        Ok(file)
    }

    #[cfg(unix)]
    fn verify_prepared_file(&self, prepared_file: &File) -> Result<(), SharedStoreError> {
        use std::os::unix::fs::MetadataExt;

        let prepared = prepared_file
            .metadata()
            .map_err(|source| SharedStoreError::Io {
                path: self.path.clone(),
                source,
            })?;
        let current = fs::symlink_metadata(&self.path).map_err(|source| SharedStoreError::Io {
            path: self.path.clone(),
            source,
        })?;
        if current.file_type().is_symlink()
            || !current.is_file()
            || prepared.dev() != current.dev()
            || prepared.ino() != current.ino()
            || current.nlink() != 1
        {
            return Err(SharedStoreError::InvalidDatabase(
                "database file changed while it was being opened".to_owned(),
            ));
        }
        Ok(())
    }

    #[cfg(windows)]
    fn verify_prepared_file(&self, prepared_file: &File) -> Result<(), SharedStoreError> {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        let current = fs::symlink_metadata(&self.path).map_err(|source| SharedStoreError::Io {
            path: self.path.clone(),
            source,
        })?;
        if current.file_type().is_symlink()
            || !current.is_file()
            || current.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(SharedStoreError::InvalidDatabase(
                "database file changed while it was being opened".to_owned(),
            ));
        }
        let current_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|source| SharedStoreError::Io {
                path: self.path.clone(),
                source,
            })?;
        if self.windows_file_identity(prepared_file)?
            != self.windows_file_identity(&current_file)?
        {
            return Err(SharedStoreError::InvalidDatabase(
                "database file changed while it was being opened".to_owned(),
            ));
        }
        Ok(())
    }

    #[cfg(windows)]
    fn windows_file_identity(&self, file: &File) -> Result<WindowsFileIdentity, SharedStoreError> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
        };

        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `file` owns a valid handle for this call, and `information`
        // points to writable storage of the exact structure Windows expects.
        let succeeded =
            unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) };
        if succeeded == 0 {
            return Err(SharedStoreError::Io {
                path: self.path.clone(),
                source: std::io::Error::last_os_error(),
            });
        }
        if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(SharedStoreError::InvalidDatabase(
                "database must not be a reparse point".to_owned(),
            ));
        }
        if information.nNumberOfLinks != 1 {
            return Err(SharedStoreError::InvalidDatabase(
                "database must not have additional hard links".to_owned(),
            ));
        }
        Ok(WindowsFileIdentity {
            volume: information.dwVolumeSerialNumber,
            index: (u64::from(information.nFileIndexHigh) << 32)
                | u64::from(information.nFileIndexLow),
        })
    }

    #[cfg(all(not(unix), not(windows)))]
    fn verify_prepared_file(&self, _prepared_file: &File) -> Result<(), SharedStoreError> {
        let current = fs::symlink_metadata(&self.path).map_err(|source| SharedStoreError::Io {
            path: self.path.clone(),
            source,
        })?;
        if current.file_type().is_symlink() || !current.is_file() {
            return Err(SharedStoreError::InvalidDatabase(
                "database file changed while it was being opened".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Reads shared provider rows in the ordering used by CC Switch products.
pub fn read_provider_rows(
    connection: &Connection,
    app_type: Option<&str>,
) -> Result<Vec<ProviderRow>, SharedStoreError> {
    let sql = format!(
        "SELECT {SELECT_PROVIDER_COLUMNS} FROM providers
         WHERE (?1 IS NULL OR app_type = ?1)
         ORDER BY COALESCE(sort_index, 999999), created_at ASC, id ASC, app_type ASC"
    );
    let mut statement = connection.prepare(&sql)?;
    let providers = statement
        .query_map([app_type], provider_from_row)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(SharedStoreError::from)?;
    Ok(providers)
}

/// Reads one shared provider row by its composite identity.
pub fn read_provider_row(
    connection: &Connection,
    id: &str,
    app_type: &str,
) -> Result<Option<ProviderRow>, SharedStoreError> {
    let sql = format!(
        "SELECT {SELECT_PROVIDER_COLUMNS} FROM providers
         WHERE id = ?1 AND app_type = ?2"
    );
    connection
        .query_row(&sql, [id, app_type], provider_from_row)
        .optional()
        .map_err(SharedStoreError::from)
}

fn provider_from_row(row: &Row<'_>) -> Result<ProviderRow, rusqlite::Error> {
    Ok(ProviderRow {
        id: row.get(0)?,
        app_type: row.get(1)?,
        name: row.get(2)?,
        settings_config: row.get(3)?,
        website_url: row.get(4)?,
        category: row.get(5)?,
        created_at: row.get(6)?,
        sort_index: row.get(7)?,
        notes: row.get(8)?,
        icon: row.get(9)?,
        icon_color: row.get(10)?,
        meta: row.get(11)?,
        is_current: row.get(12)?,
        in_failover_queue: row.get(13)?,
    })
}

fn resolve_database_path(path: PathBuf) -> Result<PathBuf, SharedStoreError> {
    let file_name = path.file_name().map(ToOwned::to_owned).ok_or_else(|| {
        SharedStoreError::InvalidDatabase("database path has no file name".to_owned())
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| SharedStoreError::Io {
        path: parent.to_owned(),
        source,
    })?;
    let parent = fs::canonicalize(parent).map_err(|source| SharedStoreError::Io {
        path: parent.to_owned(),
        source,
    })?;
    Ok(parent.join(file_name))
}

fn provider_columns(connection: &Connection) -> Result<Vec<ExistingColumn>, SharedStoreError> {
    let object_type = connection
        .query_row(
            "SELECT type FROM sqlite_schema WHERE name = 'providers'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if object_type.as_deref() != Some("table") {
        return Err(SharedStoreError::InvalidDatabase(
            "providers must be a table".to_owned(),
        ));
    }

    let mut statement = connection.prepare("PRAGMA table_xinfo(providers)")?;
    let columns = statement
        .query_map([], |row| {
            Ok(ExistingColumn {
                name: row.get(1)?,
                declared_type: row.get(2)?,
                not_null: row.get::<_, i64>(3)? != 0,
                default: row.get(4)?,
                primary_key: row.get(5)?,
                hidden: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(SharedStoreError::from)?;
    Ok(columns)
}

fn verify_base_provider_schema(columns: &[ExistingColumn]) -> Result<(), SharedStoreError> {
    for required in BASE_PROVIDER_COLUMNS {
        if !columns.iter().any(|column| column.name == *required) {
            return Err(SharedStoreError::InvalidDatabase(format!(
                "providers table is missing required column '{required}'"
            )));
        }
    }
    Ok(())
}

fn verify_provider_schema(connection: &Connection) -> Result<(), SharedStoreError> {
    let columns = provider_columns(connection)?;
    let mut primary_key = columns
        .iter()
        .filter(|column| column.primary_key > 0)
        .collect::<Vec<_>>();
    primary_key.sort_by_key(|column| column.primary_key);
    if primary_key
        .iter()
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>()
        != ["id", "app_type"]
    {
        return Err(SharedStoreError::InvalidDatabase(
            "providers primary key must be exactly (id, app_type)".to_owned(),
        ));
    }
    verify_provider_primary_key_index(connection)?;
    for expected in PROVIDER_COLUMNS {
        let actual = columns
            .iter()
            .find(|column| column.name == expected.name)
            .ok_or_else(|| {
                SharedStoreError::InvalidDatabase(format!(
                    "providers table is missing required column '{}'",
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
                "providers column '{}' does not match the shared contract",
                expected.name
            )));
        }
    }
    Ok(())
}

fn verify_provider_primary_key_index(connection: &Connection) -> Result<(), SharedStoreError> {
    let primary_key_index = connection
        .query_row(
            "SELECT name FROM pragma_index_list('providers') WHERE origin = 'pk' AND partial = 0",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            SharedStoreError::InvalidDatabase(
                "providers table has no canonical primary key index".to_owned(),
            )
        })?;
    let mut statement = connection.prepare(
        "SELECT name, \"desc\", coll FROM pragma_index_xinfo(?1)
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
    let canonical = [
        (Some("id".to_owned()), 0, "BINARY".to_owned()),
        (Some("app_type".to_owned()), 0, "BINARY".to_owned()),
    ];
    if indexed_columns != canonical {
        return Err(SharedStoreError::InvalidDatabase(
            "providers primary key must use binary (id, app_type) ordering".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn new_database_has_the_canonical_provider_contract() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("cc-switch.db");
        let database = SharedDatabase::open(&path).expect("open shared database");
        database
            .ensure_provider_schema()
            .expect("initialize provider schema");
        let connection = database.connect().expect("connect shared database");
        let columns = connection
            .prepare("PRAGMA table_xinfo(providers)")
            .expect("prepare table inspection")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("inspect provider columns")
            .collect::<Result<HashSet<_>, _>>()
            .expect("read provider columns");

        for expected in [
            "id",
            "app_type",
            "name",
            "settings_config",
            "website_url",
            "category",
            "created_at",
            "sort_index",
            "notes",
            "icon",
            "icon_color",
            "meta",
            "is_current",
            "in_failover_queue",
        ] {
            assert!(columns.contains(expected), "missing {expected}");
        }
        assert_eq!(
            database.path(),
            directory
                .path()
                .canonicalize()
                .expect("resolve temporary directory")
                .join("cc-switch.db")
        );
        assert_eq!(
            connection
                .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
                .expect("read foreign_keys pragma"),
            1
        );
        assert_eq!(
            connection
                .query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))
                .expect("read busy_timeout pragma"),
            5_000
        );
    }

    #[test]
    fn provider_reads_are_raw_filtered_and_stably_ordered() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        database
            .ensure_provider_schema()
            .expect("initialize provider schema");
        let connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "INSERT INTO providers
                    (id, app_type, name, settings_config, created_at, sort_index, meta,
                     is_current, in_failover_queue)
                 VALUES
                    ('later', 'claude', 'Later', 'secret-settings', 1, 2,
                     'secret-meta', 7, -2),
                    ('first', 'claude', 'First', '{}', 2, 1, '{}', 0, 0),
                    ('other', 'codex', 'Other', '{}', 0, 0, '{}', 0, 0),
                    ('first', 'codex', 'Duplicate', '{}', 2, 1, '{}', 0, 0)",
            )
            .expect("insert provider fixtures");

        let rows = read_provider_rows(&connection, Some("claude")).expect("read providers");
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["first", "later"]
        );
        assert_eq!(rows[1].settings_config, "secret-settings");
        assert_eq!(rows[1].is_current, 7);
        assert_eq!(rows[1].in_failover_queue, -2);
        let debug = format!("{:?}", rows[1]);
        assert!(!debug.contains("secret-settings"));
        assert!(!debug.contains("secret-meta"));
        assert_eq!(
            read_provider_row(&connection, "first", "claude")
                .expect("read provider")
                .expect("provider exists"),
            rows[0]
        );
        assert!(read_provider_row(&connection, "missing", "codex")
            .expect("read missing provider")
            .is_none());
        assert_eq!(
            read_provider_rows(&connection, None)
                .expect("read all providers")
                .iter()
                .map(|row| (row.id.as_str(), row.app_type.as_str()))
                .collect::<Vec<_>>(),
            [
                ("other", "codex"),
                ("first", "claude"),
                ("first", "codex"),
                ("later", "claude"),
            ]
        );
    }

    #[test]
    fn existing_product_data_and_extensions_are_preserved() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("cc-switch.db");
        let connection = Connection::open(&path).expect("create fixture database");
        connection
            .execute_batch(
                "CREATE TABLE providers (
                    id TEXT NOT NULL, app_type TEXT NOT NULL, name TEXT NOT NULL,
                    settings_config TEXT NOT NULL, category TEXT, created_at INTEGER,
                    sort_index INTEGER, notes TEXT, icon_color TEXT,
                    meta TEXT NOT NULL DEFAULT '{}',
                    is_current BOOLEAN NOT NULL DEFAULT 0,
                    future_column TEXT,
                    PRIMARY KEY (id, app_type)
                );
                CREATE TABLE host_extension (value TEXT NOT NULL);
                INSERT INTO providers
                    (id, app_type, name, settings_config, meta, future_column)
                    VALUES ('p', 'claude', 'Provider', '{}', '{}', 'keep');
                INSERT INTO host_extension VALUES ('keep');",
            )
            .expect("create compatible fixture");
        drop(connection);

        let database = SharedDatabase::open(&path).expect("open compatible database");
        database
            .ensure_provider_schema()
            .expect("upgrade provider schema");
        let connection = database.connect().expect("connect compatible database");
        assert_eq!(
            connection
                .query_row(
                    "SELECT future_column FROM providers WHERE id = 'p'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read future provider column"),
            "keep"
        );
        assert_eq!(
            connection
                .query_row("SELECT value FROM host_extension", [], |row| {
                    row.get::<_, String>(0)
                })
                .expect("read host extension"),
            "keep"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT in_failover_queue FROM providers WHERE id = 'p'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read upgraded provider column"),
            0
        );
    }

    #[test]
    fn incompatible_provider_table_is_rejected_without_partial_changes() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("cc-switch.db");
        Connection::open(&path)
            .expect("create fixture database")
            .execute_batch(
                "CREATE TABLE providers (
                    id TEXT NOT NULL,
                    app_type TEXT NOT NULL,
                    name TEXT NOT NULL,
                    settings_config TEXT NOT NULL,
                    icon TEXT GENERATED ALWAYS AS (name) VIRTUAL,
                    PRIMARY KEY (id, app_type)
                )",
            )
            .expect("create incompatible table");

        let database = SharedDatabase::open(&path).expect("prepare incompatible database");
        assert!(matches!(
            database.ensure_provider_schema(),
            Err(SharedStoreError::InvalidDatabase(message))
                if message.contains("does not match")
        ));
        let connection = Connection::open(path).expect("reopen incompatible table");
        let columns = connection
            .prepare("PRAGMA table_xinfo(providers)")
            .expect("prepare incompatible table inspection")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("inspect incompatible table")
            .collect::<Result<HashSet<_>, _>>()
            .expect("read incompatible table columns");
        assert_eq!(
            columns,
            HashSet::from([
                "id".to_owned(),
                "app_type".to_owned(),
                "name".to_owned(),
                "settings_config".to_owned(),
                "icon".to_owned(),
            ])
        );
    }

    #[test]
    fn noncanonical_keys_and_defaults_are_rejected() {
        for schema in [
            CREATE_PROVIDERS_TABLE
                .replace("icon_color TEXT,", "icon_color TEXT, tenant TEXT NOT NULL,")
                .replace(
                    "PRIMARY KEY (id, app_type)",
                    "PRIMARY KEY (id, app_type, tenant)",
                ),
            CREATE_PROVIDERS_TABLE.replace("notes TEXT,", "notes TEXT DEFAULT 'unexpected',"),
            CREATE_PROVIDERS_TABLE.replace("id TEXT NOT NULL,", "id TEXT COLLATE NOCASE NOT NULL,"),
        ] {
            let directory = tempfile::tempdir().expect("temporary directory");
            let path = directory.path().join("cc-switch.db");
            Connection::open(&path)
                .expect("create fixture database")
                .execute_batch(&schema)
                .expect("create noncanonical table");
            let database = SharedDatabase::open(path).expect("prepare noncanonical database");
            assert!(matches!(
                database.ensure_provider_schema(),
                Err(SharedStoreError::InvalidDatabase(_))
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn database_file_is_private_and_symbolic_links_are_rejected() {
        use std::os::unix::{fs::symlink, fs::PermissionsExt};

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("cc-switch.db");
        let database = SharedDatabase::open(&path).expect("open shared database");
        database
            .ensure_provider_schema()
            .expect("initialize provider schema");
        assert_eq!(
            fs::metadata(&path)
                .expect("database metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let hard_link = directory.path().join("hard-linked.db");
        fs::hard_link(&path, &hard_link).expect("create database hard link");
        assert!(matches!(
            database.connect(),
            Err(SharedStoreError::InvalidDatabase(message))
                if message.contains("hard links")
        ));
        fs::remove_file(hard_link).expect("remove database hard link");

        let link = directory.path().join("linked.db");
        symlink(&path, &link).expect("create database symlink");
        assert!(matches!(
            SharedDatabase::open(link),
            Err(SharedStoreError::InvalidDatabase(message))
                if message.contains("symbolic link")
        ));

        fs::remove_file(&path).expect("remove original database");
        symlink(directory.path().join("target.db"), &path).expect("replace database with symlink");
        assert!(matches!(
            database.connect(),
            Err(SharedStoreError::InvalidDatabase(message))
                if message.contains("symbolic link")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_database_hard_links_are_rejected() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("cc-switch.db");
        let database = SharedDatabase::open(&path).expect("open shared database");
        database
            .ensure_provider_schema()
            .expect("initialize provider schema");
        fs::hard_link(&path, directory.path().join("hard-linked.db"))
            .expect("create database hard link");
        assert!(matches!(
            database.connect(),
            Err(SharedStoreError::InvalidDatabase(message))
                if message.contains("hard links")
        ));
    }
}
