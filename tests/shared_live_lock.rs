use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use cc_switch_core::fs::{
    shared_live_config_lock_path, SharedLiveConfigLock, SharedLiveConfigLockError,
};
use fs4::{FileExt, TryLockError};

#[test]
fn lock_is_exclusive_and_release_keeps_the_same_nonempty_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = shared_live_config_lock_path(directory.path());
    let guard = SharedLiveConfigLock::try_acquire(&path).unwrap();
    // Check contents only between guards: Windows locks can also affect I/O.
    // Never replace a held lock file.
    assert!(matches!(
        SharedLiveConfigLock::try_acquire(&path),
        Err(SharedLiveConfigLockError::Unavailable)
    ));
    drop(guard);
    fs::write(&path, b"persistent marker").unwrap();
    let guard = SharedLiveConfigLock::try_acquire(&path).unwrap();
    drop(guard);
    assert_eq!(fs::read(&path).unwrap(), b"persistent marker");
}

#[test]
fn different_lock_paths_do_not_contend() {
    let directory = tempfile::tempdir().unwrap();
    let _first = SharedLiveConfigLock::try_acquire(&directory.path().join("first.lock")).unwrap();
    let _second = SharedLiveConfigLock::try_acquire(&directory.path().join("second.lock")).unwrap();
}

#[test]
fn io_errors_are_distinct_from_contention() {
    let directory = tempfile::tempdir().unwrap();
    let blocker = directory.path().join("not-a-directory");
    fs::write(&blocker, b"keep").unwrap();
    assert!(matches!(
        SharedLiveConfigLock::try_acquire(&blocker.join("live-config.lock")),
        Err(SharedLiveConfigLockError::Io { .. })
    ));
    assert!(matches!(
        SharedLiveConfigLock::try_acquire(directory.path()),
        Err(SharedLiveConfigLockError::Io { .. })
    ));
    assert_eq!(fs::read(&blocker).unwrap(), b"keep");
}

#[cfg(unix)]
#[test]
fn new_and_existing_lock_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let path = shared_live_config_lock_path(directory.path());
    let guard = SharedLiveConfigLock::try_acquire(&path).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(guard);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let _guard = SharedLiveConfigLock::try_acquire(&path).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn stable_symlink_aliases_contend_on_the_same_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("canonical.lock");
    let alias = directory.path().join("alias.lock");
    let guard = SharedLiveConfigLock::try_acquire(&path).unwrap();
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    assert!(matches!(
        SharedLiveConfigLock::try_acquire(&alias),
        Err(SharedLiveConfigLockError::Unavailable)
    ));
    drop(guard);
    let _guard = SharedLiveConfigLock::try_acquire(&alias).unwrap();
    assert!(fs::symlink_metadata(&alias)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn early_error_releases_the_guard() {
    fn fail(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let _guard = SharedLiveConfigLock::try_acquire(path)?;
        Err("later host work failed".into())
    }
    let directory = tempfile::tempdir().unwrap();
    let path = shared_live_config_lock_path(directory.path());
    assert!(fail(&path).is_err());
    let _guard = SharedLiveConfigLock::try_acquire(&path).unwrap();
}

// The child deliberately uses the fs4 protocol already used by Lite, not the
// new guard. This is protocol interoperability, not consumer workflow parity.
#[test]
fn core_and_existing_protocol_contend_in_both_process_directions() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("fixture"), b"core-live-lock-v1").unwrap();
    let path = shared_live_config_lock_path(directory.path());
    let guard = SharedLiveConfigLock::try_acquire(&path).unwrap();
    let mut peer = ReapedChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "existing_protocol_worker",
                "--test-threads=1",
            ])
            .env("CC_SWITCH_LOCK_FIXTURE", directory.path())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    wait_for_marker(directory.path(), "blocked", &mut peer.0);
    drop(guard);
    peer.0.stdin.as_mut().unwrap().write_all(b"a").unwrap();
    wait_for_marker(directory.path(), "held", &mut peer.0);
    assert!(matches!(
        SharedLiveConfigLock::try_acquire(&path),
        Err(SharedLiveConfigLockError::Unavailable)
    ));
    peer.0.stdin.as_mut().unwrap().write_all(b"r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = peer.0.try_wait().unwrap() {
            assert!(status.success(), "lock peer failed: {status}");
            break;
        }
        assert!(Instant::now() < deadline, "lock peer did not exit");
        thread::sleep(Duration::from_millis(10));
    }
    let _guard = SharedLiveConfigLock::try_acquire(&path).unwrap();
}

struct ReapedChild(Child);

impl Drop for ReapedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_marker(directory: &Path, marker: &str, peer: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !directory.join(marker).is_file() {
        assert!(peer.try_wait().unwrap().is_none(), "lock peer exited early");
        assert!(
            Instant::now() < deadline,
            "lock peer did not signal {marker}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "spawned only by the isolated cross-process protocol test"]
fn existing_protocol_worker() {
    let directory = PathBuf::from(std::env::var_os("CC_SWITCH_LOCK_FIXTURE").unwrap())
        .canonicalize()
        .unwrap();
    let temporary = std::env::temp_dir().canonicalize().unwrap();
    assert!(directory.starts_with(&temporary) && directory != temporary);
    assert_eq!(
        fs::read(directory.join("fixture")).unwrap(),
        b"core-live-lock-v1"
    );
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(shared_live_config_lock_path(&directory))
        .unwrap();
    assert!(matches!(
        FileExt::try_lock(&file),
        Err(TryLockError::WouldBlock)
    ));
    fs::write(directory.join("blocked"), b"ready").unwrap();
    let mut input = [0];
    std::io::stdin().read_exact(&mut input).unwrap();
    assert_eq!(input, *b"a");
    FileExt::try_lock(&file).unwrap();
    fs::write(directory.join("held"), b"ready").unwrap();
    std::io::stdin().read_exact(&mut input).unwrap();
    assert_eq!(input, *b"r");
    drop(file);
}
