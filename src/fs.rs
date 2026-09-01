//! Safe file-reading and atomic-writing primitives.

use serde::{de::DeserializeOwned, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns the common advisory-lock path for live configuration writes.
///
/// Hosts should begin their shared-catalog write transaction before taking an
/// exclusive lock here, then hold the lock through commit or rollback. Keeping
/// that order consistent avoids cross-product deadlocks and split state.
pub fn shared_live_config_lock_path(home: &Path) -> PathBuf {
    home.join(".cc-switch/live-config.lock")
}

/// An error produced while reading or replacing a file.
#[derive(Debug, Error)]
pub enum FileError {
    /// The requested file does not exist.
    #[error("file does not exist: {path:?}")]
    NotFound { path: PathBuf },
    /// The destination does not identify a file inside a directory.
    #[error("invalid file path: {path:?}")]
    InvalidPath { path: PathBuf },
    /// A filesystem operation failed.
    #[error("I/O error at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Replacing the destination with its completed temporary file failed.
    #[error("atomic replace failed ({temporary:?} -> {destination:?}): {source}")]
    AtomicReplace {
        temporary: PathBuf,
        destination: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A visible replacement or removal could not be made durable.
    #[error("filesystem change at {path:?} is visible but could not be made durable: {source}")]
    Durability {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// JSON could not be decoded from a file.
    #[error("JSON parse error at {path:?}: {source}")]
    JsonParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// A value could not be encoded as JSON.
    #[error("JSON serialization failed: {source}")]
    JsonSerialize {
        #[source]
        source: serde_json::Error,
    },
}

impl FileError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Returns whether the filesystem may already contain the requested change.
    pub const fn recovery_incomplete(&self) -> bool {
        matches!(self, Self::Durability { .. })
    }
}

/// Reads and deserializes a JSON file.
pub fn read_json_file<T: DeserializeOwned>(path: &Path) -> Result<T, FileError> {
    let content = read_text_file(path)?;
    serde_json::from_str(&content).map_err(|source| FileError::JsonParse {
        path: path.to_path_buf(),
        source,
    })
}

/// Reads a UTF-8 text file.
pub fn read_text_file(path: &Path) -> Result<String, FileError> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Err(FileError::NotFound {
            path: path.to_path_buf(),
        }),
        Err(source) => Err(FileError::io(path, source)),
    }
}

/// Writes deterministic, pretty-printed JSON and returns the bytes written.
pub fn write_json_file_with_contents<T: Serialize>(
    path: &Path,
    data: &T,
) -> Result<Vec<u8>, FileError> {
    let value = serde_json::to_value(data).map_err(|source| FileError::JsonSerialize { source })?;
    let json = serde_json::to_string_pretty(&sort_json_keys(&value))
        .map_err(|source| FileError::JsonSerialize { source })?;
    let contents = json.into_bytes();

    atomic_write(path, &contents)?;
    Ok(contents)
}

/// Writes deterministic, pretty-printed JSON atomically.
pub fn write_json_file<T: Serialize>(path: &Path, data: &T) -> Result<(), FileError> {
    write_json_file_with_contents(path, data).map(|_| ())
}

/// Writes a UTF-8 text file atomically.
pub fn write_text_file(path: &Path, data: &str) -> Result<(), FileError> {
    atomic_write(path, data.as_bytes())
}

/// Replaces a file atomically after writing its complete contents beside it.
///
/// On Unix, replacements preserve the destination mode and new files start at
/// `0600`.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), FileError> {
    atomic_write_with_unix_mode(path, data, None)
}

/// Atomically writes a credential file, using mode `0600` on Unix.
///
/// Windows access is governed by the destination directory's inherited ACL.
pub fn atomic_write_private(path: &Path, data: &[u8]) -> Result<(), FileError> {
    atomic_write_with_unix_mode(path, data, Some(0o600))
}

fn atomic_write_with_unix_mode(
    path: &Path,
    data: &[u8],
    unix_mode: Option<u32>,
) -> Result<(), FileError> {
    #[cfg(not(unix))]
    let _ = unix_mode;

    let parent = usable_parent(path)?;
    fs::create_dir_all(parent).map_err(|source| FileError::io(parent, source))?;

    path.file_name().ok_or_else(|| FileError::InvalidPath {
        path: path.to_path_buf(),
    })?;
    #[cfg(unix)]
    let final_mode = destination_unix_mode(path, unix_mode)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let (temporary, mut file) = create_temporary_file(parent, timestamp)?;

    let prepared = (|| -> std::io::Result<()> {
        file.write_all(data)?;
        file.flush()?;
        #[cfg(unix)]
        if let Some(mode) = final_mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        file.sync_all()
    })();
    if let Err(source) = prepared {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(FileError::io(&temporary, source));
    }
    drop(file);

    replace_file(&temporary, path)?;
    sync_directory(parent).map_err(|source| FileError::Durability {
        path: parent.to_owned(),
        source,
    })
}

/// Removes a file and makes the directory entry durable before returning.
pub fn remove_file_durable(path: &Path) -> Result<(), FileError> {
    fs::remove_file(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            FileError::NotFound {
                path: path.to_owned(),
            }
        } else {
            FileError::io(path, source)
        }
    })?;
    let parent = usable_parent(path)?;
    sync_directory(parent).map_err(|source| FileError::Durability {
        path: parent.to_owned(),
        source,
    })
}

fn usable_parent(path: &Path) -> Result<&Path, FileError> {
    let parent = path.parent().ok_or_else(|| FileError::InvalidPath {
        path: path.to_path_buf(),
    })?;
    if parent.as_os_str().is_empty() {
        Ok(Path::new("."))
    } else {
        Ok(parent)
    }
}

fn create_temporary_file(parent: &Path, timestamp: u128) -> Result<(PathBuf, fs::File), FileError> {
    let mut last_collision = None;
    for _ in 0..16 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".cc-switch.tmp.{}.{timestamp}.{counter}",
            std::process::id()
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                last_collision = Some((candidate, source));
            }
            Err(source) => return Err(FileError::io(candidate, source)),
        }
    }

    let (candidate, source) = last_collision.expect("temporary filename loop must run");
    Err(FileError::io(candidate, source))
}

#[cfg(unix)]
fn destination_unix_mode(path: &Path, unix_mode: Option<u32>) -> Result<Option<u32>, FileError> {
    use std::os::unix::fs::PermissionsExt;

    if unix_mode.is_some() {
        return Ok(unix_mode);
    }
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.permissions().mode())),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(FileError::io(path, source)),
    }
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, destination: &Path) -> Result<(), FileError> {
    if let Err(source) = fs::rename(temporary, destination) {
        let _ = fs::remove_file(temporary);
        return Err(FileError::AtomicReplace {
            temporary: temporary.to_path_buf(),
            destination: destination.to_path_buf(),
            source,
        });
    }
    Ok(())
}

#[cfg(windows)]
fn replace_file(temporary: &Path, destination: &Path) -> Result<(), FileError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn wide_path(path: &Path) -> Result<Vec<u16>, std::io::Error> {
        let mut value = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if value.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Windows paths cannot contain NUL",
            ));
        }
        if path.is_absolute()
            && !value.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16])
            && !value.starts_with(&[b'\\' as u16, b'\\' as u16, b'.' as u16, b'\\' as u16])
        {
            if value.starts_with(&[b'\\' as u16, b'\\' as u16]) {
                let mut extended = "\\\\?\\UNC\\".encode_utf16().collect::<Vec<_>>();
                extended.extend_from_slice(&value[2..]);
                value = extended;
            } else {
                let mut extended = "\\\\?\\".encode_utf16().collect::<Vec<_>>();
                extended.extend(value);
                value = extended;
            }
        }
        value.push(0);
        Ok(value)
    }

    let result = wide_path(temporary).and_then(|temporary_wide| {
        let destination_wide = wide_path(destination)?;
        // Both pointers remain valid and NUL-terminated for the duration of the call.
        let moved = unsafe {
            MoveFileExW(
                temporary_wide.as_ptr(),
                destination_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if moved == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    });
    if let Err(source) = result {
        let _ = fs::remove_file(temporary);
        return Err(FileError::AtomicReplace {
            temporary: temporary.to_owned(),
            destination: destination.to_owned(),
            source,
        });
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?
        .sync_all()
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn sort_json_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = Map::new();
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            for key in keys {
                sorted.insert(key.clone(), sort_json_keys(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(values) => Value::Array(values.iter().map(sort_json_keys).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temporary_files(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .expect("read test directory")
            .map(|entry| entry.expect("read entry"))
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".cc-switch.tmp.")
            })
            .map(|entry| entry.path())
            .collect()
    }

    #[test]
    fn live_config_lock_path_is_shared_across_products() {
        assert_eq!(
            shared_live_config_lock_path(Path::new("profile")),
            Path::new("profile/.cc-switch/live-config.lock")
        );
    }

    #[test]
    fn atomic_write_creates_parent_directories_and_replaces_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nested/config.json");

        atomic_write(&path, b"old").expect("create file");
        atomic_write(&path, b"new").expect("replace file");

        assert_eq!(fs::read(&path).expect("read file"), b"new");
        assert!(temporary_files(path.parent().unwrap()).is_empty());
    }

    #[test]
    fn durable_remove_deletes_an_existing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.json");
        fs::write(&path, b"contents").expect("seed file");

        remove_file_durable(&path).expect("remove file");

        assert!(!path.exists());
        assert!(matches!(
            remove_file_durable(&path),
            Err(FileError::NotFound { .. })
        ));
    }

    #[test]
    fn a_relative_file_uses_the_current_directory_as_its_parent() {
        assert_eq!(
            usable_parent(Path::new("config.json")).unwrap(),
            Path::new(".")
        );
    }

    #[test]
    fn failed_replace_preserves_destination_and_cleans_up() {
        let dir = tempfile::tempdir().expect("temp dir");
        let destination = dir.path().join("config.json");
        fs::create_dir(&destination).expect("create destination directory");

        let error = atomic_write(&destination, b"new").expect_err("replace must fail");

        assert!(matches!(error, FileError::AtomicReplace { .. }));
        assert!(destination.is_dir());
        assert!(temporary_files(dir.path()).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn failed_windows_replace_preserves_existing_file() {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 1;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.json");
        fs::write(&path, b"old").expect("seed file");
        let held_file = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .expect("hold destination without delete sharing");

        let result = atomic_write(&path, b"new");

        assert!(result.is_err());
        drop(held_file);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert!(temporary_files(dir.path()).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_an_internal_nul_without_touching_its_prefix() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let prefix = dir.path().join("victim");
        fs::write(&prefix, b"old").expect("seed prefix file");
        let invalid_name = OsString::from_wide(&[
            b'v' as u16,
            b'i' as u16,
            b'c' as u16,
            b't' as u16,
            b'i' as u16,
            b'm' as u16,
            0,
            b'x' as u16,
        ]);

        let result = atomic_write(&dir.path().join(invalid_name), b"new");

        assert!(result.is_err());
        assert_eq!(fs::read(prefix).unwrap(), b"old");
        assert!(temporary_files(dir.path()).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn atomic_write_supports_windows_long_paths() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut path = dir.path().to_path_buf();
        for index in 0..8 {
            path.push(format!("segment-{index}-012345678901234567890123456789"));
        }
        path.push("config.json");

        atomic_write(&path, b"old").expect("create long path");
        atomic_write(&path, b"new").expect("replace long path");

        assert_eq!(fs::read(path).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_supports_long_valid_file_names() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("a".repeat(240));
        fs::write(&path, b"old").expect("seed long file name");

        atomic_write(&path, b"new").expect("replace long file name");

        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(temporary_files(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn temporary_files_start_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let (path, file) = create_temporary_file(dir.path(), 0).expect("create temporary file");
        drop(file);

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn new_unix_files_start_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.json");

        atomic_write(&path, b"contents").expect("create file");

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_existing_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.json");
        fs::write(&path, b"old").expect("seed file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("set permissions");

        atomic_write(&path, b"new").expect("replace file");

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_write_uses_owner_only_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("credentials.json");

        atomic_write_private(&path, b"secret").expect("write private file");

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn json_output_is_pretty_printed_with_recursively_sorted_keys() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.json");
        let value = json!({"z": 1, "nested": {"z": 2, "a": 3}, "a": 4});

        let contents = write_json_file_with_contents(&path, &value).expect("write JSON");

        assert_eq!(
            String::from_utf8(contents).unwrap(),
            "{\n  \"a\": 4,\n  \"nested\": {\n    \"a\": 3,\n    \"z\": 2\n  },\n  \"z\": 1\n}"
        );
        assert_eq!(read_json_file::<Value>(&path).unwrap(), value);
    }

    #[test]
    fn read_json_reports_missing_and_malformed_files_separately() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.json");

        assert!(matches!(
            read_json_file::<Value>(&path),
            Err(FileError::NotFound { .. })
        ));

        fs::write(&path, "{").expect("write malformed JSON");
        assert!(matches!(
            read_json_file::<Value>(&path),
            Err(FileError::JsonParse { .. })
        ));
    }

    #[test]
    fn text_read_and_write_share_file_errors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("notes.txt");

        assert!(matches!(
            read_text_file(&path),
            Err(FileError::NotFound { .. })
        ));
        write_text_file(&path, "hello").expect("write text");
        assert_eq!(read_text_file(&path).unwrap(), "hello");
    }
}
