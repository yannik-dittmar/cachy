//! Test-only fault injection, built only with the `fault-injection` feature.
//!
//! [`FaultyFs`] performs every filesystem operation through [`RealFs`], except
//! that the one named by `CACHY_FAULT=<fault>@<n>` (the `n`th operation of
//! the process, counting from 1) does not happen. Instead, a `kill` fault
//! kills the process with `SIGKILL`, and `eio`, `enospc`, `exdev` or `eintr`
//! fails the operation with that errno. Either way cachy first notes the
//! fault on stderr. A comma-separated list injects several faults, such as
//! one that starts a rollback and one during it: `eio@20,kill@23`.
//!
//! Lease queries are not counted, nor can they fail: how often a run polls its
//! leases depends on timing, and the operation numbers must not.
//!
//! `CACHY_RETIRE_WAIT_MS=<ms>` shortens the retire wait, so that tests that
//! run cachy hundreds of times do not wait a second each time.

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use rustix::fs::Timespec;

use crate::fsops::{FsOps, Lease, RealFs, Stat};

const FAULT: &str = "CACHY_FAULT";
const RETIRE_WAIT: &str = "CACHY_RETIRE_WAIT_MS";

/// The retire wait set by `CACHY_RETIRE_WAIT_MS`, if any.
pub fn retire_wait() -> Option<Duration> {
    static WAIT: OnceLock<Option<Duration>> = OnceLock::new();
    *WAIT.get_or_init(|| {
        let ms = std::env::var(RETIRE_WAIT).ok()?;
        let ms = ms
            .parse()
            .unwrap_or_else(|_| panic!("{RETIRE_WAIT}=`{ms}`: expected milliseconds"));
        Some(Duration::from_millis(ms))
    })
}

#[derive(Clone, Copy)]
enum Fault {
    Kill,
    Errno(i32),
}

pub struct FaultyFs {
    /// The faults, each with the number of the operation it replaces.
    faults: Vec<(Fault, u64)>,
    /// Operations performed so far.
    count: Cell<u64>,
}

impl FaultyFs {
    /// Injects the faults that `CACHY_FAULT` names, if it is set.
    pub fn from_env() -> Result<FaultyFs> {
        let faults = match std::env::var(FAULT) {
            Ok(spec) => spec
                .split(',')
                .map(parse)
                .collect::<Result<_>>()
                .with_context(|| format!("invalid {FAULT}=`{spec}`"))?,
            Err(_) => Vec::new(),
        };
        Ok(FaultyFs {
            faults,
            count: Cell::new(0),
        })
    }

    /// Counts the operation `name`, and injects the fault meant for it, if
    /// any.
    fn count_op(&self, name: &str) -> io::Result<()> {
        let n = self.count.get() + 1;
        self.count.set(n);
        match self.faults.iter().find(|&&(_, at)| at == n) {
            Some((Fault::Kill, _)) => {
                eprintln!("cachy: fault injection: kill at operation {n} ({name})");
                // SAFETY: kill(2) on our own pid touches no memory.
                unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
                unreachable!("SIGKILL to self returned");
            }
            Some(&(Fault::Errno(errno), _)) => {
                let e = io::Error::from_raw_os_error(errno);
                eprintln!("cachy: fault injection: {e} at operation {n} ({name})");
                Err(e)
            }
            None => Ok(()),
        }
    }
}

fn parse(spec: &str) -> Result<(Fault, u64)> {
    let Some((fault, n)) = spec.split_once('@') else {
        bail!("expected <fault>@<operation number>");
    };
    let fault = match fault {
        "kill" => Fault::Kill,
        "eio" => Fault::Errno(libc::EIO),
        "enospc" => Fault::Errno(libc::ENOSPC),
        "exdev" => Fault::Errno(libc::EXDEV),
        "eintr" => Fault::Errno(libc::EINTR),
        _ => bail!("unknown fault `{fault}`: expected kill, eio, enospc, exdev or eintr"),
    };
    let n = n.parse().context("invalid operation number")?;
    if n == 0 {
        bail!("operations are numbered from 1");
    }
    Ok((fault, n))
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

    fn write(&self, fd: BorrowedFd, buf: &[u8]) -> io::Result<usize> {
        self.count_op("write")?;
        RealFs.write(fd, buf)
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

    fn free_space(&self, fd: BorrowedFd) -> io::Result<u64> {
        self.count_op("free_space")?;
        RealFs.free_space(fd)
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
