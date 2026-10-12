//! The filesystem-operations port: every syscall a move makes goes through
//! [`FsOps`], one method per syscall. [`RealFs`] performs them; test
//! implementations can inject errors or crashes at any single operation.
//!
//! File descriptors are closed on drop, outside the port.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::path::Path;

pub use rustix::fs::Stat;
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};

/// A kernel file lease (`F_SETLEASE`) on an open file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lease {
    /// Broken when the file is opened for writing or truncated.
    Read,
    /// Broken when the file is opened at all or truncated.
    Write,
    /// No lease.
    Unlocked,
}

impl Lease {
    fn raw(self) -> libc::c_int {
        match self {
            Lease::Read => libc::F_RDLCK,
            Lease::Write => libc::F_WRLCK,
            Lease::Unlocked => libc::F_UNLCK,
        }
    }
}

pub trait FsOps {
    /// Opens the directory `path` relative to `dir`, without following a
    /// final symlink.
    fn open_dir(&self, dir: BorrowedFd, path: &Path) -> io::Result<OwnedFd>;
    /// Opens the regular file `path` relative to `dir` for reading, without
    /// following a final symlink.
    fn open_read(&self, dir: BorrowedFd, path: &Path) -> io::Result<OwnedFd>;
    /// Creates `path` relative to `dir` for writing; fails if it exists.
    fn create_new(&self, dir: BorrowedFd, path: &Path, mode: u32) -> io::Result<OwnedFd>;
    fn mkdir(&self, dir: BorrowedFd, path: &Path, mode: u32) -> io::Result<()>;
    fn fstat(&self, fd: BorrowedFd) -> io::Result<Stat>;
    /// Stats `path` relative to `dir`, without following a final symlink.
    fn stat_at(&self, dir: BorrowedFd, path: &Path) -> io::Result<Stat>;
    fn read(&self, fd: BorrowedFd, buf: &mut [u8]) -> io::Result<usize>;
    fn write(&self, fd: BorrowedFd, buf: &[u8]) -> io::Result<usize>;
    fn fsync(&self, fd: BorrowedFd) -> io::Result<()>;
    /// Renames without ever replacing an existing `to`.
    fn rename_noreplace(
        &self,
        from_dir: BorrowedFd,
        from: &Path,
        to_dir: BorrowedFd,
        to: &Path,
    ) -> io::Result<()>;
    fn unlink(&self, dir: BorrowedFd, path: &Path) -> io::Result<()>;
    fn rmdir(&self, dir: BorrowedFd, path: &Path) -> io::Result<()>;
    /// Bytes available to unprivileged writers on `fd`'s filesystem.
    fn free_space(&self, fd: BorrowedFd) -> io::Result<u64>;
    /// Takes, changes or releases the lease on `fd`'s file. Fails with
    /// [`io::ErrorKind::WouldBlock`] if another open conflicts with it.
    fn set_lease(&self, fd: BorrowedFd, lease: Lease) -> io::Result<()>;
    /// The lease on `fd`'s file. While a conflicting open is breaking it, this
    /// is the lease it is being broken to.
    fn lease(&self, fd: BorrowedFd) -> io::Result<Lease>;
}

pub struct RealFs;

impl FsOps for RealFs {
    fn open_dir(&self, dir: BorrowedFd, path: &Path) -> io::Result<OwnedFd> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        Ok(rustix::fs::openat(dir, path, flags, Mode::empty())?)
    }

    fn open_read(&self, dir: BorrowedFd, path: &Path) -> io::Result<OwnedFd> {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        Ok(rustix::fs::openat(dir, path, flags, Mode::empty())?)
    }

    fn create_new(&self, dir: BorrowedFd, path: &Path, mode: u32) -> io::Result<OwnedFd> {
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
        Ok(rustix::fs::openat(
            dir,
            path,
            flags,
            Mode::from_raw_mode(mode),
        )?)
    }

    fn mkdir(&self, dir: BorrowedFd, path: &Path, mode: u32) -> io::Result<()> {
        Ok(rustix::fs::mkdirat(dir, path, Mode::from_raw_mode(mode))?)
    }

    fn fstat(&self, fd: BorrowedFd) -> io::Result<Stat> {
        Ok(rustix::fs::fstat(fd)?)
    }

    fn stat_at(&self, dir: BorrowedFd, path: &Path) -> io::Result<Stat> {
        Ok(rustix::fs::statat(dir, path, AtFlags::SYMLINK_NOFOLLOW)?)
    }

    fn read(&self, fd: BorrowedFd, buf: &mut [u8]) -> io::Result<usize> {
        Ok(rustix::io::read(fd, buf)?)
    }

    fn write(&self, fd: BorrowedFd, buf: &[u8]) -> io::Result<usize> {
        Ok(rustix::io::write(fd, buf)?)
    }

    fn fsync(&self, fd: BorrowedFd) -> io::Result<()> {
        Ok(rustix::fs::fsync(fd)?)
    }

    fn rename_noreplace(
        &self,
        from_dir: BorrowedFd,
        from: &Path,
        to_dir: BorrowedFd,
        to: &Path,
    ) -> io::Result<()> {
        Ok(rustix::fs::renameat_with(
            from_dir,
            from,
            to_dir,
            to,
            RenameFlags::NOREPLACE,
        )?)
    }

    fn unlink(&self, dir: BorrowedFd, path: &Path) -> io::Result<()> {
        Ok(rustix::fs::unlinkat(dir, path, AtFlags::empty())?)
    }

    fn rmdir(&self, dir: BorrowedFd, path: &Path) -> io::Result<()> {
        Ok(rustix::fs::unlinkat(dir, path, AtFlags::REMOVEDIR)?)
    }

    fn free_space(&self, fd: BorrowedFd) -> io::Result<u64> {
        let s = rustix::fs::fstatvfs(fd)?;
        Ok(s.f_bavail * s.f_frsize)
    }

    fn set_lease(&self, fd: BorrowedFd, lease: Lease) -> io::Result<()> {
        // SAFETY: F_SETLEASE takes an int argument and touches no memory.
        let r = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETLEASE, lease.raw()) };
        if r == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn lease(&self, fd: BorrowedFd) -> io::Result<Lease> {
        // SAFETY: F_GETLEASE takes no argument and touches no memory.
        match unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETLEASE) } {
            -1 => Err(io::Error::last_os_error()),
            libc::F_RDLCK => Ok(Lease::Read),
            libc::F_WRLCK => Ok(Lease::Write),
            libc::F_UNLCK => Ok(Lease::Unlocked),
            other => Err(io::Error::other(format!("unknown lease type {other}"))),
        }
    }
}
