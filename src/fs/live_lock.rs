use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use fs4::{FileExt, TryLockError};
use thiserror::Error;

/// Failure to acquire a shared live-configuration lock.
#[derive(Debug, Error)]
pub enum SharedLiveConfigLockError {
    /// Another cooperating operation holds the lock; no waiting is performed.
    #[error("live configuration lock is unavailable")]
    Unavailable,
    /// Opening, securing, or locking the file failed.
    #[error("live configuration lock I/O failed for {path:?}: {source}")]
    Io {
        /// The directory or file at which the operation failed.
        path: PathBuf,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
}

/// An exclusive advisory lock held until this value is dropped.
///
/// The host supplies a dedicated, stable lock-file path, normally from
/// [`super::shared_live_config_lock_path`]. All cooperating writers must use the
/// same file and must never unlink or replace it. Symlinks are followed, so the
/// host must keep their targets and the containing directory stable too. This
/// lock does not protect against writers that ignore the protocol.
///
/// Begin the shared-catalog write transaction before acquiring this lock. Hold
/// it through native observation, publication, and database commit or native
/// compensation. The host owns that lifetime and its transaction; acquiring a
/// lock alone does not make SQLite and filesystem changes atomic.
#[derive(Debug)]
#[must_use = "dropping the guard releases the live-configuration lock"]
pub struct SharedLiveConfigLock {
    _file: File,
}

impl SharedLiveConfigLock {
    /// Tries once to acquire the lock, without waiting for another holder.
    ///
    /// Creates missing parent directories and opens the file without truncating
    /// it. On Unix, both new and existing lock files receive mode `0600`; on
    /// Windows, access follows the containing directory's ACL. The file remains
    /// on disk after release. These rules match Lite's existing lock protocol.
    ///
    /// ```
    /// use cc_switch_core::fs::{shared_live_config_lock_path, SharedLiveConfigLock};
    /// let profile = tempfile::tempdir()?;
    /// let lock = SharedLiveConfigLock::try_acquire(
    ///     &shared_live_config_lock_path(profile.path()),
    /// )?;
    /// // The host completes its protected work before releasing the guard.
    /// drop(lock);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn try_acquire(path: &Path) -> Result<Self, SharedLiveConfigLockError> {
        let io_error = |source| SharedLiveConfigLockError::Io {
            path: path.to_owned(),
            source,
        };
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| SharedLiveConfigLockError::Io {
                path: parent.to_owned(),
                source,
            })?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path).map_err(io_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(io_error)?;
        }
        FileExt::try_lock(&file).map_err(|error| match error {
            TryLockError::WouldBlock => SharedLiveConfigLockError::Unavailable,
            TryLockError::Error(source) => io_error(source),
        })?;
        Ok(Self { _file: file })
    }
}
