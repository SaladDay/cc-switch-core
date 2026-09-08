//! Checked provider operations within an MCP-owning transaction.

use super::*;
use crate::{ProviderRow, ProviderWriteOutcome};

pub(super) type ProviderFingerprints = BTreeMap<(String, String), [u8; 32]>;

pub(super) fn read_fingerprints(
    connection: &Connection,
) -> Result<ProviderFingerprints, SharedStoreError> {
    let mut statement = connection.prepare(
        "SELECT id, app_type, providers.* FROM main.providers AS providers
         ORDER BY id COLLATE BINARY, app_type COLLATE BINARY",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(((row.get(0)?, row.get(1)?), source_fingerprint(row, 2)?))
    })?;
    rows.collect::<Result<_, _>>()
        .map_err(SharedStoreError::from)
}

fn projected_fingerprint(
    connection: &Connection,
    id: &str,
    app_type: &str,
    field: &str,
    value: &dyn rusqlite::ToSql,
) -> Result<Option<[u8; 32]>, SharedStoreError> {
    // Project only the owned field against the original row. Generated and
    // unknown fields retain their original values in this expected fingerprint.
    let columns = crate::provider_columns(connection)?
        .into_iter()
        .map(|column| {
            if column.name == field {
                "?3".to_owned()
            } else {
                format!("\"{}\"", column.name.replace('"', "\"\""))
            }
        })
        .collect::<Vec<_>>();
    let sql = format!(
        "SELECT {} FROM main.providers WHERE id COLLATE BINARY = ?1 AND app_type COLLATE BINARY = ?2",
        columns.join(", ")
    );
    connection
        .query_row(&sql, params![id, app_type, value], |row| {
            source_fingerprint(row, 0)
        })
        .optional()
        .map_err(SharedStoreError::from)
}

impl<'connection> McpTransactionGuard<'connection> {
    /// Takes ownership of a caller's provider transaction before native publication.
    ///
    /// Captures every provider, MCP catalog and native-link row, including unknown
    /// columns and Apps. Checked provider writes and MCP writes may then alternate;
    /// commit verifies all three tables. No raw transaction escape is available.
    /// The host still chooses provider policy, native operations and their recovery
    /// order, and calls `commit_preserving_on_error` only after all owned work.
    ///
    /// Use an immediate transaction for coordinated writers. Capture host-specific
    /// read inputs before transferring it here. Construction failure rolls back the
    /// entire transaction, including earlier writes, so transfer it before changing
    /// native files. Product extension-table writes after transfer are not exposed.
    /// This does not make filesystem and database changes crash-atomic.
    pub fn from_provider_transaction(
        transaction: Transaction<'connection>,
    ) -> Result<Self, SharedStoreError> {
        let mut guard = Self::from_transaction(transaction)?;
        crate::verify_provider_schema(&guard.transaction)?;
        guard.expected_providers = Some(read_fingerprints(&guard.transaction)?);
        Ok(guard)
    }

    /// Reads one raw provider, including the fingerprint of host-owned columns.
    /// Requires `from_provider_transaction`; errors poison the whole guard.
    pub fn read_provider(
        &mut self,
        id: &str,
        app_type: &str,
    ) -> Result<Option<ProviderRow>, SharedStoreError> {
        self.prepare_provider_access()?;
        match crate::read_provider_row(&self.transaction, id, app_type) {
            Ok(row) => Ok(row),
            Err(error) => self.poison(error),
        }
    }

    /// Reads providers in the shared ordering without parsing native settings.
    /// Requires `from_provider_transaction`; errors poison the whole guard.
    pub fn read_providers(
        &mut self,
        app_type: Option<&str>,
    ) -> Result<Vec<ProviderRow>, SharedStoreError> {
        self.prepare_provider_access()?;
        match crate::read_provider_rows(&self.transaction, app_type) {
            Ok(rows) => Ok(rows),
            Err(error) => self.poison(error),
        }
    }

    /// Changes only raw provider settings using the complete source fingerprint.
    /// Requires `from_provider_transaction`. A stale/suppressed write returns
    /// `NotApplied` and poisons the guard even if the host ignores that outcome.
    pub fn update_provider_settings_config_if_unchanged(
        &mut self,
        id: &str,
        app_type: &str,
        expected_fingerprint: &[u8; 32],
        settings_config: &str,
    ) -> Result<ProviderWriteOutcome, SharedStoreError> {
        self.provider_write(
            id,
            app_type,
            "settings_config",
            &settings_config,
            |transaction| {
                crate::update_provider_settings_config_if_unchanged(
                    transaction,
                    id,
                    app_type,
                    expected_fingerprint,
                    settings_config,
                )
            },
        )
    }

    /// Changes one provider selection flag, not another provider or host setting.
    /// Requires `from_provider_transaction`. The host chooses selection order.
    /// A `NotApplied` outcome or error poisons the guard until rollback.
    pub fn set_provider_current_if_unchanged(
        &mut self,
        id: &str,
        app_type: &str,
        expected_fingerprint: &[u8; 32],
        is_current: bool,
    ) -> Result<ProviderWriteOutcome, SharedStoreError> {
        self.provider_write(id, app_type, "is_current", &is_current, |transaction| {
            crate::set_provider_current_if_unchanged(
                transaction,
                id,
                app_type,
                expected_fingerprint,
                is_current,
            )
        })
    }

    fn prepare_provider_access(&mut self) -> Result<(), SharedStoreError> {
        if self.expected_providers.is_none() {
            return self.poison(SharedStoreError::McpTransactionConflict);
        }
        self.prepare_write()
    }

    fn provider_write(
        &mut self,
        id: &str,
        app_type: &str,
        field: &str,
        value: &dyn rusqlite::ToSql,
        write: impl FnOnce(&mut Transaction<'_>) -> Result<ProviderWriteOutcome, SharedStoreError>,
    ) -> Result<ProviderWriteOutcome, SharedStoreError> {
        self.prepare_provider_access()?;
        let expected = match projected_fingerprint(&self.transaction, id, app_type, field, value) {
            Ok(Some(expected)) => expected,
            Ok(None) => {
                self.failed = true;
                return Ok(ProviderWriteOutcome::NotApplied);
            }
            Err(error) => return self.poison(error),
        };
        match write(&mut self.transaction) {
            Ok(ProviderWriteOutcome::Applied) => {
                self.expected_providers
                    .as_mut()
                    .expect("provider access checked")
                    .insert((id.to_owned(), app_type.to_owned()), expected);
                self.prepare_write()?;
                Ok(ProviderWriteOutcome::Applied)
            }
            Ok(ProviderWriteOutcome::NotApplied) => {
                self.failed = true;
                Ok(ProviderWriteOutcome::NotApplied)
            }
            Err(error) => self.poison(error),
        }
    }
}
