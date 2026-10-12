//! The filesystem-operations port: every syscall a move makes goes through
//! [`FsOps`], one method per syscall. [`RealFs`] performs them; test
//! implementations can inject errors or crashes at any single operation.
//!
//! File descriptors are closed on drop, outside the port.

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};

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
}
