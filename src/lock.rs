//! The instance lock: one run per cache branch at a time.
//!
//! The instance lock is an `flock` on a file in the state directory, named
//! after the cache branch's canonical path. The kernel releases it when the
//! process exits, however it exits, so a crash never leaves it held. The file
//! itself stays: deleting it could let two runs lock two different files.

use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustix::fs::{FlockOperation, flock};

/// The mode of the state directory, if cachy creates it.
const STATE_DIR_MODE: u32 = 0o755;
/// The mode of the files that hold instance locks.
const LOCK_MODE: u32 = 0o644;

/// The error of an invocation that found the instance lock held by another,
/// which the daemon tells apart from other refusals with
/// `anyhow::Error::downcast_ref`.
#[derive(Debug)]
pub struct Held {
    cache_branch: PathBuf,
    lock_file: PathBuf,
}

impl fmt::Display for Held {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "another cachy run on cache branch {} is in progress (it holds {})",
            self.cache_branch.display(),
            self.lock_file.display()
        )
    }
}

impl std::error::Error for Held {}

/// The instance lock on a cache branch, held until dropped.
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Takes the lock on `cache_branch` in `state_dir`, creating the state
    /// directory if needed. Fails with [`Held`] if another run holds it.
    pub fn acquire(state_dir: &Path, cache_branch: &Path) -> Result<InstanceLock> {
        let canonical = fs::canonicalize(cache_branch)
            .with_context(|| format!("cannot resolve cache branch {}", cache_branch.display()))?;
        DirBuilder::new()
            .recursive(true)
            .mode(STATE_DIR_MODE)
            .create(state_dir)
            .with_context(|| format!("cannot create state directory {}", state_dir.display()))?;
        let path = lock_path(state_dir, &canonical);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(LOCK_MODE)
            .open(&path)
            .with_context(|| format!("cannot open instance lock {}", path.display()))?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(InstanceLock { _file: file }),
            Err(e) if io::Error::from(e).kind() == io::ErrorKind::WouldBlock => Err(Held {
                cache_branch: canonical,
                lock_file: path,
            }
            .into()),
            Err(e) => Err(e).with_context(|| format!("cannot lock {}", path.display())),
        }
    }
}

/// The file that holds the instance lock of the cache branch at the canonical
/// path `cache_branch`, named after the path escaped as systemd escapes
/// paths, so that distinct paths never share one. `/mnt/cache` becomes
/// `mnt-cache.lock`.
fn lock_path(state_dir: &Path, cache_branch: &Path) -> PathBuf {
    let bytes = cache_branch.as_os_str().as_bytes();
    let trimmed = bytes.strip_prefix(b"/").unwrap_or(bytes);
    let mut name = String::new();
    if trimmed.is_empty() {
        name.push('-');
    }
    for (i, &b) in trimmed.iter().enumerate() {
        match b {
            b'/' => name.push('-'),
            b'.' if i == 0 => name.push_str("\\x2e"),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.' => name.push(b as char),
            _ => name.push_str(&format!("\\x{b:02x}")),
        }
    }
    state_dir.join(format!("{name}.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(path: &str) -> String {
        lock_path(Path::new("/s"), Path::new(path))
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn instance_locks_are_named_after_the_escaped_path() {
        assert_eq!(name("/mnt/cache"), "mnt-cache.lock");
        assert_eq!(name("/"), "-.lock");
        assert_eq!(name("/mnt/my-cache"), "mnt-my\\x2dcache.lock");
        assert_eq!(name("/mnt/.cache"), "mnt-.cache.lock");
        assert_eq!(name("/.cache"), "\\x2ecache.lock");
        assert_ne!(name("/a-b"), name("/a/b"));
    }
}
