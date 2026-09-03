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

use rusqlite::{
    params, types::ValueRef, Connection, OpenFlags, OptionalExtension, Params, Row, Transaction,
    TransactionBehavior,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

mod mcp;

pub use mcp::{
    ensure_mcp_server_schema, read_mcp_server_row, read_mcp_server_rows,
    verify_mcp_server_write_contract, McpServerRow, MCP_SERVERS_TABLE,
};

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Canonical table shared by CC Switch products.
pub const PROVIDERS_TABLE: &str = "providers";

const SELECT_PROVIDER_FIELDS: &str = "id, app_type, name, settings_config,
    website_url, category, created_at, sort_index, notes, icon, icon_color,
    meta, is_current, in_failover_queue";
const PROVIDER_FIELD_COUNT: usize = 14;

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
    source_fingerprint: [u8; 32],
}

impl ProviderRow {
    /// Identifies every column value read from this source row, including
    /// unknown future columns, without exposing their contents.
    pub fn source_fingerprint(&self) -> &[u8; 32] {
        &self.source_fingerprint
    }
}

/// Raw values for inserting one row into the shared `providers` table.
pub struct ProviderInsert<'a> {
    pub id: &'a str,
    pub app_type: &'a str,
    pub name: &'a str,
    pub settings_config: &'a str,
    pub website_url: Option<&'a str>,
    pub category: Option<&'a str>,
    pub created_at: Option<i64>,
    pub sort_index: Option<i64>,
    pub notes: Option<&'a str>,
    pub icon: Option<&'a str>,
    pub icon_color: Option<&'a str>,
    pub meta: &'a str,
    pub is_current: i64,
    pub in_failover_queue: i64,
}

impl fmt::Debug for ProviderInsert<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderInsert")
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

/// Result of one provider-row write.
///
/// `NotApplied` covers a missing row, a suppressed write, or a provider update
/// whose trigger performed additional writes. Hosts decide how to interpret it
/// inside their own transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum ProviderWriteOutcome {
    /// The statement directly affected one row. Provider updates additionally
    /// guarantee that no trigger or foreign-key side-effect write persisted.
    Applied,
    /// The write did not persist, including a missing identity, a suppressed
    /// statement, or a provider update with additional write side effects.
    NotApplied,
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

const CREATE_PROVIDERS_TABLE: &str = "CREATE TABLE IF NOT EXISTS main.providers (
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
    #[error("shared provider write failed")]
    ProviderWrite {
        code: Option<rusqlite::ErrorCode>,
        extended_code: Option<i32>,
        /// SQLite ended the host transaction while executing the write.
        transaction_aborted: bool,
    },
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
                    "ALTER TABLE main.providers ADD COLUMN {} {}",
                    expected.name, expected.declaration
                ))?;
            }
        }
        verify_provider_schema(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    /// Creates or transactionally upgrades the shared `mcp_servers` contract.
    ///
    /// Product migrations, live configuration state, and private MCP tables
    /// remain owned by the host.
    pub fn ensure_mcp_server_schema(&self) -> Result<(), SharedStoreError> {
        let mut connection = self.connect()?;
        ensure_mcp_server_schema(&mut connection)
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

/// Starts the immediate transaction used for coordinated provider writes.
///
/// Hosts retain ownership of the transaction and may combine these shared row
/// operations with product-specific tables or external rollback handling. A
/// nested savepoint rolls back statement effects that SQLite might otherwise
/// retain when a provider write reports an error. After any provider write
/// error, the host must stop using and drop the transaction; `transaction_aborted`
/// reports when SQLite has already ended it. Provider update primitives also
/// roll back and return [`ProviderWriteOutcome::NotApplied`] if their statement
/// causes any additional trigger or foreign-key writes.
pub fn begin_immediate_transaction(
    connection: &mut Connection,
) -> Result<Transaction<'_>, SharedStoreError> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(SharedStoreError::from)
}

/// Inserts one raw provider row without parsing host-owned JSON fields.
pub fn insert_provider(
    transaction: &mut Transaction<'_>,
    provider: &ProviderInsert<'_>,
) -> Result<ProviderWriteOutcome, SharedStoreError> {
    execute_provider_write(
        transaction,
        "INSERT INTO main.providers (
            id, app_type, name, settings_config, website_url, category,
            created_at, sort_index, notes, icon, icon_color, meta,
            is_current, in_failover_queue
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
         )",
        params![
            provider.id,
            provider.app_type,
            provider.name,
            provider.settings_config,
            provider.website_url,
            provider.category,
            provider.created_at,
            provider.sort_index,
            provider.notes,
            provider.icon,
            provider.icon_color,
            provider.meta,
            provider.is_current,
            provider.in_failover_queue,
        ],
        false,
    )
}

/// Updates the user-visible name and raw settings of one provider.
pub fn update_provider_configuration(
    transaction: &mut Transaction<'_>,
    id: &str,
    app_type: &str,
    name: &str,
    settings_config: &str,
) -> Result<ProviderWriteOutcome, SharedStoreError> {
    execute_provider_write(
        transaction,
        "UPDATE main.providers SET name = ?1, settings_config = ?2
         WHERE id COLLATE BINARY = ?3 AND app_type COLLATE BINARY = ?4",
        params![name, settings_config, id, app_type],
        true,
    )
}

/// Atomically updates the shared editable details of one provider.
pub fn update_provider_details(
    transaction: &mut Transaction<'_>,
    id: &str,
    app_type: &str,
    name: &str,
    settings_config: &str,
    category: Option<&str>,
    meta: &str,
) -> Result<ProviderWriteOutcome, SharedStoreError> {
    execute_provider_write(
        transaction,
        "UPDATE main.providers
         SET name = ?1, settings_config = ?2, category = ?3, meta = ?4
         WHERE id COLLATE BINARY = ?5 AND app_type COLLATE BINARY = ?6",
        params![name, settings_config, category, meta, id, app_type],
        true,
    )
}

/// Updates the raw metadata JSON of one provider.
pub fn update_provider_metadata(
    transaction: &mut Transaction<'_>,
    id: &str,
    app_type: &str,
    meta: &str,
) -> Result<ProviderWriteOutcome, SharedStoreError> {
    execute_provider_write(
        transaction,
        "UPDATE main.providers SET meta = ?1
         WHERE id COLLATE BINARY = ?2 AND app_type COLLATE BINARY = ?3",
        params![meta, id, app_type],
        true,
    )
}

/// Changes the current flag on one provider.
pub fn set_provider_current(
    transaction: &mut Transaction<'_>,
    id: &str,
    app_type: &str,
    is_current: bool,
) -> Result<ProviderWriteOutcome, SharedStoreError> {
    execute_provider_write(
        transaction,
        "UPDATE main.providers SET is_current = ?1
         WHERE id COLLATE BINARY = ?2 AND app_type COLLATE BINARY = ?3",
        params![is_current, id, app_type],
        true,
    )
}

/// Deletes one provider by its composite identity.
pub fn delete_provider(
    transaction: &mut Transaction<'_>,
    id: &str,
    app_type: &str,
) -> Result<ProviderWriteOutcome, SharedStoreError> {
    execute_provider_write(
        transaction,
        "DELETE FROM main.providers
         WHERE id COLLATE BINARY = ?1 AND app_type COLLATE BINARY = ?2",
        params![id, app_type],
        false,
    )
}

fn execute_provider_write<P: Params>(
    transaction: &mut Transaction<'_>,
    sql: &str,
    params: P,
    reject_side_effects: bool,
) -> Result<ProviderWriteOutcome, SharedStoreError> {
    if transaction.is_autocommit() {
        return Err(SharedStoreError::ProviderWrite {
            code: None,
            extended_code: None,
            transaction_aborted: true,
        });
    }
    verify_provider_write_schema(transaction)?;
    let transaction_aborted = transaction.is_autocommit();
    let savepoint = transaction
        .savepoint()
        .map_err(|error| redact_provider_write_error(error, transaction_aborted))?;
    let result = (|| {
        let total_before = savepoint.total_changes();
        let changed = {
            let mut statement = savepoint.prepare(sql)?;
            let mut rows = statement.query(params)?;
            while rows.next()?.is_some() {}
            savepoint.changes() as usize
        };
        Ok((changed, savepoint.total_changes() - total_before))
    })();
    let (changed, total_changed) = match result {
        Ok(result) => result,
        Err(error) => {
            let _ = savepoint.finish();
            return Err(redact_provider_write_error(
                error,
                transaction.is_autocommit(),
            ));
        }
    };
    if reject_side_effects && total_changed != changed as u64 {
        savepoint
            .finish()
            .map_err(|error| redact_provider_write_error(error, transaction.is_autocommit()))?;
        return Ok(ProviderWriteOutcome::NotApplied);
    }
    let outcome = match changed {
        0 => ProviderWriteOutcome::NotApplied,
        1 => ProviderWriteOutcome::Applied,
        _ => {
            savepoint
                .finish()
                .map_err(|error| redact_provider_write_error(error, transaction.is_autocommit()))?;
            return Err(SharedStoreError::InvalidDatabase(
                "provider write affected multiple rows".to_owned(),
            ));
        }
    };
    savepoint
        .commit()
        .map_err(|error| redact_provider_write_error(error, transaction.is_autocommit()))?;
    Ok(outcome)
}

fn redact_provider_write_error(
    error: rusqlite::Error,
    transaction_aborted: bool,
) -> SharedStoreError {
    let extended_code = match &error {
        rusqlite::Error::SqliteFailure(error, _) => Some(error.extended_code),
        _ => None,
    };
    SharedStoreError::ProviderWrite {
        code: error.sqlite_error_code(),
        extended_code,
        transaction_aborted,
    }
}

fn verify_provider_write_schema(transaction: &Transaction<'_>) -> Result<(), SharedStoreError> {
    verify_provider_schema(transaction).map_err(|error| match error {
        SharedStoreError::Database(error) => {
            redact_provider_write_error(error, transaction.is_autocommit())
        }
        error => error,
    })
}

/// Reads shared provider rows in the ordering used by CC Switch products.
pub fn read_provider_rows(
    connection: &Connection,
    app_type: Option<&str>,
) -> Result<Vec<ProviderRow>, SharedStoreError> {
    let sql = format!(
        "SELECT {SELECT_PROVIDER_FIELDS}, providers.* FROM main.providers AS providers
         WHERE (?1 IS NULL OR app_type COLLATE BINARY = ?1)
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
        "SELECT {SELECT_PROVIDER_FIELDS}, providers.* FROM main.providers AS providers
         WHERE id COLLATE BINARY = ?1 AND app_type COLLATE BINARY = ?2"
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
        source_fingerprint: source_fingerprint(row, PROVIDER_FIELD_COUNT)?,
    })
}

pub(crate) fn source_fingerprint(
    row: &Row<'_>,
    source_offset: usize,
) -> Result<[u8; 32], rusqlite::Error> {
    let statement = row.as_ref();
    let mut hasher = Sha256::new();
    hasher.update(((statement.column_count() - source_offset) as u64).to_le_bytes());
    for index in source_offset..statement.column_count() {
        match row.get_ref(index)? {
            ValueRef::Null => hasher.update([0]),
            ValueRef::Integer(value) => {
                hasher.update([1]);
                hasher.update(value.to_le_bytes());
            }
            ValueRef::Real(value) => {
                hasher.update([2]);
                hasher.update(value.to_bits().to_le_bytes());
            }
            ValueRef::Text(value) => {
                hasher.update([3]);
                hasher.update((value.len() as u64).to_le_bytes());
                hasher.update(value);
            }
            ValueRef::Blob(value) => {
                hasher.update([4]);
                hasher.update((value.len() as u64).to_le_bytes());
                hasher.update(value);
            }
        }
    }
    Ok(hasher.finalize().into())
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
            "SELECT type FROM main.sqlite_schema WHERE name = 'providers'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if object_type.as_deref() != Some("table") {
        return Err(SharedStoreError::InvalidDatabase(
            "providers must be a table".to_owned(),
        ));
    }

    let mut statement = connection.prepare("PRAGMA main.table_xinfo(providers)")?;
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
            "SELECT name FROM pragma_index_list('providers', 'main')
             WHERE origin = 'pk' AND partial = 0",
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

    fn provider_insert<'a>(
        id: &'a str,
        settings_config: &'a str,
        meta: &'a str,
    ) -> ProviderInsert<'a> {
        ProviderInsert {
            id,
            app_type: "claude",
            name: "Provider",
            settings_config,
            website_url: None,
            category: None,
            created_at: Some(1),
            sort_index: Some(0),
            notes: None,
            icon: None,
            icon_color: None,
            meta,
            is_current: 0,
            in_failover_queue: 0,
        }
    }

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
        let transaction = connection
            .unchecked_transaction()
            .expect("begin read transaction");
        assert!(read_provider_row(&transaction, "first", "claude")
            .expect("read provider through transaction")
            .is_some());
        transaction.rollback().expect("roll back read transaction");
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

        connection
            .execute_batch("PRAGMA short_column_names=OFF; PRAGMA full_column_names=ON;")
            .expect("enable qualified result names");
        let qualified = read_provider_row(&connection, "later", "claude")
            .expect("read with qualified result names")
            .expect("provider exists");
        assert_eq!(qualified, rows[1]);
    }

    #[test]
    fn provider_read_errors_do_not_expose_sensitive_values() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        database
            .ensure_provider_schema()
            .expect("initialize provider schema");
        let connection = database.connect().expect("connect shared database");
        connection
            .execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('sensitive', 'claude', 'Sensitive', '{}', '{}')",
                [],
            )
            .expect("insert provider fixture");

        for (column, secret) in [
            ("settings_config", b"secret-settings".as_slice()),
            ("meta", b"secret-meta".as_slice()),
        ] {
            connection
                .execute(
                    &format!(
                        "UPDATE providers SET {column} = ?1
                         WHERE id = 'sensitive' AND app_type = 'claude'"
                    ),
                    [secret],
                )
                .expect("store invalid sensitive value");
            let error = read_provider_row(&connection, "sensitive", "claude")
                .expect_err("invalid sensitive column must fail");
            assert!(!error.to_string().contains("secret-"));
            connection
                .execute(
                    &format!(
                        "UPDATE providers SET {column} = '{{}}'
                         WHERE id = 'sensitive' AND app_type = 'claude'"
                    ),
                    [],
                )
                .expect("restore sensitive value");
        }
    }

    #[test]
    fn provider_write_primitives_preserve_host_extensions() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        database
            .ensure_provider_schema()
            .expect("initialize provider schema");
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "ALTER TABLE main.providers
                 ADD COLUMN future_column TEXT NOT NULL DEFAULT 'keep';
                 CREATE TEMP TABLE providers AS
                 SELECT * FROM main.providers WHERE 0;",
            )
            .expect("add host extension and temporary shadow");
        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");
        let mut provider = provider_insert("provider", "secret-settings", "secret-meta");
        provider.name = "Original";
        provider.website_url = Some("https://example.com");
        provider.category = Some("custom");
        provider.sort_index = Some(2);
        provider.notes = Some("note");
        provider.icon = Some("icon");
        provider.icon_color = Some("#fff");
        provider.in_failover_queue = 1;
        let debug = format!("{provider:?}");
        assert!(!debug.contains("secret-settings"));
        assert!(!debug.contains("secret-meta"));
        assert_eq!(
            insert_provider(&mut transaction, &provider).expect("insert provider"),
            ProviderWriteOutcome::Applied
        );
        assert_eq!(
            update_provider_configuration(&mut transaction, "provider", "claude", "Updated", "{}",)
                .expect("update provider configuration"),
            ProviderWriteOutcome::Applied
        );
        assert_eq!(
            update_provider_metadata(&mut transaction, "provider", "claude", "{}")
                .expect("update provider metadata"),
            ProviderWriteOutcome::Applied
        );
        assert_eq!(
            update_provider_details(
                &mut transaction,
                "provider",
                "claude",
                "Imported",
                "imported-settings",
                Some("imported"),
                "imported-meta",
            )
            .expect("update provider details"),
            ProviderWriteOutcome::Applied
        );
        assert_eq!(
            set_provider_current(&mut transaction, "provider", "claude", true)
                .expect("set current provider"),
            ProviderWriteOutcome::Applied
        );
        assert_eq!(
            set_provider_current(&mut transaction, "provider", "claude", false)
                .expect("clear current provider"),
            ProviderWriteOutcome::Applied
        );
        transaction.commit().expect("commit provider writes");

        let row = read_provider_row(&connection, "provider", "claude")
            .expect("read provider")
            .expect("provider exists");
        assert_eq!(row.name, "Imported");
        assert_eq!(row.settings_config, "imported-settings");
        assert_eq!(row.category.as_deref(), Some("imported"));
        assert_eq!(row.meta, "imported-meta");
        assert_eq!(row.is_current, 0);
        assert_eq!(row.in_failover_queue, 1);
        assert_eq!(
            connection
                .query_row(
                    "SELECT future_column FROM main.providers
                     WHERE id = 'provider' AND app_type = 'claude'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read host extension"),
            "keep"
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM temp.providers", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("read temporary shadow"),
            0
        );

        connection
            .execute_batch(
                "CREATE TRIGGER rewrite_host_extension
                 BEFORE UPDATE OF name ON main.providers
                 BEGIN
                     UPDATE providers SET future_column = 'rewritten'
                     WHERE id = NEW.id AND app_type = NEW.app_type;
                     SELECT RAISE(IGNORE);
                 END;",
            )
            .expect("create extension-rewriting host trigger");
        let mut transaction = begin_immediate_transaction(&mut connection)
            .expect("begin extension-guarded transaction");
        assert_eq!(
            update_provider_configuration(
                &mut transaction,
                "provider",
                "claude",
                "Rejected",
                "rejected-settings",
            )
            .expect("guard provider extensions"),
            ProviderWriteOutcome::NotApplied
        );
        transaction
            .commit()
            .expect("commit extension-guarded transaction");
        assert_eq!(
            connection
                .query_row(
                    "SELECT name, future_column FROM main.providers
                     WHERE id = 'provider' AND app_type = 'claude'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .expect("read extension-guarded provider"),
            ("Imported".to_owned(), "keep".to_owned())
        );

        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");
        assert_eq!(
            update_provider_configuration(&mut transaction, "missing", "claude", "Missing", "{}",)
                .expect("update missing configuration"),
            ProviderWriteOutcome::NotApplied
        );
        assert_eq!(
            update_provider_metadata(&mut transaction, "missing", "claude", "{}")
                .expect("update missing metadata"),
            ProviderWriteOutcome::NotApplied
        );
        assert_eq!(
            set_provider_current(&mut transaction, "missing", "claude", true)
                .expect("set missing current provider"),
            ProviderWriteOutcome::NotApplied
        );
        assert_eq!(
            delete_provider(&mut transaction, "provider", "claude").expect("delete provider"),
            ProviderWriteOutcome::Applied
        );
        assert_eq!(
            delete_provider(&mut transaction, "provider", "claude")
                .expect("delete missing provider"),
            ProviderWriteOutcome::NotApplied
        );
        transaction.commit().expect("commit provider deletes");
    }

    #[test]
    fn provider_writes_report_host_trigger_suppression() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        database
            .ensure_provider_schema()
            .expect("initialize provider schema");
        let mut connection = database.connect().expect("connect shared database");
        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");
        assert_eq!(
            insert_provider(
                &mut transaction,
                &provider_insert("provider", "original", "original")
            )
            .expect("insert provider fixture"),
            ProviderWriteOutcome::Applied
        );
        transaction.commit().expect("commit provider fixture");
        connection
            .execute_batch(
                "CREATE TRIGGER ignore_insert BEFORE INSERT ON providers
                 BEGIN SELECT RAISE(IGNORE); END;
                 CREATE TRIGGER ignore_config BEFORE UPDATE OF name, settings_config ON providers
                 BEGIN SELECT RAISE(IGNORE); END;
                 CREATE TRIGGER ignore_meta BEFORE UPDATE OF meta ON providers
                 BEGIN SELECT RAISE(IGNORE); END;
                 CREATE TRIGGER ignore_current BEFORE UPDATE OF is_current ON providers
                 BEGIN SELECT RAISE(IGNORE); END;
                 CREATE TRIGGER ignore_delete BEFORE DELETE ON providers
                 BEGIN SELECT RAISE(IGNORE); END;",
            )
            .expect("create suppressing host triggers");
        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");

        assert_eq!(
            insert_provider(&mut transaction, &provider_insert("ignored", "{}", "{}"))
                .expect("suppress insert"),
            ProviderWriteOutcome::NotApplied
        );
        assert_eq!(
            update_provider_configuration(
                &mut transaction,
                "provider",
                "claude",
                "Updated",
                "new",
            )
            .expect("suppress configuration update"),
            ProviderWriteOutcome::NotApplied
        );
        assert_eq!(
            update_provider_metadata(&mut transaction, "provider", "claude", "new")
                .expect("suppress metadata update"),
            ProviderWriteOutcome::NotApplied
        );
        assert_eq!(
            update_provider_details(
                &mut transaction,
                "provider",
                "claude",
                "Updated",
                "new",
                Some("new"),
                "new",
            )
            .expect("suppress details update"),
            ProviderWriteOutcome::NotApplied
        );
        assert_eq!(
            set_provider_current(&mut transaction, "provider", "claude", true)
                .expect("suppress current update"),
            ProviderWriteOutcome::NotApplied
        );
        assert_eq!(
            delete_provider(&mut transaction, "provider", "claude").expect("suppress delete"),
            ProviderWriteOutcome::NotApplied
        );

        assert!(read_provider_row(&transaction, "ignored", "claude")
            .expect("read ignored provider")
            .is_none());
        let unchanged = read_provider_row(&transaction, "provider", "claude")
            .expect("read provider")
            .expect("provider remains");
        assert_eq!(unchanged.name, "Provider");
        assert_eq!(unchanged.settings_config, "original");
        assert_eq!(unchanged.meta, "original");
        assert_eq!(unchanged.is_current, 0);
        transaction.commit().expect("commit suppressed writes");
    }

    #[test]
    fn provider_write_errors_do_not_expose_trigger_messages() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        database
            .ensure_provider_schema()
            .expect("initialize provider schema");
        let mut connection = database.connect().expect("connect shared database");
        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");
        assert_eq!(
            insert_provider(&mut transaction, &provider_insert("provider", "{}", "{}"))
                .expect("insert provider fixture"),
            ProviderWriteOutcome::Applied
        );
        let duplicate = insert_provider(&mut transaction, &provider_insert("provider", "{}", "{}"))
            .expect_err("duplicate provider must fail");
        assert!(matches!(
            &duplicate,
            SharedStoreError::ProviderWrite {
                code: Some(rusqlite::ErrorCode::ConstraintViolation),
                extended_code: Some(rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY),
                transaction_aborted: false,
            }
        ));
        transaction.commit().expect("commit provider fixture");
        connection
            .execute_batch(
                "CREATE TRIGGER reject_sensitive_insert BEFORE INSERT ON providers
                 BEGIN SELECT RAISE(ABORT, 'secret-settings'); END;
                 CREATE TRIGGER reject_sensitive_config
                 BEFORE UPDATE OF settings_config ON providers
                 BEGIN SELECT RAISE(ABORT, 'secret-settings'); END;
                 CREATE TRIGGER reject_sensitive_meta BEFORE UPDATE OF meta ON providers
                 BEGIN SELECT RAISE(ABORT, 'secret-meta'); END;
                 CREATE TRIGGER reject_sensitive_current
                 BEFORE UPDATE OF is_current ON providers
                 BEGIN SELECT RAISE(ABORT, 'secret-meta'); END;
                 CREATE TRIGGER reject_sensitive_delete BEFORE DELETE ON providers
                 BEGIN SELECT RAISE(ABORT, 'secret-meta'); END;",
            )
            .expect("create rejecting host triggers");
        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");

        let errors = [
            insert_provider(
                &mut transaction,
                &provider_insert("blocked", "secret-settings", "secret-meta"),
            )
            .expect_err("insert must be rejected"),
            update_provider_configuration(
                &mut transaction,
                "provider",
                "claude",
                "Provider",
                "secret-settings",
            )
            .expect_err("configuration update must be rejected"),
            update_provider_metadata(&mut transaction, "provider", "claude", "secret-meta")
                .expect_err("metadata update must be rejected"),
            update_provider_details(
                &mut transaction,
                "provider",
                "claude",
                "Provider",
                "secret-settings",
                Some("secret-category"),
                "secret-meta",
            )
            .expect_err("details update must be rejected"),
            set_provider_current(&mut transaction, "provider", "claude", true)
                .expect_err("current update must be rejected"),
            delete_provider(&mut transaction, "provider", "claude")
                .expect_err("delete must be rejected"),
        ];
        for error in errors {
            assert!(matches!(
                &error,
                SharedStoreError::ProviderWrite {
                    code: Some(rusqlite::ErrorCode::ConstraintViolation),
                    extended_code: Some(rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER),
                    transaction_aborted: false,
                }
            ));
            let display = error.to_string();
            let debug = format!("{error:?}");
            assert!(!display.contains("secret-"));
            assert!(!debug.contains("secret-"));
        }
        assert!(!duplicate.to_string().contains("secret-"));
        assert!(!format!("{duplicate:?}").contains("secret-"));
        transaction.rollback().expect("roll back rejected writes");
    }

    #[test]
    fn provider_write_schema_errors_are_redacted() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        database
            .ensure_provider_schema()
            .expect("initialize provider schema");
        let mut connection = database.connect().expect("connect shared database");
        connection
            .execute_batch(
                "PRAGMA writable_schema = ON;
                 INSERT INTO main.sqlite_schema(type, name, tbl_name, rootpage, sql)
                 VALUES (
                    'trigger', 'secret-settings', 'providers', 0,
                    'CREATE TRIGGER secret-settings broken'
                 );
                 PRAGMA writable_schema = OFF;",
            )
            .expect("install malformed schema fixture");
        let schema_version: i64 = connection
            .pragma_query_value(None, "schema_version", |row| row.get(0))
            .expect("read schema version");
        connection
            .pragma_update(None, "schema_version", schema_version + 1)
            .expect("reload malformed schema");
        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");

        let error = insert_provider(&mut transaction, &provider_insert("provider", "{}", "{}"))
            .expect_err("malformed schema must reject provider write");
        assert!(matches!(
            &error,
            SharedStoreError::ProviderWrite {
                code: Some(_),
                transaction_aborted: false,
                ..
            }
        ));
        assert!(!error.to_string().contains("secret-settings"));
        assert!(!format!("{error:?}").contains("secret-settings"));
        transaction
            .rollback()
            .expect("roll back provider transaction");
    }

    #[test]
    fn provider_writes_are_atomic_with_fail_triggers_and_count_changes() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        database
            .ensure_provider_schema()
            .expect("initialize provider schema");
        let mut connection = database.connect().expect("connect shared database");
        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");
        assert_eq!(
            insert_provider(&mut transaction, &provider_insert("provider", "old", "{}"))
                .expect("insert provider fixture"),
            ProviderWriteOutcome::Applied
        );
        transaction.commit().expect("commit provider fixture");
        connection
            .execute_batch(
                "CREATE TRIGGER reject_after_insert AFTER INSERT ON providers
                 BEGIN SELECT RAISE(FAIL, 'secret-after-insert'); END;
                 CREATE TRIGGER reject_after_delete AFTER DELETE ON providers
                 BEGIN SELECT RAISE(FAIL, 'secret-after-delete'); END;",
            )
            .expect("create fail triggers");

        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");
        let errors = [
            insert_provider(&mut transaction, &provider_insert("failed", "{}", "{}"))
                .expect_err("after-insert trigger must fail"),
            delete_provider(&mut transaction, "provider", "claude")
                .expect_err("after-delete trigger must fail"),
        ];
        for error in errors {
            assert!(!error.to_string().contains("secret-"));
            assert!(!format!("{error:?}").contains("secret-"));
        }
        assert!(read_provider_row(&transaction, "failed", "claude")
            .expect("read failed insert")
            .is_none());
        assert!(read_provider_row(&transaction, "provider", "claude")
            .expect("read failed delete")
            .is_some());
        transaction.commit().expect("commit outer transaction");

        connection
            .execute_batch(
                "DROP TRIGGER reject_after_insert;
                 DROP TRIGGER reject_after_delete;
                 PRAGMA count_changes = ON;",
            )
            .expect("enable changed-row results");
        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");
        assert_eq!(
            update_provider_configuration(
                &mut transaction,
                "provider",
                "claude",
                "Updated",
                "new",
            )
            .expect("update with count_changes enabled"),
            ProviderWriteOutcome::Applied
        );
        transaction.commit().expect("commit provider update");
        assert_eq!(
            read_provider_row(&connection, "provider", "claude")
                .expect("read updated provider")
                .expect("provider exists")
                .settings_config,
            "new"
        );

        connection
            .execute_batch(
                "PRAGMA count_changes = OFF;
                 CREATE TABLE host_state (value TEXT NOT NULL);
                 CREATE TRIGGER rollback_after_delete AFTER DELETE ON providers
                 BEGIN SELECT RAISE(ROLLBACK, 'secret-rollback'); END;",
            )
            .expect("create rollback trigger");
        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");
        transaction
            .execute("INSERT INTO host_state VALUES ('pending')", [])
            .expect("write host state");
        let error = delete_provider(&mut transaction, "provider", "claude")
            .expect_err("rollback trigger must abort the transaction");
        assert!(matches!(
            &error,
            SharedStoreError::ProviderWrite {
                code: Some(rusqlite::ErrorCode::ConstraintViolation),
                extended_code: Some(rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER),
                transaction_aborted: true,
            }
        ));
        assert!(!error.to_string().contains("secret-"));
        assert!(!format!("{error:?}").contains("secret-"));
        assert!(transaction.is_autocommit());
        assert!(matches!(
            insert_provider(
                &mut transaction,
                &provider_insert("must-not-commit", "{}", "{}"),
            )
            .expect_err("inactive transaction must reject another write"),
            SharedStoreError::ProviderWrite {
                transaction_aborted: true,
                ..
            }
        ));
        drop(transaction);
        assert!(read_provider_row(&connection, "provider", "claude")
            .expect("read rolled-back provider")
            .is_some());
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM host_state", [], |row| row
                    .get::<_, i64>(0))
                .expect("read rolled-back host state"),
            0
        );
        assert!(read_provider_row(&connection, "must-not-commit", "claude")
            .expect("read rejected provider")
            .is_none());
    }

    #[test]
    fn provider_writes_reject_a_noncanonical_identity_before_changes() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        database
            .ensure_provider_schema()
            .expect("initialize provider schema");
        let mut connection = database.connect().expect("connect shared database");
        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");
        assert_eq!(
            insert_provider(&mut transaction, &provider_insert("duplicate", "{}", "{}"))
                .expect("insert provider fixture"),
            ProviderWriteOutcome::Applied
        );
        transaction.commit().expect("commit provider fixture");
        connection
            .execute_batch(
                "ALTER TABLE providers RENAME TO canonical_providers;
                 CREATE TABLE providers AS SELECT * FROM canonical_providers;
                 INSERT INTO providers SELECT * FROM canonical_providers;",
            )
            .expect("replace provider schema without its identity constraint");

        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");
        assert!(matches!(
            delete_provider(&mut transaction, "duplicate", "claude")
                .expect_err("noncanonical provider identity must fail"),
            SharedStoreError::InvalidDatabase(_)
        ));
        transaction
            .rollback()
            .expect("roll back provider transaction");
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM providers", [], |row| row
                    .get::<_, i64>(0))
                .expect("count unchanged providers"),
            2
        );
    }

    #[test]
    fn provider_writes_use_binary_identity_matching() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("cc-switch.db");
        Connection::open(&path)
            .expect("create fixture database")
            .execute_batch(
                "CREATE TABLE providers (
                    id TEXT COLLATE NOCASE NOT NULL,
                    app_type TEXT COLLATE NOCASE NOT NULL,
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
                    PRIMARY KEY (id COLLATE BINARY, app_type COLLATE BINARY)
                 );
                 INSERT INTO providers (id, app_type, name, settings_config)
                 VALUES ('Foo', 'claude', 'Upper', 'upper');",
            )
            .expect("create case-insensitive-column fixture");
        let database = SharedDatabase::open(&path).expect("open shared database");
        database
            .ensure_provider_schema()
            .expect("accept binary provider identity");
        let mut connection = database.connect().expect("connect shared database");
        let mut transaction =
            begin_immediate_transaction(&mut connection).expect("begin provider transaction");
        assert_eq!(
            update_provider_metadata(&mut transaction, "foo", "claude", "wrong")
                .expect("try wrong-case identity"),
            ProviderWriteOutcome::NotApplied
        );
        transaction
            .execute(
                "INSERT INTO providers (id, app_type, name, settings_config)
                 VALUES ('foo', 'claude', 'Lower', 'lower')",
                [],
            )
            .expect("insert second binary identity");
        assert_eq!(
            set_provider_current(&mut transaction, "FOO", "claude", true)
                .expect("try ambiguous wrong-case identity"),
            ProviderWriteOutcome::NotApplied
        );
        assert_eq!(
            delete_provider(&mut transaction, "FOO", "claude")
                .expect("try deleting ambiguous wrong-case identity"),
            ProviderWriteOutcome::NotApplied
        );
        assert_eq!(
            update_provider_configuration(&mut transaction, "Foo", "claude", "Updated", "updated",)
                .expect("update exact identity"),
            ProviderWriteOutcome::Applied
        );
        transaction.commit().expect("commit provider writes");
        let rows = read_provider_rows(&connection, Some("claude")).expect("read providers");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            connection
                .query_row(
                    "SELECT settings_config FROM providers
                     WHERE id COLLATE BINARY = 'Foo'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read upper-case provider"),
            "updated"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT settings_config FROM providers
                     WHERE id COLLATE BINARY = 'foo'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read lower-case provider"),
            "lower"
        );
    }

    #[test]
    fn immediate_transactions_exclude_other_writers_and_can_roll_back() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = SharedDatabase::open(directory.path().join("cc-switch.db"))
            .expect("open shared database");
        database
            .ensure_provider_schema()
            .expect("initialize provider schema");
        let mut first = database.connect().expect("connect first writer");
        let second = database.connect().expect("connect second writer");
        second
            .busy_timeout(Duration::ZERO)
            .expect("disable second writer wait");
        let transaction =
            begin_immediate_transaction(&mut first).expect("begin immediate transaction");
        transaction
            .execute(
                "INSERT INTO providers (id, app_type, name, settings_config)
                 VALUES ('pending', 'claude', 'Pending', '{}')",
                [],
            )
            .expect("write inside immediate transaction");
        let error = second
            .execute(
                "INSERT INTO providers (id, app_type, name, settings_config)
                 VALUES ('blocked', 'claude', 'Blocked', '{}')",
                [],
            )
            .expect_err("second writer must be excluded");
        assert!(matches!(error, rusqlite::Error::SqliteFailure(_, _)));
        transaction.rollback().expect("roll back transaction");
        assert!(read_provider_row(&second, "blocked", "claude")
            .expect("read rolled back provider")
            .is_none());
        assert!(read_provider_row(&second, "pending", "claude")
            .expect("read rolled back provider")
            .is_none());
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

        let before = read_provider_row(&connection, "p", "claude")
            .expect("read provider before extension update")
            .expect("provider exists");
        connection
            .execute(
                "UPDATE providers SET future_column = 'changed' WHERE id = 'p'",
                [],
            )
            .expect("update future provider column");
        let after = read_provider_row(&connection, "p", "claude")
            .expect("read provider after extension update")
            .expect("provider exists");
        assert_eq!(before.settings_config, after.settings_config);
        assert_ne!(before.source_fingerprint(), after.source_fingerprint());
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
