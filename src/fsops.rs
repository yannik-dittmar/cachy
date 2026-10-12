//! The filesystem-operations port: every syscall a move makes goes through
//! [`FsOps`], one method per syscall. [`RealFs`] performs them; test
//! implementations can inject errors or crashes at any single operation.
//!
//! File descriptors are closed on drop, outside the port.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

pub use rustix::fs::Stat;
use rustix::fs::{
    Advice, AtFlags, Gid, Mode, OFlags, RenameFlags, SeekFrom, Timespec, Timestamps, Uid,
    XattrFlags,
};

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
    /// Creates `path` relative to `dir` for reading and writing; fails if it
    /// exists.
    fn create_new(&self, dir: BorrowedFd, path: &Path, mode: u32) -> io::Result<OwnedFd>;
    fn mkdir(&self, dir: BorrowedFd, path: &Path, mode: u32) -> io::Result<()>;
    /// Creates the symlink `path` relative to `dir`, with the link text
    /// `target`; fails if `path` exists.
    fn symlink(&self, target: &OsStr, dir: BorrowedFd, path: &Path) -> io::Result<()>;
    /// The link text of the symlink `path` relative to `dir`.
    fn read_link(&self, dir: BorrowedFd, path: &Path) -> io::Result<OsString>;
    fn fstat(&self, fd: BorrowedFd) -> io::Result<Stat>;
    /// Stats `path` relative to `dir`, without following a final symlink.
    fn stat_at(&self, dir: BorrowedFd, path: &Path) -> io::Result<Stat>;
    fn read(&self, fd: BorrowedFd, buf: &mut [u8]) -> io::Result<usize>;
    /// Reads at `offset`, leaving the file offset as it is.
    fn read_at(&self, fd: BorrowedFd, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    /// Writes at `offset`, leaving the file offset as it is.
    fn write_at(&self, fd: BorrowedFd, buf: &[u8], offset: u64) -> io::Result<usize>;
    /// The start of the first data region at or after `offset`, or `None` if
    /// only a hole follows (`SEEK_DATA`). Moves the file offset there.
    fn seek_data(&self, fd: BorrowedFd, offset: u64) -> io::Result<Option<u64>>;
    /// The start of the first hole at or after `offset`, which is the end of
    /// the file if no other hole follows (`SEEK_HOLE`). Moves the file offset
    /// there.
    fn seek_hole(&self, fd: BorrowedFd, offset: u64) -> io::Result<u64>;
    /// Truncates or extends the file to `len` bytes; an extension is a hole.
    fn set_len(&self, fd: BorrowedFd, len: u64) -> io::Result<()>;
    /// Evicts the file's clean pages from the page cache, so that the next
    /// read comes from storage. A hint: some filesystems, such as ZFS, keep a
    /// cache of their own.
    fn drop_cache(&self, fd: BorrowedFd) -> io::Result<()>;
    fn fsync(&self, fd: BorrowedFd) -> io::Result<()>;
    /// Renames without ever replacing an existing `to`.
    fn rename_noreplace(
        &self,
        from_dir: BorrowedFd,
        from: &Path,
        to_dir: BorrowedFd,
        to: &Path,
    ) -> io::Result<()>;
    /// Creates `to` relative to `to_dir` as a hard link to `from` relative
    /// to `from_dir`, without following a final symlink; fails if `to`
    /// exists.
    fn link(
        &self,
        from_dir: BorrowedFd,
        from: &Path,
        to_dir: BorrowedFd,
        to: &Path,
    ) -> io::Result<()>;
    fn unlink(&self, dir: BorrowedFd, path: &Path) -> io::Result<()>;
    fn rmdir(&self, dir: BorrowedFd, path: &Path) -> io::Result<()>;
    /// The names of the entries in the directory `dir`, without `.` and `..`.
    fn list_dir(&self, dir: BorrowedFd) -> io::Result<Vec<OsString>>;
    /// Bytes available to unprivileged writers on `fd`'s filesystem.
    fn free_space(&self, fd: BorrowedFd) -> io::Result<u64>;
    /// Takes, changes or releases the lease on `fd`'s file. Fails with
    /// [`io::ErrorKind::WouldBlock`] if another open conflicts with it.
    fn set_lease(&self, fd: BorrowedFd, lease: Lease) -> io::Result<()>;
    /// The lease on `fd`'s file. While a conflicting open is breaking it, this
    /// is the lease it is being broken to.
    fn lease(&self, fd: BorrowedFd) -> io::Result<Lease>;
    fn set_owner(&self, fd: BorrowedFd, uid: u32, gid: u32) -> io::Result<()>;
    /// Sets the permission bits, including setuid, setgid and sticky.
    fn set_mode(&self, fd: BorrowedFd, mode: u32) -> io::Result<()>;
    fn set_times(&self, fd: BorrowedFd, atime: Timespec, mtime: Timespec) -> io::Result<()>;
    /// Sets the owner of `path` relative to `dir`, without following a final
    /// symlink.
    fn set_owner_at(&self, dir: BorrowedFd, path: &Path, uid: u32, gid: u32) -> io::Result<()>;
    /// Sets the times of `path` relative to `dir`, without following a final
    /// symlink.
    fn set_times_at(
        &self,
        dir: BorrowedFd,
        path: &Path,
        atime: Timespec,
        mtime: Timespec,
    ) -> io::Result<()>;
    /// The names of `fd`'s extended attributes, including ACLs.
    fn list_xattrs(&self, fd: BorrowedFd) -> io::Result<Vec<OsString>>;
    fn get_xattr(&self, fd: BorrowedFd, name: &OsStr) -> io::Result<Vec<u8>>;
    /// Creates or replaces an extended attribute.
    fn set_xattr(&self, fd: BorrowedFd, name: &OsStr, value: &[u8]) -> io::Result<()>;
    fn remove_xattr(&self, fd: BorrowedFd, name: &OsStr) -> io::Result<()>;
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
        let flags = OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
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

    fn symlink(&self, target: &OsStr, dir: BorrowedFd, path: &Path) -> io::Result<()> {
        Ok(rustix::fs::symlinkat(target, dir, path)?)
    }

    fn read_link(&self, dir: BorrowedFd, path: &Path) -> io::Result<OsString> {
        let target = rustix::fs::readlinkat(dir, path, Vec::new())?;
        Ok(OsString::from_vec(target.into_bytes()))
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

    fn read_at(&self, fd: BorrowedFd, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        Ok(rustix::io::pread(fd, buf, offset)?)
    }

    fn write_at(&self, fd: BorrowedFd, buf: &[u8], offset: u64) -> io::Result<usize> {
        Ok(rustix::io::pwrite(fd, buf, offset)?)
    }

    fn seek_data(&self, fd: BorrowedFd, offset: u64) -> io::Result<Option<u64>> {
        match rustix::fs::seek(fd, SeekFrom::Data(offset)) {
            Ok(data) => Ok(Some(data)),
            Err(rustix::io::Errno::NXIO) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn seek_hole(&self, fd: BorrowedFd, offset: u64) -> io::Result<u64> {
        Ok(rustix::fs::seek(fd, SeekFrom::Hole(offset))?)
    }

    fn set_len(&self, fd: BorrowedFd, len: u64) -> io::Result<()> {
        Ok(rustix::fs::ftruncate(fd, len)?)
    }

    fn drop_cache(&self, fd: BorrowedFd) -> io::Result<()> {
        Ok(rustix::fs::fadvise(fd, 0, None, Advice::DontNeed)?)
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

    fn link(
        &self,
        from_dir: BorrowedFd,
        from: &Path,
        to_dir: BorrowedFd,
        to: &Path,
    ) -> io::Result<()> {
        Ok(rustix::fs::linkat(
            from_dir,
            from,
            to_dir,
            to,
            AtFlags::empty(),
        )?)
    }

    fn unlink(&self, dir: BorrowedFd, path: &Path) -> io::Result<()> {
        Ok(rustix::fs::unlinkat(dir, path, AtFlags::empty())?)
    }

    fn rmdir(&self, dir: BorrowedFd, path: &Path) -> io::Result<()> {
        Ok(rustix::fs::unlinkat(dir, path, AtFlags::REMOVEDIR)?)
    }

    fn list_dir(&self, dir: BorrowedFd) -> io::Result<Vec<OsString>> {
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::read_from(dir)? {
            let name = entry?.file_name().to_bytes().to_vec();
            if name != b"." && name != b".." {
                names.push(OsString::from_vec(name));
            }
        }
        Ok(names)
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

    fn set_owner(&self, fd: BorrowedFd, uid: u32, gid: u32) -> io::Result<()> {
        let (uid, gid) = (Uid::from_raw(uid), Gid::from_raw(gid));
        Ok(rustix::fs::fchown(fd, Some(uid), Some(gid))?)
    }

    fn set_mode(&self, fd: BorrowedFd, mode: u32) -> io::Result<()> {
        Ok(rustix::fs::fchmod(fd, Mode::from_raw_mode(mode))?)
    }

    fn set_times(&self, fd: BorrowedFd, atime: Timespec, mtime: Timespec) -> io::Result<()> {
        let times = Timestamps {
            last_access: atime,
            last_modification: mtime,
        };
        Ok(rustix::fs::futimens(fd, &times)?)
    }

    fn set_owner_at(&self, dir: BorrowedFd, path: &Path, uid: u32, gid: u32) -> io::Result<()> {
        let (uid, gid) = (Uid::from_raw(uid), Gid::from_raw(gid));
        Ok(rustix::fs::chownat(
            dir,
            path,
            Some(uid),
            Some(gid),
            AtFlags::SYMLINK_NOFOLLOW,
        )?)
    }

    fn set_times_at(
        &self,
        dir: BorrowedFd,
        path: &Path,
        atime: Timespec,
        mtime: Timespec,
    ) -> io::Result<()> {
        let times = Timestamps {
            last_access: atime,
            last_modification: mtime,
        };
        Ok(rustix::fs::utimensat(
            dir,
            path,
            &times,
            AtFlags::SYMLINK_NOFOLLOW,
        )?)
    }

    fn list_xattrs(&self, fd: BorrowedFd) -> io::Result<Vec<OsString>> {
        let names = read_sized(|buf| rustix::fs::flistxattr(fd, buf))?;
        Ok(names
            .split(|&b| b == 0)
            .filter(|name| !name.is_empty())
            .map(|name| OsString::from_vec(name.to_vec()))
            .collect())
    }

    fn get_xattr(&self, fd: BorrowedFd, name: &OsStr) -> io::Result<Vec<u8>> {
        read_sized(|buf| rustix::fs::fgetxattr(fd, name, buf))
    }

    fn set_xattr(&self, fd: BorrowedFd, name: &OsStr, value: &[u8]) -> io::Result<()> {
        Ok(rustix::fs::fsetxattr(fd, name, value, XattrFlags::empty())?)
    }

    fn remove_xattr(&self, fd: BorrowedFd, name: &OsStr) -> io::Result<()> {
        Ok(rustix::fs::fremovexattr(fd, name)?)
    }
}

/// Reads a value of unknown size with `read`, which fills its buffer and
/// returns the size, or with an empty buffer only returns the size.
fn read_sized(read: impl Fn(&mut [u8]) -> rustix::io::Result<usize>) -> io::Result<Vec<u8>> {
    loop {
        let mut buf = vec![0; read(&mut [])?];
        match read(&mut buf) {
            Ok(len) => {
                buf.truncate(len);
                return Ok(buf);
            }
            // It grew between the two calls.
            Err(rustix::io::Errno::RANGE) => continue,
            Err(e) => return Err(e.into()),
        }
    }
}
