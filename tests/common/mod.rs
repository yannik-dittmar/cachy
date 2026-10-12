//! Process-boundary test harness: drives the real `cachy` binary against real
//! directory trees.
//!
//! The backing branch must be on a different filesystem than the cache branch,
//! or a move would free no space on the cache branch. Which filesystems the
//! branches live on is chosen per test process, so the same tests run against
//! each pair of filesystems:
//!
//! - `CACHY_TEST_CACHE_BRANCH_FS=<kind>:<dir>` creates cache branches in
//!   `<dir>`, which must be on a filesystem of `<kind>` (`tmpfs`, `ext4` or
//!   `zfs`). The default is `tmpfs:/dev/shm`.
//! - `CACHY_TEST_BACKING_BRANCH_FS=<kind>:<dir>` does the same for backing
//!   branches. By default they live in the default temp directory, on
//!   whatever filesystem that is.
//! - `CACHY_TEST_PRIVILEGED=1` marks the privileged CI tier: fixtures that need
//!   privileges, such as [`CachePool`], fail the test when unavailable instead
//!   of skipping it.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};

use rustix::fs::{StatVfs, Timestamps, statvfs};
use tempfile::TempDir;

const CACHE_BRANCH_FS: &str = "CACHY_TEST_CACHE_BRANCH_FS";
const BACKING_BRANCH_FS: &str = "CACHY_TEST_BACKING_BRANCH_FS";
const PRIVILEGED: &str = "CACHY_TEST_PRIVILEGED";

/// A filesystem kind, recognised by its `statfs` magic number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fs {
    Tmpfs,
    Ext4,
    Zfs,
    Other,
}

impl Fs {
    fn of(path: &Path) -> Fs {
        let magic = rustix::fs::statfs(path)
            .unwrap_or_else(|e| panic!("statfs {}: {e}", path.display()))
            .f_type as i64;
        match magic {
            0x0102_1994 => Fs::Tmpfs,
            0xEF53 => Fs::Ext4,
            0x2FC1_2FC1 => Fs::Zfs,
            _ => Fs::Other,
        }
    }

    fn parse(kind: &str) -> Fs {
        match kind {
            "tmpfs" => Fs::Tmpfs,
            "ext4" => Fs::Ext4,
            "zfs" => Fs::Zfs,
            _ => panic!("unknown filesystem kind `{kind}`: expected tmpfs, ext4 or zfs"),
        }
    }
}

/// Where a fixture creates branches of one role: a directory and, if the
/// environment names one, the filesystem kind it must be on.
fn branch_location(var: &str, default: Option<(Fs, &str)>) -> (Option<Fs>, PathBuf) {
    match std::env::var(var) {
        Ok(value) => {
            let (kind, dir) = value
                .split_once(':')
                .unwrap_or_else(|| panic!("{var}=`{value}`: expected <kind>:<dir>"));
            (Some(Fs::parse(kind)), PathBuf::from(dir))
        }
        Err(_) => match default {
            Some((fs, dir)) => (Some(fs), PathBuf::from(dir)),
            None => (None, std::env::temp_dir()),
        },
    }
}

fn branch_dir(var: &str, default: Option<(Fs, &str)>, prefix: &str) -> (TempDir, Fs) {
    let (expected, dir) = branch_location(var, default);
    let branch = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&dir)
        .unwrap_or_else(|e| panic!("create a branch in {}: {e}", dir.display()));
    let fs = Fs::of(branch.path());
    if let Some(expected) = expected {
        assert_eq!(fs, expected, "{} is not on {expected:?}", dir.display());
    }
    (branch, fs)
}

fn privileged() -> bool {
    std::env::var_os(PRIVILEGED).is_some_and(|v| v == "1")
}

/// Tests that set thresholds from measured free space share the cache
/// branch's filesystem, so they must not run concurrently with each other.
static FREE_SPACE_TESTS: Mutex<()> = Mutex::new(());

pub fn free_space_lock() -> MutexGuard<'static, ()> {
    FREE_SPACE_TESTS.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Fixture {
    pub cache_branch: TempDir,
    pub backing_branch: TempDir,
    cache_branch_fs: Fs,
    backing_branch_fs: Fs,
    scratch: TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let (cache_branch, cache_branch_fs) = branch_dir(
            CACHE_BRANCH_FS,
            Some((Fs::Tmpfs, "/dev/shm")),
            "cachy-cache-",
        );
        let (backing_branch, backing_branch_fs) =
            branch_dir(BACKING_BRANCH_FS, None, "cachy-backing-");
        assert_ne!(
            device(cache_branch.path()),
            device(backing_branch.path()),
            "the backing branch must be on a different filesystem than the cache branch"
        );
        let scratch = tempfile::tempdir().unwrap();
        Fixture {
            cache_branch,
            backing_branch,
            cache_branch_fs,
            backing_branch_fs,
            scratch,
        }
    }

    pub fn cache_branch_fs(&self) -> Fs {
        self.cache_branch_fs
    }

    pub fn backing_branch_fs(&self) -> Fs {
        self.backing_branch_fs
    }

    pub fn on_cache_branch(&self, rel: &str) -> PathBuf {
        self.cache_branch.path().join(rel)
    }

    pub fn on_backing_branch(&self, rel: &str) -> PathBuf {
        self.backing_branch.path().join(rel)
    }

    /// Creates a file on the cache branch with the given content and mtime
    /// (seconds since the epoch).
    pub fn cache_branch_file(&self, rel: &str, content: &[u8], mtime: i64) {
        write_file(&self.on_cache_branch(rel), content, mtime);
    }

    pub fn backing_branch_file(&self, rel: &str, content: &[u8], mtime: i64) {
        write_file(&self.on_backing_branch(rel), content, mtime);
    }

    /// Free space of the cache branch as cachy measures it. The filesystem is
    /// synced first: ZFS reports writes and frees only once their transaction
    /// group has synced, so without it a later sync could change free space
    /// between this measurement and cachy's.
    pub fn cache_branch_free(&self) -> u64 {
        let dir = fs::File::open(self.cache_branch.path()).unwrap();
        rustix::fs::syncfs(&dir).unwrap();
        free_bytes(&statvfs(self.cache_branch.path()).unwrap())
    }

    /// A valid config for this fixture's branches with the given threshold and
    /// target, as YAML scalars.
    pub fn config(&self, threshold: &str, target: &str) -> String {
        format!(
            "cache_branch: {}\nbacking_branches:\n  - {}\nthreshold: {threshold}\ntarget: {target}\n",
            self.cache_branch.path().display(),
            self.backing_branch.path().display(),
        )
    }

    /// A cache pool over this fixture's branches, or `None` (with a note on
    /// stderr) if mergerfs or passwordless sudo is unavailable outside the
    /// privileged tier.
    pub fn cache_pool(&self) -> Option<CachePool> {
        match CachePool::mount(self.cache_branch.path(), self.backing_branch.path()) {
            Ok(pool) => Some(pool),
            Err(e) if !privileged() => {
                eprintln!("skipped: no cache pool: {e}");
                None
            }
            Err(e) => panic!("no cache pool: {e}"),
        }
    }

    /// Runs `cachy run --config <file>` with the given config file content.
    pub fn run(&self, config: &str) -> Output {
        let path = self.scratch.path().join("cachy.yml");
        fs::write(&path, config).unwrap();
        cachy(&["run", "--config", path.to_str().unwrap()])
    }

    /// Relative paths of all entries (files and directories) under a branch,
    /// sorted.
    pub fn tree(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }
}

/// A mergerfs cache pool with the cache branch first and
/// `category.create=ff`, so new files land on the cache branch. mergerfs's
/// default `minfreespace` (4G) would fail every create on small test
/// filesystems with ENOSPC, so the pool sets 1M.
///
/// Like a production pool, it is mounted by root (through passwordless sudo)
/// with `allow_other`; it is unmounted on drop.
pub struct CachePool {
    mountpoint: TempDir,
}

impl CachePool {
    fn mount(cache_branch: &Path, backing_branch: &Path) -> Result<CachePool, String> {
        let mountpoint = tempfile::Builder::new()
            .prefix("cachy-pool-")
            .tempdir()
            .map_err(|e| e.to_string())?;
        let branches = format!("{}:{}", cache_branch.display(), backing_branch.display());
        sudo(&[
            "mergerfs".as_ref(),
            "-o".as_ref(),
            "category.create=ff,minfreespace=1M,allow_other,fsname=cachy-test-pool".as_ref(),
            branches.as_ref(),
            mountpoint.path().as_os_str(),
        ])?;
        Ok(CachePool { mountpoint })
    }

    /// The path of `rel` as applications see it, through the pool.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.mountpoint.path().join(rel)
    }

    /// Creates a file through the pool with the given content and mtime
    /// (seconds since the epoch).
    pub fn file(&self, rel: &str, content: &[u8], mtime: i64) {
        write_file(&self.path(rel), content, mtime);
    }
}

impl Drop for CachePool {
    fn drop(&mut self) {
        if let Err(e) = sudo(&["umount".as_ref(), self.mountpoint.path().as_os_str()]) {
            // Leave the mountpoint behind rather than delete through the pool.
            eprintln!("cannot unmount the cache pool: {e}");
            self.mountpoint.disable_cleanup(true);
        }
    }
}

/// Runs a command as root through passwordless sudo.
fn sudo(args: &[&std::ffi::OsStr]) -> Result<(), String> {
    let out = Command::new("sudo")
        .arg("-n")
        .args(args)
        .output()
        .map_err(|e| format!("sudo: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "sudo {}: {}",
            args[0].display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

pub fn cachy(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cachy"))
        .args(args)
        .output()
        .expect("spawn cachy")
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Asserts the exit code and returns stderr, showing it on failure.
pub fn assert_exit(out: &Output, code: i32) -> String {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(code), "stderr:\n{err}");
    err
}

fn write_file(path: &Path, content: &[u8], mtime: i64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
    let ts = rustix::fs::Timespec {
        tv_sec: mtime,
        tv_nsec: 0,
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        path,
        &Timestamps {
            last_access: ts,
            last_modification: ts,
        },
        rustix::fs::AtFlags::empty(),
    )
    .unwrap();
}

fn free_bytes(s: &StatVfs) -> u64 {
    s.f_bavail * s.f_frsize
}

fn device(path: &Path) -> u64 {
    rustix::fs::stat(path).unwrap().st_dev
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        out.push(path.strip_prefix(root).unwrap().display().to_string());
        if path.is_dir() {
            walk(root, &path, out);
        }
    }
}
