//! Metadata copying: owner, group, mode, xattrs, ACLs and timestamps, from a
//! file or directory on one branch to its copy on another.
//!
//! The steps run in an order where none undoes an earlier one: a chown clears
//! setuid, setgid and file capabilities, so the mode and the xattrs follow it;
//! the timestamps come last.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rustix::fs::Timespec;

use crate::config::MetadataKind;
use crate::fsops::{FsOps, Stat};

/// The xattrs that hold POSIX ACLs.
const ACLS: [&str; 2] = ["system.posix_acl_access", "system.posix_acl_default"];

/// Kinds of metadata that a copy may lose, with a warning.
#[derive(Clone, Copy, Default)]
pub struct AllowedLosses {
    pub xattrs: bool,
    pub acls: bool,
}

impl AllowedLosses {
    pub fn of(kinds: &[MetadataKind]) -> AllowedLosses {
        AllowedLosses {
            xattrs: kinds.contains(&MetadataKind::Xattrs),
            acls: kinds.contains(&MetadataKind::Acls),
        }
    }

    fn allows(self, kind: Kind) -> bool {
        match kind {
            Kind::Xattr => self.xattrs,
            Kind::Acl => self.acls,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Xattr,
    Acl,
}

impl Kind {
    fn of(name: &OsStr) -> Kind {
        if ACLS.iter().any(|acl| name == *acl) {
            Kind::Acl
        } else {
            Kind::Xattr
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Kind::Xattr => "xattr",
            Kind::Acl => "ACL",
        })
    }
}

/// Copies everything but the timestamps from `src`, whose metadata is
/// `stat`, to `dst`: owner and group, then mode, then xattrs and ACLs. `dst`
/// loses any ACL it inherited that `src` lacks. `what` names `dst` in
/// warnings about allowed losses.
pub fn copy_attributes(
    fs: &dyn FsOps,
    stat: &Stat,
    src: BorrowedFd,
    dst: BorrowedFd,
    allowed: AllowedLosses,
    what: &Path,
) -> Result<()> {
    fs.set_owner(dst, stat.st_uid, stat.st_gid)
        .context("chown")?;
    fs.set_mode(dst, stat.st_mode & 0o7777).context("chmod")?;
    let names = list_xattrs(fs, src).context("list xattrs")?;
    for name in &names {
        let copied = fs
            .get_xattr(src, name)
            .and_then(|value| fs.set_xattr(dst, name, &value));
        if let Err(e) = copied {
            lose(name, e, allowed, what)?;
        }
    }
    // A new file or directory inherits its parent's default ACL.
    for name in list_xattrs(fs, dst).context("list xattrs of the copy")? {
        if Kind::of(&name) != Kind::Acl || names.contains(&name) {
            continue;
        }
        match fs.remove_xattr(dst, &name) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::ENODATA) => {}
            Err(e) => lose(&name, e, allowed, what)?,
        }
    }
    Ok(())
}

/// Copies the atime and mtime of `stat` to `dst`.
pub fn copy_times(fs: &dyn FsOps, stat: &Stat, dst: BorrowedFd) -> Result<()> {
    let time = |sec, nsec| Timespec {
        tv_sec: sec,
        tv_nsec: nsec as _,
    };
    fs.set_times(
        dst,
        time(stat.st_atime, stat.st_atime_nsec),
        time(stat.st_mtime, stat.st_mtime_nsec),
    )
    .context("set timestamps")
}

/// Copies all metadata of the file `src`, whose metadata is `stat`, to `dst`
/// and checks that owner, group, mode and mtime arrived unchanged.
pub fn copy_file(
    fs: &dyn FsOps,
    stat: &Stat,
    src: BorrowedFd,
    dst: BorrowedFd,
    allowed: AllowedLosses,
    what: &Path,
) -> Result<()> {
    copy_attributes(fs, stat, src, dst, allowed, what)?;
    copy_times(fs, stat, dst)?;
    let copied = fs.fstat(dst).context("fstat the copy")?;
    if !same(stat, &copied) {
        bail!(
            "not preserved: owner {}:{}, mode {:o}, mtime {}.{:09} became {}:{}, {:o}, {}.{:09}",
            stat.st_uid,
            stat.st_gid,
            stat.st_mode & 0o7777,
            stat.st_mtime,
            stat.st_mtime_nsec,
            copied.st_uid,
            copied.st_gid,
            copied.st_mode & 0o7777,
            copied.st_mtime,
            copied.st_mtime_nsec,
        );
    }
    Ok(())
}

/// Whether `a` and `b` have the same owner, group, mode and mtime: the
/// metadata that a move never loses.
pub fn same(a: &Stat, b: &Stat) -> bool {
    (a.st_uid, a.st_gid, a.st_mode, a.st_mtime, a.st_mtime_nsec)
        == (b.st_uid, b.st_gid, b.st_mode, b.st_mtime, b.st_mtime_nsec)
}

/// The xattrs of `fd`, or none if its filesystem has no xattr support.
fn list_xattrs(fs: &dyn FsOps, fd: BorrowedFd) -> io::Result<Vec<OsString>> {
    match fs.list_xattrs(fd) {
        Err(e) if e.raw_os_error() == Some(libc::EOPNOTSUPP) => Ok(Vec::new()),
        listed => listed,
    }
}

/// Fails because the xattr `name` could not be copied, or only warns if its
/// kind's loss is allowed.
fn lose(name: &OsStr, e: io::Error, allowed: AllowedLosses, what: &Path) -> Result<()> {
    let kind = Kind::of(name);
    if !allowed.allows(kind) {
        return Err(e).with_context(|| format!("copy {kind} {}", name.display()));
    }
    eprintln!(
        "cachy: warning: {}: {kind} {} not preserved, as allow_metadata_loss allows: {e}",
        what.display(),
        name.display()
    );
    Ok(())
}
