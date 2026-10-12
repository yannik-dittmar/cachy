//! Test-only fault injection, built only with the `fault-injection` feature.
//!
//! [`FaultyFs`] performs every filesystem operation through [`RealFs`], except
//! that the one named by `CACHY_FAULT=<fault>@<n>` (the `n`th operation of
//! the process, counting from 1) does not happen. Instead, a `kill` fault
//! kills the process with `SIGKILL`, and `eio`, `enospc`, `exdev` or `eintr`
//! fails the operation with that errno. Either way cachy first notes the
//! fault on stderr.
//!
//! Lease queries are not counted, nor can they fail: how often a run polls its
//! leases depends on timing, and the operation numbers must not.
//!
//! `CACHY_RETIRE_WAIT_MS=<ms>` shortens the retire wait, so that tests that
//! run cachy hundreds of times do not wait a second each time.

use std::cell::Cell;
use std::ffi::OsString;
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};

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
    /// The fault and the number of the operation it replaces.
    fault: Option<(Fault, u64)>,
    /// Operations performed so far.
    count: Cell<u64>,
}

impl FaultyFs {
    /// Injects the fault that `CACHY_FAULT` names, if it is set.
    pub fn from_env() -> Result<FaultyFs> {
        let fault = match std::env::var(FAULT) {
            Ok(spec) => Some(parse(&spec).with_context(|| format!("invalid {FAULT}=`{spec}`"))?),
            Err(_) => None,
        };
        Ok(FaultyFs {
            fault,
            count: Cell::new(0),
        })
    }

    /// Counts the operation `name`, and injects the fault if it is the one.
    fn op(&self, name: &str) -> io::Result<()> {
        let n = self.count.get() + 1;
        self.count.set(n);
        match self.fault {
            Some((Fault::Kill, at)) if at == n => {
                eprintln!("cachy: fault injection: kill at operation {n} ({name})");
                // SAFETY: kill(2) on our own pid touches no memory.
                unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
                unreachable!("SIGKILL to self returned");
            }
            Some((Fault::Errno(errno), at)) if at == n => {
                let e = io::Error::from_raw_os_error(errno);
                eprintln!("cachy: fault injection: {e} at operation {n} ({name})");
                Err(e)
            }
            _ => Ok(()),
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
        self.op("open_dir")?;
        RealFs.open_dir(dir, path)
    }

    fn open_read(&self, dir: BorrowedFd, path: &Path) -> io::Result<OwnedFd> {
        self.op("open_read")?;
        RealFs.open_read(dir, path)
    }

    fn create_new(&self, dir: BorrowedFd, path: &Path, mode: u32) -> io::Result<OwnedFd> {
        self.op("create_new")?;
        RealFs.create_new(dir, path, mode)
    }

    fn mkdir(&self, dir: BorrowedFd, path: &Path, mode: u32) -> io::Result<()> {
        self.op("mkdir")?;
        RealFs.mkdir(dir, path, mode)
    }

    fn fstat(&self, fd: BorrowedFd) -> io::Result<Stat> {
        self.op("fstat")?;
        RealFs.fstat(fd)
    }

    fn stat_at(&self, dir: BorrowedFd, path: &Path) -> io::Result<Stat> {
        self.op("stat_at")?;
        RealFs.stat_at(dir, path)
    }

    fn read(&self, fd: BorrowedFd, buf: &mut [u8]) -> io::Result<usize> {
        self.op("read")?;
        RealFs.read(fd, buf)
    }

    fn write(&self, fd: BorrowedFd, buf: &[u8]) -> io::Result<usize> {
        self.op("write")?;
        RealFs.write(fd, buf)
    }

    fn fsync(&self, fd: BorrowedFd) -> io::Result<()> {
        self.op("fsync")?;
        RealFs.fsync(fd)
    }

    fn rename_noreplace(
        &self,
        from_dir: BorrowedFd,
        from: &Path,
        to_dir: BorrowedFd,
        to: &Path,
    ) -> io::Result<()> {
        self.op("rename_noreplace")?;
        RealFs.rename_noreplace(from_dir, from, to_dir, to)
    }

    fn unlink(&self, dir: BorrowedFd, path: &Path) -> io::Result<()> {
        self.op("unlink")?;
        RealFs.unlink(dir, path)
    }

    fn rmdir(&self, dir: BorrowedFd, path: &Path) -> io::Result<()> {
        self.op("rmdir")?;
        RealFs.rmdir(dir, path)
    }

    fn list_dir(&self, dir: BorrowedFd) -> io::Result<Vec<OsString>> {
        self.op("list_dir")?;
        RealFs.list_dir(dir)
    }

    fn free_space(&self, fd: BorrowedFd) -> io::Result<u64> {
        self.op("free_space")?;
        RealFs.free_space(fd)
    }

    fn set_lease(&self, fd: BorrowedFd, lease: Lease) -> io::Result<()> {
        self.op("set_lease")?;
        RealFs.set_lease(fd, lease)
    }

    fn lease(&self, fd: BorrowedFd) -> io::Result<Lease> {
        RealFs.lease(fd)
    }
}
