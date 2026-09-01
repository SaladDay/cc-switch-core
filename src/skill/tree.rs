use std::{
    ffi::OsStr,
    fs::{self, DirEntry, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use thiserror::Error;

const MAX_TREE_ENTRIES: usize = 10_000;
const MAX_TREE_BYTES: u64 = 512 * 1024 * 1024;
pub(super) const MAX_TREE_DEPTH: usize = 64;

pub(super) struct ScanBudget {
    entries: usize,
    bytes: u64,
    maximum_entries: usize,
    maximum_bytes: u64,
}

impl ScanBudget {
    pub(super) const fn new(maximum_entries: usize, maximum_bytes: u64) -> Self {
        Self {
            entries: 0,
            bytes: 0,
            maximum_entries,
            maximum_bytes,
        }
    }

    pub(super) fn charge_entries(&mut self, count: usize) -> Result<(), TreeError> {
        let next = self
            .entries
            .checked_add(count)
            .ok_or(TreeError::EntryLimit {
                limit: self.maximum_entries,
            })?;
        if next > self.maximum_entries {
            self.entries = self.maximum_entries;
            return Err(TreeError::EntryLimit {
                limit: self.maximum_entries,
            });
        }
        self.entries = next;
        Ok(())
    }

    pub(super) fn charge_bytes(&mut self, count: u64) -> Result<(), TreeError> {
        let next = self.bytes.checked_add(count).ok_or(TreeError::ByteLimit {
            limit: self.maximum_bytes,
        })?;
        if next > self.maximum_bytes {
            self.bytes = self.maximum_bytes;
            return Err(TreeError::ByteLimit {
                limit: self.maximum_bytes,
            });
        }
        self.bytes = next;
        Ok(())
    }

    pub(super) fn remaining_bytes(&self) -> u64 {
        self.maximum_bytes.saturating_sub(self.bytes)
    }

    pub(super) fn exhaust_bytes(&mut self) {
        self.bytes = self.maximum_bytes;
    }
}

#[derive(Default)]
struct TreeBudget {
    entries: usize,
    bytes: u64,
}

pub(super) fn digest_tree(
    root: &Path,
    skip_root_entry: Option<&OsStr>,
    aggregate: &mut ScanBudget,
) -> Result<[u8; 32], TreeError> {
    let mut tree = TreeBudget::default();
    let mut hasher = Sha256::new();
    hasher.update(b"cc-switch-skill-tree-v1\0");
    hash_directory(
        root,
        root,
        0,
        skip_root_entry,
        &mut tree,
        aggregate,
        &mut hasher,
    )?;
    Ok(hasher.finalize().into())
}

fn hash_directory(
    root: &Path,
    directory: &Path,
    depth: usize,
    skip_root_entry: Option<&OsStr>,
    tree: &mut TreeBudget,
    aggregate: &mut ScanBudget,
    hasher: &mut Sha256,
) -> Result<(), TreeError> {
    if depth > MAX_TREE_DEPTH {
        return Err(TreeError::DepthLimit {
            limit: MAX_TREE_DEPTH,
        });
    }
    let entries = read_entries(
        directory,
        tree,
        aggregate,
        (directory == root).then_some(skip_root_entry).flatten(),
    )?;
    for entry in entries {
        let path = entry.path();
        let relative = normalized_relative(root, &path)?;
        let metadata = fs::symlink_metadata(&path).map_err(|source| TreeError::Io {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_dir() {
            hasher.update(b"d\0");
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            hash_directory(
                root,
                &path,
                depth.saturating_add(1),
                skip_root_entry,
                tree,
                aggregate,
                hasher,
            )?;
        } else if metadata.file_type().is_file() {
            hasher.update(b"f\0");
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            hash_file(&path, &metadata, tree, aggregate, hasher)?;
        } else {
            return Err(TreeError::UnsupportedEntry { path });
        }
    }
    Ok(())
}

pub(super) fn copy_tree(
    source: &Path,
    destination: &Path,
    aggregate: &mut ScanBudget,
) -> Result<(), TreeError> {
    let mut tree = TreeBudget::default();
    copy_directory(source, source, destination, 0, &mut tree, aggregate)
}

fn copy_directory(
    root: &Path,
    source: &Path,
    destination: &Path,
    depth: usize,
    tree: &mut TreeBudget,
    aggregate: &mut ScanBudget,
) -> Result<(), TreeError> {
    if depth > MAX_TREE_DEPTH {
        return Err(TreeError::DepthLimit {
            limit: MAX_TREE_DEPTH,
        });
    }
    for entry in read_entries(source, tree, aggregate, None)? {
        let source_path = entry.path();
        let relative = source_path
            .strip_prefix(root)
            .map_err(|_| TreeError::InvalidPath {
                path: source_path.clone(),
            })?;
        let destination_path = destination.join(relative);
        let metadata = fs::symlink_metadata(&source_path).map_err(|source| TreeError::Io {
            path: source_path.clone(),
            source,
        })?;
        if metadata.file_type().is_dir() {
            create_private_directory(&destination_path)?;
            copy_directory(
                root,
                &source_path,
                destination,
                depth.saturating_add(1),
                tree,
                aggregate,
            )?;
            fs::set_permissions(&destination_path, metadata.permissions()).map_err(|source| {
                TreeError::Io {
                    path: destination_path.clone(),
                    source,
                }
            })?;
        } else if metadata.file_type().is_file() {
            copy_file(&source_path, &destination_path, &metadata, tree, aggregate)?;
        } else {
            return Err(TreeError::UnsupportedEntry { path: source_path });
        }
    }
    Ok(())
}

fn copy_file(
    source_path: &Path,
    destination_path: &Path,
    before: &fs::Metadata,
    tree: &mut TreeBudget,
    aggregate: &mut ScanBudget,
) -> Result<(), TreeError> {
    let mut source = File::open(source_path).map_err(|source| TreeError::Io {
        path: source_path.to_owned(),
        source,
    })?;
    let opened = source.metadata().map_err(|source| TreeError::Io {
        path: source_path.to_owned(),
        source,
    })?;
    if !opened.file_type().is_file() || opened.len() != before.len() {
        return Err(TreeError::Changed {
            path: source_path.to_owned(),
        });
    }
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination_path)
        .map_err(|source| TreeError::Io {
            path: destination_path.to_owned(),
            source,
        })?;

    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = source.read(&mut buffer).map_err(|source| TreeError::Io {
            path: source_path.to_owned(),
            source,
        })?;
        if count == 0 {
            break;
        }
        let count = u64::try_from(count).map_err(|_| TreeError::ByteLimit {
            limit: MAX_TREE_BYTES,
        })?;
        charge_file(count, tree, aggregate)?;
        destination
            .write_all(&buffer[..count as usize])
            .map_err(|source| TreeError::Io {
                path: destination_path.to_owned(),
                source,
            })?;
        copied = copied.saturating_add(count);
    }
    let after = source.metadata().map_err(|source| TreeError::Io {
        path: source_path.to_owned(),
        source,
    })?;
    if copied != before.len() || after.len() != before.len() {
        return Err(TreeError::Changed {
            path: source_path.to_owned(),
        });
    }
    destination
        .set_permissions(before.permissions())
        .and_then(|()| destination.sync_all())
        .map_err(|source| TreeError::Io {
            path: destination_path.to_owned(),
            source,
        })
}

fn read_entries(
    directory: &Path,
    tree: &mut TreeBudget,
    aggregate: &mut ScanBudget,
    skip: Option<&OsStr>,
) -> Result<Vec<DirEntry>, TreeError> {
    let reader = fs::read_dir(directory).map_err(|source| TreeError::Io {
        path: directory.to_owned(),
        source,
    })?;
    let mut entries = Vec::new();
    for entry in reader {
        let entry = entry.map_err(|source| TreeError::Io {
            path: directory.to_owned(),
            source,
        })?;
        if skip.is_some_and(|name| entry.file_name().as_os_str() == name) {
            continue;
        }
        tree.entries = tree.entries.saturating_add(1);
        if tree.entries > MAX_TREE_ENTRIES {
            return Err(TreeError::EntryLimit {
                limit: MAX_TREE_ENTRIES,
            });
        }
        aggregate.charge_entries(1)?;
        entries.push(entry);
    }
    entries.sort_by_key(DirEntry::file_name);
    Ok(entries)
}

fn hash_file(
    path: &Path,
    before: &fs::Metadata,
    tree: &mut TreeBudget,
    aggregate: &mut ScanBudget,
    hasher: &mut Sha256,
) -> Result<(), TreeError> {
    let mut file = File::open(path).map_err(|source| TreeError::Io {
        path: path.to_owned(),
        source,
    })?;
    hasher.update(before.len().to_le_bytes());
    let mut read_bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|source| TreeError::Io {
            path: path.to_owned(),
            source,
        })?;
        if count == 0 {
            break;
        }
        let count = u64::try_from(count).map_err(|_| TreeError::ByteLimit {
            limit: MAX_TREE_BYTES,
        })?;
        charge_file(count, tree, aggregate)?;
        read_bytes = read_bytes.saturating_add(count);
        hasher.update(&buffer[..count as usize]);
    }
    let after = fs::symlink_metadata(path).map_err(|source| TreeError::Io {
        path: path.to_owned(),
        source,
    })?;
    if !after.file_type().is_file() || before.len() != read_bytes || after.len() != read_bytes {
        return Err(TreeError::Changed {
            path: path.to_owned(),
        });
    }
    hasher.update(b"\0");
    Ok(())
}

fn charge_file(
    count: u64,
    tree: &mut TreeBudget,
    aggregate: &mut ScanBudget,
) -> Result<(), TreeError> {
    tree.bytes = tree.bytes.saturating_add(count);
    if tree.bytes > MAX_TREE_BYTES {
        return Err(TreeError::ByteLimit {
            limit: MAX_TREE_BYTES,
        });
    }
    aggregate.charge_bytes(count)
}

fn normalized_relative(root: &Path, path: &Path) -> Result<String, TreeError> {
    path.strip_prefix(root)
        .map_err(|_| TreeError::InvalidPath {
            path: path.to_owned(),
        })?
        .to_str()
        .map(|relative| relative.replace('\\', "/"))
        .ok_or_else(|| TreeError::InvalidPath {
            path: path.to_owned(),
        })
}

fn create_private_directory(path: &Path) -> Result<(), TreeError> {
    crate::fs::create_private_directory(path).map_err(|source| TreeError::Io {
        path: path.to_owned(),
        source,
    })
}

#[derive(Debug, Error)]
pub(super) enum TreeError {
    #[error("Skill tree exceeds the {limit}-entry limit")]
    EntryLimit { limit: usize },
    #[error("Skill tree exceeds the {limit}-byte limit")]
    ByteLimit { limit: u64 },
    #[error("Skill tree exceeds the {limit}-level depth limit")]
    DepthLimit { limit: usize },
    #[error("unsupported entry in Skill tree: {path:?}")]
    UnsupportedEntry { path: PathBuf },
    #[error("Skill tree path is not portable: {path:?}")]
    InvalidPath { path: PathBuf },
    #[error("Skill tree changed while it was being read: {path:?}")]
    Changed { path: PathBuf },
    #[error("Skill tree I/O failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_and_copy_reject_symlinks() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("SKILL.md"), "# Demo\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(source.join("SKILL.md"), source.join("alias")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(source.join("SKILL.md"), source.join("alias")).unwrap();
        fs::create_dir(&destination).unwrap();

        let mut budget = ScanBudget::new(20_000, 1024 * 1024 * 1024);
        assert!(matches!(
            digest_tree(&source, None, &mut budget),
            Err(TreeError::UnsupportedEntry { .. })
        ));
        let mut budget = ScanBudget::new(20_000, 1024 * 1024 * 1024);
        assert!(matches!(
            copy_tree(&source, &destination, &mut budget),
            Err(TreeError::UnsupportedEntry { .. })
        ));
    }

    #[test]
    fn copy_stops_before_writing_a_chunk_over_the_budget() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("SKILL.md"), "four").unwrap();

        let mut budget = ScanBudget::new(10, 3);
        assert!(matches!(
            copy_tree(&source, &destination, &mut budget),
            Err(TreeError::ByteLimit { limit: 3 })
        ));
        assert_eq!(fs::metadata(destination.join("SKILL.md")).unwrap().len(), 0);
    }
}
