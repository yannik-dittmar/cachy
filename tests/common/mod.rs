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

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use rustix::fs::{StatVfs, Timestamps, statvfs};
use rustix::process::{Pid, Signal, kill_process};
use tempfile::TempDir;

const CACHE_BRANCH_FS: &str = "CACHY_TEST_CACHE_BRANCH_FS";
const BACKING_BRANCH_FS: &str = "CACHY_TEST_BACKING_BRANCH_FS";
const PRIVILEGED: &str = "CACHY_TEST_PRIVILEGED";
/// Sets how long a fault-injection build of cachy waits for freed space.
const FREED_WAIT: &str = "CACHY_FREED_WAIT_MS";

/// The staging directory at the root of each branch.
pub const STAGING: &str = ".cachy-staging";

/// A filesystem kind, recognised by its `statfs` magic number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsKind {
    Tmpfs,
    Ext4,
    Zfs,
    Other,
}

impl FsKind {
    fn of(path: &Path) -> FsKind {
        let magic = rustix::fs::statfs(path)
            .unwrap_or_else(|e| panic!("statfs {}: {e}", path.display()))
            .f_type as i64;
        match magic {
            0x0102_1994 => FsKind::Tmpfs,
            0xEF53 => FsKind::Ext4,
            0x2FC1_2FC1 => FsKind::Zfs,
            _ => FsKind::Other,
        }
    }

    fn parse(kind: &str) -> FsKind {
        match kind {
            "tmpfs" => FsKind::Tmpfs,
            "ext4" => FsKind::Ext4,
            "zfs" => FsKind::Zfs,
            _ => panic!("unknown filesystem kind `{kind}`: expected tmpfs, ext4 or zfs"),
        }
    }
}

/// Where a fixture creates branches of one role: a directory and, if the
/// environment names one, the filesystem kind it must be on.
fn branch_location(var: &str, default: Option<(FsKind, &str)>) -> (Option<FsKind>, PathBuf) {
    match std::env::var(var) {
        Ok(value) => {
            let (kind, dir) = value
                .split_once(':')
                .unwrap_or_else(|| panic!("{var}=`{value}`: expected <kind>:<dir>"));
            (Some(FsKind::parse(kind)), PathBuf::from(dir))
        }
        Err(_) => match default {
            Some((fs, dir)) => (Some(fs), PathBuf::from(dir)),
            None => (None, std::env::temp_dir()),
        },
    }
}

fn create_branch(var: &str, default: Option<(FsKind, &str)>, prefix: &str) -> (TempDir, FsKind) {
    let (expected, dir) = branch_location(var, default);
    let branch = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&dir)
        .unwrap_or_else(|e| panic!("create a branch in {}: {e}", dir.display()));
    let fs = FsKind::of(branch.path());
    if let Some(expected) = expected {
        assert_eq!(fs, expected, "{} is not on {expected:?}", dir.display());
    }
    (branch, fs)
}

fn privileged() -> bool {
    std::env::var_os(PRIVILEGED).is_some_and(|v| v == "1")
}

/// Notes on stderr that the calling test is skipping its checks; the test
/// then returns early. CI shows the note (`--show-output`).
pub fn skip(reason: &str) {
    eprintln!("skipped: {reason}");
}

/// A child process starts with a copy of its parent's descriptors and keeps
/// it until it execs. A file that one test thread has open for writing can
/// thus stay open in a child that another thread spawned meanwhile, and make
/// cachy's lease on it fail as if the file were in use. Test files are written
/// holding this lock exclusively, and children are spawned holding it shared:
/// `Command::spawn` returns only once the child has exec'd, which closes the
/// copies.
static SPAWN: RwLock<()> = RwLock::new(());

/// Spawns `command` so that it holds no test file open for writing.
pub fn spawn(command: &mut Command) -> std::io::Result<Child> {
    let _spawning = SPAWN.read().unwrap_or_else(|e| e.into_inner());
    command.spawn()
}

/// Like [`Command::output`], but spawns `command` with [`spawn`].
pub fn output(command: &mut Command) -> std::io::Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn(command)?.wait_with_output()
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
    pub cache_branch_fs: FsKind,
    pub backing_branch_fs: FsKind,
    scratch: TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let (cache_branch, cache_branch_fs) = create_branch(
            CACHE_BRANCH_FS,
            Some((FsKind::Tmpfs, "/dev/shm")),
            "cachy-cache-",
        );
        let (backing_branch, backing_branch_fs) =
            create_branch(BACKING_BRANCH_FS, None, "cachy-backing-");
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

    pub fn on_cache_branch(&self, rel: &str) -> PathBuf {
        self.cache_branch.path().join(rel)
    }

    pub fn on_backing_branch(&self, rel: &str) -> PathBuf {
        self.backing_branch.path().join(rel)
    }

    /// Where a move of `rel` keeps the retired source on the cache branch.
    pub fn staged_on_cache_branch(&self, rel: &str) -> PathBuf {
        self.on_cache_branch(STAGING).join(rel)
    }

    /// Where a move of `rel` writes its copy on the backing branch before
    /// publishing it.
    pub fn staged_on_backing_branch(&self, rel: &str) -> PathBuf {
        self.on_backing_branch(STAGING).join(rel)
    }

    pub fn assert_no_staging_directories(&self) {
        assert!(!self.on_cache_branch(STAGING).exists());
        assert!(!self.on_backing_branch(STAGING).exists());
    }

    /// Creates a file on the cache branch with the given content and mtime
    /// (seconds since the epoch).
    pub fn cache_branch_file(&self, rel: &str, content: &[u8], mtime: i64) {
        write_file(&self.on_cache_branch(rel), content, mtime);
    }

    /// Creates a hardlink group on the cache branch: `names[0]` with the
    /// given content and mtime, and every other name a link to it.
    pub fn cache_branch_hardlinks(&self, names: &[&str], content: &[u8], mtime: i64) {
        self.cache_branch_file(names[0], content, mtime);
        for name in &names[1..] {
            let link = self.on_cache_branch(name);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            fs::hard_link(self.on_cache_branch(names[0]), link).unwrap();
        }
    }

    pub fn backing_branch_file(&self, rel: &str, content: &[u8], mtime: i64) {
        write_file(&self.on_backing_branch(rel), content, mtime);
    }

    /// Creates a file where a move of `rel` keeps the retired source, as if
    /// cachy had crashed while retiring it.
    pub fn staged_cache_branch_file(&self, rel: &str, content: &[u8], mtime: i64) {
        write_file(&self.staged_on_cache_branch(rel), content, mtime);
    }

    /// Creates a file where a move of `rel` writes its copy, as if cachy had
    /// crashed while copying it.
    pub fn staged_backing_branch_file(&self, rel: &str, content: &[u8], mtime: i64) {
        write_file(&self.staged_on_backing_branch(rel), content, mtime);
    }

    /// Free space of the cache branch as cachy measures it. The filesystem is
    /// synced first: ZFS reports writes and frees, in free space and in
    /// `st_blocks`, only once their transaction group has synced, so without
    /// it a later sync could change free space between this measurement and
    /// cachy's. `syncfs` does not sync it on ZFS; `zpool sync` does, as root.
    pub fn cache_branch_free(&self) -> u64 {
        let dir = fs::File::open(self.cache_branch.path()).unwrap();
        rustix::fs::syncfs(&dir).unwrap();
        if self.cache_branch_fs == FsKind::Zfs {
            let dataset = zfs_dataset(self.cache_branch.path());
            let pool = dataset.split('/').next().unwrap();
            sudo(&[OsStr::new("zpool"), OsStr::new("sync"), OsStr::new(pool)])
                .unwrap_or_else(|e| panic!("sync the cache branch's zpool: {e}"));
        }
        free_bytes(&statvfs(self.cache_branch.path()).unwrap())
    }

    /// A valid config for this fixture's branches with the given threshold and
    /// target, as YAML scalars. Its minimum age is `0s`, because files that
    /// tests create always have a fresh ctime.
    pub fn config(&self, threshold: &str, target: &str) -> String {
        format!(
            "cache_branch: {}\nbacking_branches:\n  - {}\nthreshold: {threshold}\ntarget: {target}\nminimum_age: 0s\n",
            self.cache_branch.path().display(),
            self.backing_branch.path().display(),
        )
    }

    /// A config whose threshold and target lie far above any test
    /// filesystem's free space, so a run moves every eligible move unit.
    pub fn move_everything_config(&self) -> String {
        self.config("1T", "2T")
    }

    /// Like [`Fixture::move_everything_config`], but with the storage layout
    /// given by `layout`: YAML lines that set `cache_pool`,
    /// `backing_branches` or both, in place of the fixture's backing branch.
    pub fn layout_config(&self, layout: &str) -> String {
        format!(
            "cache_branch: {}\n{layout}threshold: 1T\ntarget: 2T\nminimum_age: 0s\n",
            self.cache_branch.path().display(),
        )
    }

    /// Another backing branch, in the same directory as the fixture's.
    pub fn extra_backing_branch(&self) -> TempDir {
        create_branch(BACKING_BRANCH_FS, None, "cachy-backing-").0
    }

    /// A config under which the check finds no work, so cachy only recovers.
    pub fn no_work_config(&self) -> String {
        self.config("0", "1")
    }

    /// A cache pool over this fixture's branches, or `None` (with a note on
    /// stderr) if mergerfs or passwordless sudo is unavailable outside the
    /// privileged tier.
    pub fn cache_pool(&self) -> Option<CachePool> {
        let branches = format!(
            "{}:{}",
            self.cache_branch.path().display(),
            self.backing_branch.path().display()
        );
        self.cache_pool_with(&branches, "")
    }

    /// A cache pool over `branches`, in mergerfs's branch syntax, with the
    /// extra mergerfs `options` (comma-separated, may be empty); `None` as for
    /// [`Fixture::cache_pool`].
    pub fn cache_pool_with(&self, branches: &str, options: &str) -> Option<CachePool> {
        match CachePool::mount(branches, options) {
            Ok(pool) => Some(pool),
            Err(e) if !privileged() => {
                skip(&format!("no cache pool: {e}"));
                None
            }
            Err(e) => panic!("no cache pool: {e}"),
        }
    }

    /// Runs `cachy run --config <file>` as root, through sudo unless the
    /// tests run as root. Check [`root_available`] first.
    pub fn run_as_root(&self, config: &str) -> Output {
        let command = self.command(config);
        let mut command = if rustix::process::geteuid().is_root() {
            command
        } else {
            let mut sudo = Command::new("sudo");
            sudo.arg("-n")
                .arg(command.get_program())
                .args(command.get_args());
            sudo
        };
        output(&mut command).expect("spawn cachy")
    }

    /// Runs `cachy run --config <file>` with the given config file content.
    pub fn run(&self, config: &str) -> Output {
        self.spawn(config)
            .wait_with_output()
            .expect("wait for cachy")
    }

    /// Starts `cachy run --config <file>` without waiting for it, so a test
    /// can act on the branches while the run is in progress.
    pub fn spawn(&self, config: &str) -> Child {
        spawn(&mut self.command(config)).expect("spawn cachy")
    }

    /// `cachy run --config <file>` with the given config file content, for a
    /// test to adjust before running it.
    ///
    /// A fault-injection build does not wait for freed space to show at the
    /// end of the run: on ZFS, the frees of files that a test has just
    /// written never show, because the files never reached the disk, and
    /// every run would wait in vain. [`Fixture::command_waiting_for_frees`]
    /// keeps the wait.
    pub fn command(&self, config: &str) -> Command {
        let mut command = self.command_waiting_for_frees(config);
        command.env(FREED_WAIT, "0");
        command
    }

    /// Like [`Fixture::command`], but the run waits for freed space to show
    /// before it warns that something holds it, as a normal build does.
    pub fn command_waiting_for_frees(&self, config: &str) -> Command {
        let path = self.scratch.path().join("cachy.yml");
        fs::write(&path, config).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_cachy"));
        command
            .args(["run", "--config", path.to_str().unwrap()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
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
/// filesystems with ENOSPC, so the cache pool sets 1M.
///
/// Like a production cache pool, it is mounted by root (through passwordless sudo)
/// with `allow_other`; it is unmounted on drop.
pub struct CachePool {
    mountpoint: TempDir,
}

impl CachePool {
    fn mount(branches: &str, options: &str) -> Result<CachePool, String> {
        let mountpoint = tempfile::Builder::new()
            .prefix("cachy-pool-")
            .tempdir()
            .map_err(|e| e.to_string())?;
        let mut all_options =
            "category.create=ff,minfreespace=1M,allow_other,fsname=cachy-test-pool".to_owned();
        if !options.is_empty() {
            all_options = format!("{all_options},{options}");
        }
        sudo(&[
            OsStr::new("mergerfs"),
            OsStr::new("-o"),
            OsStr::new(&all_options),
            OsStr::new(branches),
            mountpoint.path().as_os_str(),
        ])?;
        Ok(CachePool { mountpoint })
    }

    pub fn mountpoint(&self) -> &Path {
        self.mountpoint.path()
    }

    /// The path of `rel` as applications see it, through the cache pool.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.mountpoint.path().join(rel)
    }

    /// Creates a file through the cache pool with the given content and mtime
    /// (seconds since the epoch).
    pub fn file(&self, rel: &str, content: &[u8], mtime: i64) {
        write_file(&self.path(rel), content, mtime);
    }
}

impl Drop for CachePool {
    fn drop(&mut self) {
        if let Err(e) = sudo(&[OsStr::new("umount"), self.mountpoint.path().as_os_str()]) {
            // Leave the mountpoint behind rather than delete through the cache
            // pool.
            eprintln!("cannot unmount the cache pool: {e}");
            self.mountpoint.disable_cleanup(true);
        }
    }
}

/// A directory that stands in for a cache pool: its `.mergerfs` file carries
/// the `user.mergerfs.*` xattrs that mergerfs's runtime interface serves, so
/// layout discovery can be tested without mergerfs.
pub struct FakeCachePool {
    dir: TempDir,
}

impl FakeCachePool {
    /// A fake cache pool whose branches read as `branches`, in mergerfs's
    /// format (`<path>=<mode>[,<minfreespace>]`, separated by `:`), with
    /// `minfreespace` 1M (as small test filesystems need, like [`CachePool`])
    /// and `moveonenospc` off.
    pub fn new(branches: &str) -> FakeCachePool {
        let pool = FakeCachePool {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::write(pool.dir.path().join(".mergerfs"), b"").unwrap();
        pool.set("branches", branches);
        pool.set("minfreespace", "1M");
        pool.set("moveonenospc", "false");
        pool
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Sets the value that `user.mergerfs.<key>` reads as.
    pub fn set(&self, key: &str, value: &str) {
        set_xattr(
            &self.dir.path().join(".mergerfs"),
            &format!("user.mergerfs.{key}"),
            value.as_bytes(),
        )
        .unwrap();
    }
}

/// A backing branch on a tmpfs of its own, of a given size, so that tests can
/// give branches different free space. Mounted through passwordless sudo,
/// owned by the invoking user, and unmounted on drop.
pub struct TmpfsBranch {
    dir: TempDir,
}

impl TmpfsBranch {
    /// A tmpfs branch of `size` (such as `4M`), or `None` (with a note on
    /// stderr) if mounting is unavailable outside the privileged tier.
    pub fn mount(size: &str) -> Option<TmpfsBranch> {
        let dir = tempfile::Builder::new()
            .prefix("cachy-tmpfs-")
            .tempdir()
            .unwrap();
        let options = format!(
            "size={size},uid={},gid={},mode=0755",
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw()
        );
        let mounted = sudo(&[
            OsStr::new("mount"),
            OsStr::new("-t"),
            OsStr::new("tmpfs"),
            OsStr::new("-o"),
            OsStr::new(&options),
            OsStr::new("cachy-test-branch"),
            dir.path().as_os_str(),
        ]);
        match mounted {
            Ok(()) => Some(TmpfsBranch { dir }),
            Err(e) if !privileged() => {
                skip(&format!("no tmpfs branch: {e}"));
                None
            }
            Err(e) => panic!("no tmpfs branch: {e}"),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for TmpfsBranch {
    fn drop(&mut self) {
        if let Err(e) = sudo(&[OsStr::new("umount"), self.dir.path().as_os_str()]) {
            eprintln!("cannot unmount the tmpfs branch: {e}");
            self.dir.disable_cleanup(true);
        }
    }
}

/// Runs a command as root through passwordless sudo, which the privileged tier
/// has.
pub fn sudo(args: &[&OsStr]) -> Result<(), String> {
    let out =
        output(Command::new("sudo").arg("-n").args(args)).map_err(|e| format!("sudo: {e}"))?;
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

/// Whether the tests run as root or may use sudo (in the privileged tier).
/// If not, notes on stderr that the calling test skips its checks.
pub fn root_available() -> bool {
    let available = rustix::process::geteuid().is_root() || privileged();
    if !available {
        skip("acting as root needs the privileged tier");
    }
    available
}

/// Gives `path` to `uid` and `gid`, through sudo unless the tests run as
/// root. Check [`root_available`] first.
pub fn chown(path: &Path, uid: u32, gid: u32) {
    if rustix::process::geteuid().is_root() {
        std::os::unix::fs::lchown(path, Some(uid), Some(gid)).unwrap();
    } else {
        sudo(&[
            OsStr::new("chown"),
            OsStr::new("-h"),
            OsStr::new(&format!("{uid}:{gid}")),
            path.as_os_str(),
        ])
        .unwrap();
    }
}

/// Sets the mode of `path`, through sudo unless the tests run as root, for a
/// file that [`chown`] gave away. Check [`root_available`] first.
pub fn chmod_as_root(path: &Path, mode: u32) {
    if rustix::process::geteuid().is_root() {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    } else {
        sudo(&[
            OsStr::new("chmod"),
            OsStr::new(&format!("{mode:o}")),
            path.as_os_str(),
        ])
        .unwrap();
    }
}

/// The ZFS dataset that holds `path`, for commands such as
/// `sudo zfs snapshot <dataset>@<name>`.
pub fn zfs_dataset(path: &Path) -> String {
    let out = output(
        Command::new("findmnt")
            .args(["--noheadings", "--output", "SOURCE", "--target"])
            .arg(path),
    )
    .expect("spawn findmnt");
    assert!(out.status.success(), "findmnt {}", path.display());
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

pub fn cachy(args: &[&str]) -> Output {
    output(Command::new(env!("CARGO_BIN_EXE_cachy")).args(args)).expect("spawn cachy")
}

/// Waits until `path` exists, polling without sleeping so that a test can
/// catch a run at a step that lasts only milliseconds.
pub fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} did not appear",
            path.display()
        );
        std::hint::spin_loop();
    }
}

/// Stops a running cachy (`SIGSTOP`) where it is; [`resume`] continues it.
pub fn pause(child: &Child) {
    kill_process(Pid::from_child(child), Signal::STOP).unwrap();
}

pub fn resume(child: &Child) {
    kill_process(Pid::from_child(child), Signal::CONT).unwrap();
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The filesystem operations that a fault-injection build of cachy traced
/// (with `CACHY_FAULT_TRACE=1`) in `stderr`, as `(number, name)`, such as
/// `(7, "(write_at)")`.
pub fn traced_operations(stderr: &str) -> Vec<(u64, &str)> {
    stderr
        .lines()
        .filter_map(|line| {
            let op = line.strip_prefix("cachy: trace: operation ")?;
            let (n, name) = op.split_once(' ')?;
            Some((n.parse().unwrap(), name))
        })
        .collect()
}

/// Asserts the exit code and returns stderr, showing it on failure.
pub fn assert_exit(out: &Output, code: i32) -> String {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(code), "stderr:\n{err}");
    err
}

pub fn write_file(path: &Path, content: &[u8], mtime: i64) {
    let _writing = SPAWN.write().unwrap_or_else(|e| e.into_inner());
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

/// The extended attributes of `path` (not following a final symlink), by
/// name, including ACLs (`system.posix_acl_*`).
pub fn xattrs(path: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut names = vec![0; 64 * 1024];
    let len = match rustix::fs::llistxattr(path, &mut names[..]) {
        Ok(len) => len,
        Err(rustix::io::Errno::OPNOTSUPP) => 0,
        Err(e) => panic!("list xattrs of {}: {e}", path.display()),
    };
    names[..len]
        .split(|&b| b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| {
            let name = String::from_utf8(name.to_vec()).unwrap();
            let mut value = vec![0; 64 * 1024];
            let len = rustix::fs::lgetxattr(path, name.as_str(), &mut value[..])
                .unwrap_or_else(|e| panic!("get xattr {name} of {}: {e}", path.display()));
            value.truncate(len);
            (name, value)
        })
        .collect()
}

pub fn set_xattr(path: &Path, name: &str, value: &[u8]) -> rustix::io::Result<()> {
    rustix::fs::lsetxattr(path, name, value, rustix::fs::XattrFlags::empty())
}

/// Name of the xattr that holds a file's access ACL.
pub const ACCESS_ACL: &str = "system.posix_acl_access";
/// Name of the xattr that holds a directory's default ACL.
pub const DEFAULT_ACL: &str = "system.posix_acl_default";

/// An ACL in the kernel's xattr format, granting `user` all permissions on
/// top of the owner's `rwx`, the group's `r-x` and others' `---`. As a
/// default ACL, it leaves the owner able to use new directories.
pub fn acl_granting(user: u32) -> Vec<u8> {
    const USER_OBJ: u16 = 0x01;
    const USER: u16 = 0x02;
    const GROUP_OBJ: u16 = 0x04;
    const MASK: u16 = 0x10;
    const OTHER: u16 = 0x20;
    const UNDEFINED_ID: u32 = u32::MAX;
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, perm, id) in [
        (USER_OBJ, 7u16, UNDEFINED_ID),
        (USER, 7, user),
        (GROUP_OBJ, 5, UNDEFINED_ID),
        (MASK, 7, UNDEFINED_ID),
        (OTHER, 0, UNDEFINED_ID),
    ] {
        acl.extend(tag.to_le_bytes());
        acl.extend(perm.to_le_bytes());
        acl.extend(id.to_le_bytes());
    }
    acl
}

/// Sets `path`'s atime and mtime (seconds since the epoch).
pub fn set_times(path: &Path, atime: i64, mtime: i64) {
    let ts = |tv_sec| rustix::fs::Timespec { tv_sec, tv_nsec: 0 };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        path,
        &Timestamps {
            last_access: ts(atime),
            last_modification: ts(mtime),
        },
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
}
