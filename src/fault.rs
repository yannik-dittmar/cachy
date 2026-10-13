//! Test-only fault injection, built only with the `fault-injection` feature.
//!
//! [`FaultyFs`] performs every filesystem operation through [`RealFs`], except
//! the one named by `CACHY_FAULT=<fault>@<n>` (the `n`th operation of the
//! process, counting from 1), or every operation of the given name with
//! `<fault>@<name>`, such as `einval@rename_noreplace`. A `kill` fault kills
//! the process with `SIGKILL` instead, and `eio`, `enospc`, `exdev`, `eintr`
//! or `einval` fails the operation with that errno. A `corrupt` fault lets a
//! write succeed with the first byte of its data flipped, as faulty storage
//! would; on any other operation it does nothing. Either way cachy first
//! notes the fault on stderr. A comma-separated list injects several faults,
//! such as one that starts a rollback and one during it: `eio@20,kill@23`.
//!
//! Lease queries are not counted, nor can they fail: how often a run polls its
//! leases depends on timing, and the operation numbers must not.
//!
//! `CACHY_FAULT_TRACE=1` notes every counted operation on stderr, as
//! `cachy: trace: operation <n> (<name>)`, so that a test can find the
//! number of the operation it wants to inject a fault at.
//!
//! The capability probe's operations are counted apart, so that the probe
//! does not shift the numbers of a run's other operations, and take their
//! faults from `CACHY_PROBE_FAULT`, in the same syntax. Their trace lines read
//! `cachy: trace: probe operation <n> (<name>)`. `CACHY_ZFS_VERSION=<version>`
//! makes the probe take the cache branch for OpenZFS with that module
//! version, as `/sys/module/zfs/version` would give it.
//!
//! `CACHY_RETIRE_WAIT_MS=<ms>` shortens the retire wait, and
//! `CACHY_FREED_WAIT_MS=<ms>` the wait for freed space at the end of a run, so
//! that tests that run cachy hundreds of times do not wait each time.
//! `CACHY_WRITE_DELAY_MS=<ms>` makes every write wait first, so that a test
//! can watch moves while they copy.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use rustix::fs::Timespec;

use crate::fsops::{FsOps, FsStat, Lease, RealFs, Stat};

const FAULT: &str = "CACHY_FAULT";
const PROBE_FAULT: &str = "CACHY_PROBE_FAULT";
const ZFS_VERSION: &str = "CACHY_ZFS_VERSION";
const TRACE: &str = "CACHY_FAULT_TRACE";
const RETIRE_WAIT: &str = "CACHY_RETIRE_WAIT_MS";
const FREED_WAIT: &str = "CACHY_FREED_WAIT_MS";
const WRITE_DELAY: &str = "CACHY_WRITE_DELAY_MS";

/// The retire wait set by `CACHY_RETIRE_WAIT_MS`, if any.
pub fn retire_wait() -> Option<Duration> {
    static WAIT: OnceLock<Option<Duration>> = OnceLock::new();
    *WAIT.get_or_init(|| wait_from_env(RETIRE_WAIT))
}

/// The wait for freed space set by `CACHY_FREED_WAIT_MS`, if any.
pub fn freed_wait() -> Option<Duration> {
    static WAIT: OnceLock<Option<Duration>> = OnceLock::new();
    *WAIT.get_or_init(|| wait_from_env(FREED_WAIT))
}

/// The OpenZFS module version set by `CACHY_ZFS_VERSION`, if any.
pub fn zfs_version() -> Option<String> {
    std::env::var(ZFS_VERSION).ok()
}

fn wait_from_env(var: &str) -> Option<Duration> {
    let ms = std::env::var(var).ok()?;
    let ms = ms
        .parse()
        .unwrap_or_else(|_| panic!("{var}=`{ms}`: expected milliseconds"));
    Some(Duration::from_millis(ms))
}

#[derive(Clone, Copy)]
enum Fault {
    Kill,
    Errno(i32),
    Corrupt,
}

/// Whether the operation, if it is a write, is to corrupt its data.
#[derive(PartialEq, Eq)]
enum CorruptWrite {
    Yes,
    No,
}

/// The operations a fault replaces.
#[derive(PartialEq, Eq)]
enum Target {
    /// The operation with this number.
    Number(u64),
    /// Every operation of this name.
    Name(String),
}

pub struct FaultyFs {
    /// The faults, each with the operations it replaces.
    faults: Vec<(Fault, Target)>,
    /// What trace lines and fault notes call an operation.
    label: &'static str,
    /// Operations performed so far, by all threads. With moves on more than
    /// one backing branch at once, the numbers depend on timing.
    count: AtomicU64,
    trace: bool,
    /// How long each write waits before it starts.
    write_delay: Option<Duration>,
}

impl FaultyFs {
    /// Injects the faults that `CACHY_FAULT` names, if it is set.
    pub fn from_env() -> Result<FaultyFs> {
        FaultyFs::from_var(FAULT, "operation")
    }

    /// The capability probe's operations, counted apart from the others,
    /// with the faults that `CACHY_PROBE_FAULT` names, if it is set.
    pub fn probe_from_env() -> Result<FaultyFs> {
        FaultyFs::from_var(PROBE_FAULT, "probe operation")
    }

    fn from_var(var: &str, label: &'static str) -> Result<FaultyFs> {
        let faults = match std::env::var(var) {
            Ok(spec) => spec
                .split(',')
                .map(parse)
                .collect::<Result<_>>()
                .with_context(|| format!("invalid {var}=`{spec}`"))?,
            Err(_) => Vec::new(),
        };
        Ok(FaultyFs {
            faults,
            label,
            count: AtomicU64::new(0),
            trace: std::env::var_os(TRACE).is_some_and(|v| v == "1"),
            write_delay: wait_from_env(WRITE_DELAY),
        })
    }

    /// Counts the operation `name`, and injects the fault meant for it, if
    /// any. A `corrupt` fault is left to the operation.
    fn count_op(&self, name: &str) -> io::Result<CorruptWrite> {
        let n = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        let label = self.label;
        if self.trace {
            eprintln!("cachy: trace: {label} {n} ({name})");
        }
        let fault = self.faults.iter().find(|(_, target)| match target {
            Target::Number(at) => *at == n,
            Target::Name(at) => at == name,
        });
        match fault {
            Some((Fault::Kill, _)) => {
                eprintln!("cachy: fault injection: kill at {label} {n} ({name})");
                // SAFETY: kill(2) on our own pid touches no memory.
                unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
                unreachable!("SIGKILL to self returned");
            }
            Some(&(Fault::Errno(errno), _)) => {
                let e = io::Error::from_raw_os_error(errno);
                eprintln!("cachy: fault injection: {e} at {label} {n} ({name})");
                Err(e)
            }
            Some((Fault::Corrupt, _)) => {
                eprintln!("cachy: fault injection: corrupt at {label} {n} ({name})");
                Ok(CorruptWrite::Yes)
            }
            None => Ok(CorruptWrite::No),
        }
    }
}

fn parse(spec: &str) -> Result<(Fault, Target)> {
    let Some((fault, at)) = spec.split_once('@') else {
        bail!("expected <fault>@<operation number or name>");
    };
    let fault = match fault {
        "kill" => Fault::Kill,
        "eio" => Fault::Errno(libc::EIO),
        "enospc" => Fault::Errno(libc::ENOSPC),
        "exdev" => Fault::Errno(libc::EXDEV),
        "eintr" => Fault::Errno(libc::EINTR),
        "einval" => Fault::Errno(libc::EINVAL),
        "corrupt" => Fault::Corrupt,
        _ => bail!(
            "unknown fault `{fault}`: expected kill, eio, enospc, exdev, eintr, einval or corrupt"
        ),
    };
    let target = if at.bytes().all(|b| b.is_ascii_digit()) {
        let n = at.parse().context("invalid operation number")?;
        if n == 0 {
            bail!("operations are numbered from 1");
        }
        Target::Number(n)
    } else {
        Target::Name(at.to_owned())
    };
    Ok((fault, target))
}

impl FsOps for FaultyFs {
    fn open_dir(&self, dir: BorrowedFd, path: &Path) -> io::Result<OwnedFd> {
        self.count_op("open_dir")?;
        RealFs.open_dir(dir, path)
    }

    fn open_read(&self, dir: BorrowedFd, path: &Path) -> io::Result<OwnedFd> {
        self.count_op("open_read")?;
        RealFs.open_read(dir, path)
    }

    fn create_new(&self, dir: BorrowedFd, path: &Path, mode: u32) -> io::Result<OwnedFd> {
        self.count_op("create_new")?;
        RealFs.create_new(dir, path, mode)
    }

    fn mkdir(&self, dir: BorrowedFd, path: &Path, mode: u32) -> io::Result<()> {
        self.count_op("mkdir")?;
        RealFs.mkdir(dir, path, mode)
    }

    fn symlink(&self, target: &OsStr, dir: BorrowedFd, path: &Path) -> io::Result<()> {
        self.count_op("symlink")?;
        RealFs.symlink(target, dir, path)
    }

    fn read_link(&self, dir: BorrowedFd, path: &Path) -> io::Result<OsString> {
        self.count_op("read_link")?;
        RealFs.read_link(dir, path)
    }

    fn fstat(&self, fd: BorrowedFd) -> io::Result<Stat> {
        self.count_op("fstat")?;
        RealFs.fstat(fd)
    }

    fn stat_at(&self, dir: BorrowedFd, path: &Path) -> io::Result<Stat> {
        self.count_op("stat_at")?;
        RealFs.stat_at(dir, path)
    }

    fn read(&self, fd: BorrowedFd, buf: &mut [u8]) -> io::Result<usize> {
        self.count_op("read")?;
        RealFs.read(fd, buf)
    }

    fn read_at(&self, fd: BorrowedFd, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.count_op("read_at")?;
        RealFs.read_at(fd, buf, offset)
    }

    fn write_at(&self, fd: BorrowedFd, buf: &[u8], offset: u64) -> io::Result<usize> {
        if let Some(delay) = self.write_delay {
            std::thread::sleep(delay);
        }
        if self.count_op("write_at")? == CorruptWrite::Yes {
            let mut corrupted = buf.to_vec();
            if let Some(first) = corrupted.first_mut() {
                *first ^= 0xff;
            }
            return RealFs.write_at(fd, &corrupted, offset);
        }
        RealFs.write_at(fd, buf, offset)
    }

    fn seek_data(&self, fd: BorrowedFd, offset: u64) -> io::Result<Option<u64>> {
        self.count_op("seek_data")?;
        RealFs.seek_data(fd, offset)
    }

    fn seek_hole(&self, fd: BorrowedFd, offset: u64) -> io::Result<u64> {
        self.count_op("seek_hole")?;
        RealFs.seek_hole(fd, offset)
    }

    fn set_len(&self, fd: BorrowedFd, len: u64) -> io::Result<()> {
        self.count_op("set_len")?;
        RealFs.set_len(fd, len)
    }

    fn drop_cache(&self, fd: BorrowedFd) -> io::Result<()> {
        self.count_op("drop_cache")?;
        RealFs.drop_cache(fd)
    }

    fn fsync(&self, fd: BorrowedFd) -> io::Result<()> {
        self.count_op("fsync")?;
        RealFs.fsync(fd)
    }

    fn rename_noreplace(
        &self,
        from_dir: BorrowedFd,
        from: &Path,
        to_dir: BorrowedFd,
        to: &Path,
    ) -> io::Result<()> {
        self.count_op("rename_noreplace")?;
        RealFs.rename_noreplace(from_dir, from, to_dir, to)
    }

    fn rename_exchange(
        &self,
        from_dir: BorrowedFd,
        from: &Path,
        to_dir: BorrowedFd,
        to: &Path,
    ) -> io::Result<()> {
        self.count_op("rename_exchange")?;
        RealFs.rename_exchange(from_dir, from, to_dir, to)
    }

    fn link(
        &self,
        from_dir: BorrowedFd,
        from: &Path,
        to_dir: BorrowedFd,
        to: &Path,
    ) -> io::Result<()> {
        self.count_op("link")?;
        RealFs.link(from_dir, from, to_dir, to)
    }

    fn unlink(&self, dir: BorrowedFd, path: &Path) -> io::Result<()> {
        self.count_op("unlink")?;
        RealFs.unlink(dir, path)
    }

    fn rmdir(&self, dir: BorrowedFd, path: &Path) -> io::Result<()> {
        self.count_op("rmdir")?;
        RealFs.rmdir(dir, path)
    }

    fn list_dir(&self, dir: BorrowedFd) -> io::Result<Vec<OsString>> {
        self.count_op("list_dir")?;
        RealFs.list_dir(dir)
    }

    fn stat_fs(&self, fd: BorrowedFd) -> io::Result<FsStat> {
        self.count_op("stat_fs")?;
        RealFs.stat_fs(fd)
    }

    fn set_lease(&self, fd: BorrowedFd, lease: Lease) -> io::Result<()> {
        self.count_op("set_lease")?;
        RealFs.set_lease(fd, lease)
    }

    fn lease(&self, fd: BorrowedFd) -> io::Result<Lease> {
        RealFs.lease(fd)
    }

    fn set_owner(&self, fd: BorrowedFd, uid: u32, gid: u32) -> io::Result<()> {
        self.count_op("set_owner")?;
        RealFs.set_owner(fd, uid, gid)
    }

    fn set_mode(&self, fd: BorrowedFd, mode: u32) -> io::Result<()> {
        self.count_op("set_mode")?;
        RealFs.set_mode(fd, mode)
    }

    fn set_times(&self, fd: BorrowedFd, atime: Timespec, mtime: Timespec) -> io::Result<()> {
        self.count_op("set_times")?;
        RealFs.set_times(fd, atime, mtime)
    }

    fn set_owner_at(&self, dir: BorrowedFd, path: &Path, uid: u32, gid: u32) -> io::Result<()> {
        self.count_op("set_owner_at")?;
        RealFs.set_owner_at(dir, path, uid, gid)
    }

    fn set_times_at(
        &self,
        dir: BorrowedFd,
        path: &Path,
        atime: Timespec,
        mtime: Timespec,
    ) -> io::Result<()> {
        self.count_op("set_times_at")?;
        RealFs.set_times_at(dir, path, atime, mtime)
    }

    fn list_xattrs(&self, fd: BorrowedFd) -> io::Result<Vec<OsString>> {
        self.count_op("list_xattrs")?;
        RealFs.list_xattrs(fd)
    }

    fn get_xattr(&self, fd: BorrowedFd, name: &OsStr) -> io::Result<Vec<u8>> {
        self.count_op("get_xattr")?;
        RealFs.get_xattr(fd, name)
    }

    fn set_xattr(&self, fd: BorrowedFd, name: &OsStr, value: &[u8]) -> io::Result<()> {
        self.count_op("set_xattr")?;
        RealFs.set_xattr(fd, name, value)
    }

    fn remove_xattr(&self, fd: BorrowedFd, name: &OsStr) -> io::Result<()> {
        self.count_op("remove_xattr")?;
        RealFs.remove_xattr(fd, name)
    }
}
