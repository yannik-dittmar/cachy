//! Process-boundary test harness: drives the real `cachy` binary against real
//! directory trees. The cache branch lives on tmpfs (`/dev/shm`), the backing
//! branch on a different filesystem, so a move really frees cache free space.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};

use rustix::fs::{StatVfs, Timestamps, statvfs};
use tempfile::TempDir;

const TMPFS_ROOT: &str = "/dev/shm";
const TMPFS_MAGIC: i64 = 0x0102_1994;

/// Tests that set thresholds from measured free space share one tmpfs, so
/// they must not run concurrently with each other.
static FREE_SPACE: Mutex<()> = Mutex::new(());

pub fn free_space_lock() -> MutexGuard<'static, ()> {
    FREE_SPACE.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Fixture {
    pub cache: TempDir,
    pub backing: TempDir,
    scratch: TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let tmpfs = rustix::fs::statfs(TMPFS_ROOT).expect("statfs /dev/shm");
        assert_eq!(
            tmpfs.f_type as i64, TMPFS_MAGIC,
            "{TMPFS_ROOT} must be tmpfs"
        );
        let cache = tempfile::Builder::new()
            .prefix("cachy-cache-")
            .tempdir_in(TMPFS_ROOT)
            .unwrap();
        let backing = tempfile::Builder::new()
            .prefix("cachy-backing-")
            .tempdir()
            .unwrap();
        assert_ne!(
            device(cache.path()),
            device(backing.path()),
            "the backing branch must be on a different filesystem than the cache branch"
        );
        let scratch = tempfile::tempdir().unwrap();
        Fixture {
            cache,
            backing,
            scratch,
        }
    }

    pub fn cache_path(&self, rel: &str) -> PathBuf {
        self.cache.path().join(rel)
    }

    pub fn backing_path(&self, rel: &str) -> PathBuf {
        self.backing.path().join(rel)
    }

    /// Creates a file on the cache branch with the given content and mtime
    /// (seconds since the epoch).
    pub fn cache_file(&self, rel: &str, content: &[u8], mtime: i64) {
        write_file(&self.cache_path(rel), content, mtime);
    }

    pub fn backing_file(&self, rel: &str, content: &[u8], mtime: i64) {
        write_file(&self.backing_path(rel), content, mtime);
    }

    /// Free space of the cache branch as cachy measures it.
    pub fn cache_free(&self) -> u64 {
        free_bytes(&statvfs(self.cache.path()).unwrap())
    }

    /// A valid config for this fixture's branches with the given threshold and
    /// target, as YAML scalars.
    pub fn config(&self, threshold: &str, target: &str) -> String {
        format!(
            "cache_branch: {}\nbacking_branches:\n  - {}\nthreshold: {threshold}\ntarget: {target}\n",
            self.cache.path().display(),
            self.backing.path().display(),
        )
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
