//! The capability probe: before anything else, every invocation tries what
//! cachy's guarantees depend on, on scratch files in each filesystem's
//! staging directory, so that a kernel or module upgrade cannot silently
//! weaken them. It probes what cachy does on each filesystem:
//!
//! - On the cache branch: the in-use guard, which is a lease of the
//!   configured type unless the configuration explicitly chooses a scan of
//!   `/proc` (a scan that finds a scratch file's opens) or no guard at all,
//!   a no-replace rename (retiring a source, and recovery renaming one back),
//!   and hole reporting (`SEEK_DATA`/`SEEK_HOLE`, which sparse copies read).
//! - On each backing branch that takes new files: a no-replace rename
//!   (publishing a copy) and an exchange rename (replacing a stale copy).
//!   cachy writes nothing to the other backing branches, not even scratch
//!   files.
//!
//! A missing lease, `/proc` scan or rename refuses the run, as does a lease
//! failure whenever the configuration has not chosen another guard. A `/proc`
//! scan that cannot read some processes' descriptors only warns: it does not
//! see their opens. Hole reporting that cannot be trusted only makes moves
//! copy every byte, with a warning, as does an OpenZFS module under the cache
//! branch with the bug that reports data as holes. A filesystem without room for the scratch files is not probed, with
//! a warning: refusing would keep a full cache branch full, and a missing
//! capability then fails each move that needs it instead.
//!
//! cachy owns the scratch files, so the probe cannot show that a lease on a
//! file of another user fails without `CAP_LEASE` (`EACCES`); each move of
//! such a file fails instead.
//!
//! The scratch files live in `<staging directory>/.cachy-staging`, which
//! mirrors no relpath and otherwise holds only the directories that moves are
//! creating. A crash can leave them behind; the next probe removes them, by
//! their name prefix, before recovery runs, so recovery never takes one for a
//! retired source. The probe removes the staging directories it created.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::fsops::{FsOps, Lease};
use crate::layout::BackingBranch;
use crate::log;
use crate::proc_scan;
use crate::transaction::{Branch, InUseGuard, STAGING, STAGING_MODE};

/// What the names of scratch files start with.
const SCRATCH_PREFIX: &str = ".cachy-probe-";
/// The permissions of scratch files: cachy's alone.
const SCRATCH_MODE: u32 = 0o600;
/// The hole-reporting check writes a block of data at the start of a scratch
/// file and another one here, after a hole.
const HOLE_CHECK_OFFSET: u64 = 1 << 20;
/// The size of each block of data that the hole-reporting check writes.
const HOLE_CHECK_BLOCK: usize = 4096;

/// `statfs`'s `f_type` for OpenZFS.
const ZFS_SUPER_MAGIC: i64 = 0x2fc1_2fc1;
/// Where the OpenZFS module gives its version.
const ZFS_VERSION_FILE: &str = "/sys/module/zfs/version";
/// The OpenZFS issue of the bug that reports data as holes.
const ZFS_HOLE_BUG: &str = "OpenZFS issue #15526, fixed in 2.2.2";

/// What the probe found that changes how cachy moves files.
pub struct Capabilities {
    /// Moves copy every byte, because the cache branch's hole reporting
    /// cannot be trusted.
    pub dense_copies: bool,
    /// The cache branch is on OpenZFS, where `atime=off` does not always show
    /// as a mount option.
    pub cache_branch_on_zfs: bool,
}

/// A capability that cachy refuses to run without.
struct Capability {
    /// What it is, with the syscall that provides it.
    name: &'static str,
    /// Why cachy needs it.
    needed: &'static str,
}

const NOREPLACE: Capability = Capability {
    name: "no-replace renames (RENAME_NOREPLACE)",
    needed: "to publish and retire files without ever replacing one",
};
const EXCHANGE: Capability = Capability {
    name: "exchange renames (RENAME_EXCHANGE)",
    needed: "to replace stale copies without ever overwriting another file",
};
const READ_LEASE: Capability = Capability {
    name: "read leases (F_SETLEASE with F_RDLCK)",
    needed: "to tell which files are in use, unless open_file_guard is set to proc or off",
};
const WRITE_LEASE: Capability = Capability {
    name: "write leases (F_SETLEASE with F_WRLCK)",
    needed: "to tell which files are in use, unless open_file_guard is set to proc or off",
};
const PROC_SCAN: Capability = Capability {
    name: "finding open files in /proc",
    needed: "to tell which files are in use with open_file_guard: proc",
};

/// Probes the cache branch, for what the in-use guard `in_use` needs, and
/// each backing branch that takes new files, and warns of what makes moves
/// copy densely. Returns an error naming each filesystem and what it lacks if
/// cachy must refuse to run.
pub fn probe(
    fs: &dyn FsOps,
    in_use: InUseGuard,
    cache_branch: &Branch,
    backing_branches: &[BackingBranch],
) -> Result<Capabilities> {
    let cache_checks = Checks {
        role: "cache branch",
        in_use,
        holes: true,
        exchange: false,
    };
    let backing_checks = Checks {
        role: "backing branch",
        in_use: InUseGuard::Off,
        holes: false,
        exchange: true,
    };
    let branches = std::iter::once((cache_branch, &cache_checks)).chain(
        backing_branches
            .iter()
            .filter(|backing| backing.takes_new_files())
            .map(|backing| (&backing.branch, &backing_checks)),
    );
    let mut refusals = Vec::new();
    // Why moves copy every byte, if they do.
    let mut dense = Vec::new();
    for (branch, checks) in branches {
        let (role, path) = (checks.role, branch.path.display());
        match probe_branch(fs, branch, checks) {
            Ok(findings) => {
                for (capability, e) in findings.missing {
                    refusals.push(format!(
                        "{role} {path} does not support {} ({e}); cachy needs them {}",
                        capability.name, capability.needed
                    ));
                }
                if let Some(why) = findings.unreliable_holes {
                    dense.push(format!(
                        "{role} {path}: hole reporting cannot be trusted ({why})"
                    ));
                }
                if findings.hidden_processes > 0 {
                    log::warning!(
                        "cannot read the open descriptors of {} processes (not permitted, or a descriptor cannot be statted), so open_file_guard: proc moves files that only they have open; run cachy as root, or with CAP_SYS_PTRACE",
                        findings.hidden_processes
                    );
                }
            }
            Err(e) if from_lack_of_space(&e) => log::warning!(
                "cannot probe {role} {path}: no space for its scratch files ({e:#}); a move that needs what the branch lacks fails instead"
            ),
            Err(e) => refusals.push(format!("cannot probe {role} {path}: {e:#}")),
        }
    }
    if !refusals.is_empty() {
        bail!("refusing to run:\n  - {}", refusals.join("\n  - "));
    }

    let zfs_version = openzfs_version(cache_branch);
    dense.extend(zfs_version.as_ref().and_then(hole_bug));
    for why in &dense {
        log::warning!("{why}, so moves copy every byte and sparse files arrive dense");
    }
    Ok(Capabilities {
        dense_copies: !dense.is_empty(),
        cache_branch_on_zfs: zfs_version.is_some(),
    })
}

/// Why the OpenZFS module with `version` may report data as holes, if it may.
fn hole_bug(version: &io::Result<String>) -> Option<String> {
    match version {
        Err(e) => Some(format!(
            "cannot read the OpenZFS module version from {ZFS_VERSION_FILE} ({e}), and it may report data as holes ({ZFS_HOLE_BUG})"
        )),
        Ok(version) => match release(version) {
            None => Some(format!(
                "unrecognised OpenZFS module version `{version}`, which may report data as holes ({ZFS_HOLE_BUG})"
            )),
            Some(release) if reports_data_as_holes(release) => Some(format!(
                "OpenZFS {version} can report data as holes ({ZFS_HOLE_BUG})"
            )),
            Some(_) => None,
        },
    }
}

/// What to try on one filesystem.
struct Checks {
    /// What the branch is to cachy, as messages name it.
    role: &'static str,
    /// Check what this in-use guard needs: take and release its lease, or
    /// scan `/proc` for opens.
    in_use: InUseGuard,
    /// Check that data after a hole shows as data.
    holes: bool,
    /// Swap two files with an exchange rename.
    exchange: bool,
}

/// What probing one filesystem found.
#[derive(Default)]
struct Findings {
    /// What it lacks, each with the error that showed it.
    missing: Vec<(&'static Capability, io::Error)>,
    /// Why moves cannot trust its hole reporting, if they cannot.
    unreliable_holes: Option<String>,
    /// The number of processes whose open descriptors a `/proc` scan could
    /// not read.
    hidden_processes: usize,
}

impl Findings {
    /// Records that trying `capability` failed with `e`. Fails instead if
    /// that was for lack of space, which says nothing about the capability.
    fn lacks(&mut self, capability: &'static Capability, e: io::Error) -> Result<()> {
        if out_of_space(&e) {
            return Err(anyhow::Error::from(e).context(capability.name));
        }
        self.missing.push((capability, e));
        Ok(())
    }
}

/// Runs `checks` on scratch files in `branch`'s staging directory, and
/// removes them and the directories it created. An error means the
/// filesystem could not be probed.
fn probe_branch(fs: &dyn FsOps, branch: &Branch, checks: &Checks) -> Result<Findings> {
    let mut created = Vec::new();
    let result = scratch_dir(fs, branch, &mut created).and_then(|dir| {
        let checked = check(fs, &dir, checks);
        // A scratch file left behind refuses the run even if the checks
        // passed: recovery would take one on the cache branch for a retired
        // source, and rename it back into the cache pool.
        let removed = remove_scratch_files(fs, &dir).context("remove scratch files");
        removed.and(checked)
    });
    // Innermost first. Fails harmlessly if a directory is not empty.
    for (parent, name) in created.iter().rev() {
        let _ = fs.rmdir(parent.as_fd(), Path::new(name));
    }
    result
}

/// Opens `<staging directory>/.cachy-staging` on `branch`, creating what is
/// missing. Each directory it creates goes to `created`, with its parent.
fn scratch_dir(
    fs: &dyn FsOps,
    branch: &Branch,
    created: &mut Vec<(OwnedFd, &'static str)>,
) -> Result<OwnedFd> {
    let mut dir = fs
        .open_dir(branch.fd.as_fd(), Path::new("."))
        .context("open branch")?;
    // `<branch>/.cachy-staging/.cachy-staging`.
    for name in [STAGING, STAGING] {
        let child = match fs.mkdir(dir.as_fd(), Path::new(name), STAGING_MODE) {
            Ok(()) => {
                let child = fs.open_dir(dir.as_fd(), Path::new(name));
                created.push((dir, name));
                child
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                fs.open_dir(dir.as_fd(), Path::new(name))
            }
            Err(e) => Err(e),
        };
        dir = child.context("create staging directory")?;
    }
    Ok(dir)
}

/// Removes the scratch files in `dir`, of this probe or of one that crashed:
/// the entries whose names have the scratch prefix, except directories,
/// which moves are creating.
fn remove_scratch_files(fs: &dyn FsOps, dir: &OwnedFd) -> Result<()> {
    for name in fs.list_dir(dir.as_fd()).context("list staging directory")? {
        if !name.as_bytes().starts_with(SCRATCH_PREFIX.as_bytes()) {
            continue;
        }
        let name = Path::new(&name);
        let stat = match fs.stat_at(dir.as_fd(), name) {
            Ok(stat) => stat,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).context(format!("stat {}", name.display())),
        };
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            continue;
        }
        match fs.unlink(dir.as_fd(), name) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(e).context(format!("remove {}", name.display()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Runs `checks` on scratch files in `dir`, and returns what the filesystem
/// lacks.
fn check(fs: &dyn FsOps, dir: &OwnedFd, checks: &Checks) -> Result<Findings> {
    let mut findings = Findings::default();
    let pid = std::process::id();
    let scratch = |n: &str| PathBuf::from(format!("{SCRATCH_PREFIX}{pid}-{n}"));
    let (a, b, c) = (scratch("a"), scratch("b"), scratch("c"));

    let file = fs
        .create_new(dir.as_fd(), &a, SCRATCH_MODE)
        .context("create scratch file")?;
    if checks.holes {
        findings.unreliable_holes = check_holes(fs, &file)?;
    }
    if let InUseGuard::Proc { .. } = checks.in_use {
        match check_proc_scan(fs, dir, &a, &file) {
            Ok(hidden) => findings.hidden_processes = hidden,
            Err(e) => findings.lacks(&PROC_SCAN, e)?,
        }
    }
    // A read lease conflicts with any open for writing, even cachy's own.
    drop(file);

    if let InUseGuard::Lease(lease) = checks.in_use {
        let file = fs.open_read(dir.as_fd(), &a).context("open scratch file")?;
        if let Err(e) = take_and_release(fs, &file, lease) {
            let capability = match lease {
                Lease::Write => &WRITE_LEASE,
                _ => &READ_LEASE,
            };
            findings.lacks(capability, e)?;
        }
    }

    // To a free name: the kernel itself refuses to rename onto an existing
    // name with RENAME_NOREPLACE, whether the filesystem supports it or not.
    let renamed = match fs.rename_noreplace(dir.as_fd(), &a, dir.as_fd(), &c) {
        Ok(()) => &c,
        Err(e) => {
            findings.lacks(&NOREPLACE, e)?;
            &a
        }
    };

    if checks.exchange {
        drop(
            fs.create_new(dir.as_fd(), &b, SCRATCH_MODE)
                .context("create scratch file")?,
        );
        let inode = |name: &Path| {
            fs.stat_at(dir.as_fd(), name)
                .map(|stat| stat.st_ino)
                .context("stat scratch file")
        };
        let swapped_in = inode(renamed)?;
        match fs.rename_exchange(dir.as_fd(), renamed, dir.as_fd(), &b) {
            Ok(()) if inode(&b)? == swapped_in => {}
            Ok(()) => findings.lacks(&EXCHANGE, io::Error::other("the files were not swapped"))?,
            Err(e) => findings.lacks(&EXCHANGE, e)?,
        }
    }
    Ok(findings)
}

/// Opens the scratch file `name` in `dir` for reading too, beside its open
/// for writing `file`, and checks that a scan of `/proc` finds both opens.
/// Returns the number of processes whose descriptors the scan could not
/// read.
fn check_proc_scan(
    fs: &dyn FsOps,
    dir: &OwnedFd,
    name: &Path,
    file: &OwnedFd,
) -> io::Result<usize> {
    let stat = fs.fstat(file.as_fd())?;
    let reader = fs.open_read(dir.as_fd(), name)?;
    let opens = proc_scan::scan((stat.st_dev, stat.st_ino), true);
    drop(reader);
    let opens = opens?;
    if !opens.writing || !opens.reading {
        return Err(io::Error::other(format!(
            "a scratch file's opens were not found: {opens:?}"
        )));
    }
    Ok(opens.hidden)
}

/// Takes a lease of the type `lease` on `file`, checks that it holds, and
/// releases it.
fn take_and_release(fs: &dyn FsOps, file: &OwnedFd, lease: Lease) -> io::Result<()> {
    fs.set_lease(file.as_fd(), lease)?;
    let held = fs.lease(file.as_fd());
    fs.set_lease(file.as_fd(), Lease::Unlocked)?;
    match held? {
        held if held == lease => Ok(()),
        held => Err(io::Error::other(format!(
            "the lease taken shows as {held:?}"
        ))),
    }
}

/// Writes a block of data at the start of the empty `file` and another after
/// a hole, and checks that the filesystem reports both as data, as a sparse
/// copy relies on. Returns why its hole reporting cannot be trusted, if it
/// cannot. Not fsynced: OpenZFS's bug reported dirty data as holes.
fn check_holes(fs: &dyn FsOps, file: &OwnedFd) -> Result<Option<String>> {
    let block = [0xa5; HOLE_CHECK_BLOCK];
    for offset in [0, HOLE_CHECK_OFFSET] {
        write_all_at(fs, file, &block, offset).context("write scratch file")?;
    }
    for offset in [0, HOLE_CHECK_OFFSET] {
        let end = offset + HOLE_CHECK_BLOCK as u64;
        match fs.seek_data(file.as_fd(), offset) {
            Ok(Some(data)) if data == offset => {}
            Ok(_) => return Ok(Some(format!("SEEK_DATA skips the data at {offset}"))),
            Err(e) => return Ok(Some(format!("SEEK_DATA: {e}"))),
        }
        match fs.seek_hole(file.as_fd(), offset) {
            Ok(hole) if hole >= end => {}
            Ok(hole) => {
                return Ok(Some(format!(
                    "SEEK_HOLE finds a hole in the data at {hole}"
                )));
            }
            Err(e) => return Ok(Some(format!("SEEK_HOLE: {e}"))),
        }
    }
    Ok(None)
}

/// Writes all of `buf` to `file` at `offset`.
fn write_all_at(fs: &dyn FsOps, file: &OwnedFd, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match fs.write_at(file.as_fd(), buf, offset)? {
            0 => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            n => {
                buf = &buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

/// Whether `e` is the error for a full filesystem or quota.
fn out_of_space(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT))
}

/// Whether `e`, or an error below it, is [`out_of_space`].
fn from_lack_of_space(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|cause| cause.downcast_ref::<io::Error>().is_some_and(out_of_space))
}

/// The OpenZFS module's version, as `/sys/module/zfs/version` gives it, if
/// the cache branch is on OpenZFS.
fn openzfs_version(cache_branch: &Branch) -> Option<io::Result<String>> {
    #[cfg(feature = "fault-injection")]
    if let Some(version) = crate::fault::zfs_version() {
        return Some(Ok(version));
    }
    // `f_type` is an `i64` only on some architectures.
    #[allow(clippy::unnecessary_cast)]
    let on_zfs = rustix::fs::fstatfs(&cache_branch.fd)
        .is_ok_and(|stat| stat.f_type as i64 == ZFS_SUPER_MAGIC);
    on_zfs
        .then(|| std::fs::read_to_string(ZFS_VERSION_FILE).map(|version| version.trim().to_owned()))
}

/// The release of an OpenZFS module version, such as `(2, 2, 1)` for
/// `2.2.1-1ubuntu1`, or `(2, 2, 0)` for the release candidate `2.2.0-rc4`.
fn release(version: &str) -> Option<(u32, u32, u32)> {
    let release = version.split(['-', '_']).next()?;
    let mut parts = release.split('.').map(|part| part.parse().ok());
    let release = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(release)
}

/// Whether OpenZFS `release` can report data as holes: 2.2.0 and 2.2.1. The
/// bug is older (docs/research/mergerfs-and-openzfs-semantics.md), but
/// releases before 2.2.0 have no no-replace renames, so the probe refuses
/// them anyway.
fn reports_data_as_holes(release: (u32, u32, u32)) -> bool {
    ((2, 2, 0)..(2, 2, 2)).contains(&release)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use anyhow::anyhow;

    use super::*;

    #[test]
    fn releases_are_read_from_module_versions() {
        for (version, expected) in [
            ("2.2.1-1", Some((2, 2, 1))),
            ("2.3.4-1ubuntu1", Some((2, 3, 4))),
            ("2.2.0-rc4", Some((2, 2, 0))),
            ("2.4.2", Some((2, 4, 2))),
            ("2.2", None),
            ("2.2.1.5", None),
            ("unknown", None),
            ("", None),
        ] {
            assert_eq!(release(version), expected, "{version}");
        }
    }

    #[test]
    fn only_releases_with_the_hole_reporting_bug_report_data_as_holes() {
        for (release, affected) in [
            ((2, 1, 15), false),
            ((2, 2, 0), true),
            ((2, 2, 1), true),
            ((2, 2, 2), false),
            ((2, 3, 4), false),
        ] {
            assert_eq!(reports_data_as_holes(release), affected, "{release:?}");
        }
    }

    #[test]
    fn lack_of_space_is_found_below_context() {
        let e = anyhow::Error::from(io::Error::from_raw_os_error(libc::ENOSPC))
            .context("create scratch file");
        assert!(from_lack_of_space(&e));
        let e = anyhow!(io::Error::from_raw_os_error(libc::EIO)).context("create scratch file");
        assert!(!from_lack_of_space(&e));
    }

    #[test]
    fn scratch_names_never_name_a_file_of_the_staging_area() {
        assert!(
            !OsStr::new(STAGING)
                .as_bytes()
                .starts_with(SCRATCH_PREFIX.as_bytes())
        );
    }
}
