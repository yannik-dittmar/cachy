//! The move transaction: relocates one regular file from the cache branch to a
//! backing branch, so that afterwards it exists on exactly one of them.
//!
//! The source is held under a lease for the whole move, so a move unit that is
//! in use is never moved.
//!
//! Each staging directory mirrors relpaths (`<root>/.cachy-staging/<relpath>`),
//! so anything left in it after a crash names the move it belongs to.

use std::fmt;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::fsops::{FsOps, Lease, Stat};

/// Name of the staging directory at the root of each filesystem cachy writes to.
pub const STAGING: &str = ".cachy-staging";

/// How long a retired source stays in the staging directory before it is
/// unlinked. An open that looked up the source before it was retired, but
/// reaches the lease check only after this, is not caught.
const RETIRE_GRACE: Duration = Duration::from_secs(1);
const COPY_BUF: usize = 1 << 20;
const DIR_MODE: u32 = 0o755;
const STAGING_MODE: u32 = 0o700;
const TEMP_MODE: u32 = 0o600;

/// A branch root, held open so every operation is relative to it.
pub struct Branch {
    pub path: std::path::PathBuf,
    pub fd: OwnedFd,
}

impl Branch {
    pub fn open(path: &Path) -> io::Result<Branch> {
        let dir = std::fs::File::open(path)?;
        if !dir.metadata()?.is_dir() {
            return Err(io::Error::from(io::ErrorKind::NotADirectory));
        }
        Ok(Branch {
            path: path.to_owned(),
            fd: dir.into(),
        })
    }
}

pub enum Outcome {
    Moved,
    /// The move unit was left where it is, untouched.
    Skipped(Skip),
}

/// Why a move unit was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Another process has it open for writing, or opened it during the move.
    InUse,
    /// It was changed, replaced or renamed during the move.
    Changed,
    /// It changed more recently than the minimum age.
    TooYoung,
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Skip::InUse => "in use",
            Skip::Changed => "changed",
            Skip::TooYoung => "too young",
        })
    }
}

/// What keeps a move away from move units that are in use.
pub struct Guard {
    /// The lease held on the source during a move. Any open that conflicts
    /// with it, before or during the move, makes the move unit in use.
    pub lease: Lease,
    /// Move units that changed more recently than this are never moved.
    pub minimum_age: Duration,
}

/// Why a move stopped short: a clean skip, or a failure.
enum Abort {
    Skip(Skip),
    Fail(anyhow::Error),
}

impl From<anyhow::Error> for Abort {
    fn from(e: anyhow::Error) -> Abort {
        Abort::Fail(e)
    }
}

/// Moves the regular file `rel` from the cache branch to the backing branch,
/// unless `guard` finds it in use. A skipped move, or one that fails before
/// publish, leaves the source where it was and no copy behind.
///
/// A stale copy at `rel` on the backing branch is not replaced yet: the
/// publish fails and the move with it.
pub fn move_file(
    fs: &dyn FsOps,
    cache_branch: &Branch,
    backing_branch: &Branch,
    rel: &Path,
    guard: &Guard,
) -> Result<Outcome> {
    match transact(fs, cache_branch, backing_branch, rel, guard) {
        Ok(()) => Ok(Outcome::Moved),
        Err(Abort::Skip(skip)) => Ok(Outcome::Skipped(skip)),
        Err(Abort::Fail(e)) => Err(e),
    }
}

fn transact(
    fs: &dyn FsOps,
    cache_branch: &Branch,
    backing_branch: &Branch,
    rel: &Path,
    guard: &Guard,
) -> Result<(), Abort> {
    let (parent, name) = split(rel);

    let source = Source::open(fs, cache_branch, parent, name, guard)?;
    let (backing_staging, staged_parent) = staging_dirs(fs, backing_branch, parent)
        .context("create staging directory on the backing branch")?;

    let temp = fs
        .create_new(staged_parent.as_fd(), name, TEMP_MODE)
        .context("create temp file")?;
    let staged = copy(fs, &source, &temp)
        .and_then(|()| Ok(fs.fsync(temp.as_fd()).context("fsync temp file")?))
        .and_then(|()| source.revalidate());
    let published = staged.and_then(|()| {
        Ok(publish(
            fs,
            backing_branch,
            parent,
            name,
            &staged_parent,
            &temp,
        )?)
    });
    drop(temp);
    if published.is_err() {
        let _ = fs.unlink(staged_parent.as_fd(), name);
    }
    remove_empty_dirs(fs, &backing_staging, parent);

    retire(fs, cache_branch, &source, parent, &published?)
}

/// A published copy, identified so that a rollback removes it only while it
/// is still cachy's own.
struct Published<'a> {
    dir: OwnedFd,
    name: &'a Path,
    dev: u64,
    ino: u64,
}

impl Published<'_> {
    fn remove(&self, fs: &dyn FsOps) -> Result<()> {
        match fs.stat_at(self.dir.as_fd(), self.name) {
            Ok(stat) if (stat.st_dev, stat.st_ino) == (self.dev, self.ino) => {}
            // Replaced or removed since: no longer cachy's to remove.
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).context("stat published copy"),
        }
        fs.unlink(self.dir.as_fd(), self.name)
            .context("remove published copy")?;
        fs.fsync(self.dir.as_fd())
            .context("fsync destination directory")
    }
}

/// Makes the staged temp file `copy` visible as
/// `<backing branch>/<parent>/<name>`, never replacing an existing file.
fn publish<'a>(
    fs: &dyn FsOps,
    backing_branch: &Branch,
    parent: &Path,
    name: &'a Path,
    staged_parent: &OwnedFd,
    copy: &OwnedFd,
) -> Result<Published<'a>> {
    let stat = fs.fstat(copy.as_fd()).context("fstat temp file")?;
    let dest_parent = make_dirs(fs, &backing_branch.fd, parent, DIR_MODE)
        .context("create destination directory")?;
    fs.rename_noreplace(staged_parent.as_fd(), name, dest_parent.as_fd(), name)
        .context("publish")?;
    fs.fsync(dest_parent.as_fd())
        .context("fsync destination directory")?;
    fs.fsync(staged_parent.as_fd())
        .context("fsync staging directory on the backing branch")?;
    Ok(Published {
        dir: dest_parent,
        name,
        dev: stat.st_dev,
        ino: stat.st_ino,
    })
}

/// Removes the source from the cache branch. It is first renamed into the
/// cache branch's staging directory and kept there for a grace period, so an
/// open that looked up the source before the rename reaches the lease check
/// in time to roll the move back.
fn retire(
    fs: &dyn FsOps,
    cache_branch: &Branch,
    source: &Source,
    parent: &Path,
    published: &Published,
) -> Result<(), Abort> {
    let name = source.name;
    let (cache_staging, retired_parent) = staging_dirs(fs, cache_branch, parent)
        .context("create staging directory on the cache branch")?;

    fs.rename_noreplace(source.dir.as_fd(), name, retired_parent.as_fd(), name)
        .context("retire: rename source into staging directory")?;
    fs.fsync(source.dir.as_fd())
        .context("fsync source directory")?;
    thread::sleep(RETIRE_GRACE);
    let retired = match source.check_lease() {
        Ok(()) => unlink_retired(fs, &retired_parent, name),
        Err(abort) => roll_back(fs, source, &retired_parent, published).and(Err(abort)),
    };
    remove_empty_dirs(fs, &cache_staging, parent);
    retired
}

fn unlink_retired(fs: &dyn FsOps, retired_parent: &OwnedFd, name: &Path) -> Result<(), Abort> {
    fs.unlink(retired_parent.as_fd(), name)
        .context("retire: unlink source")?;
    fs.fsync(retired_parent.as_fd())
        .context("fsync staging directory on the cache branch")?;
    Ok(())
}

/// Undoes a move whose source is retired but not yet unlinked: renames the
/// source back and removes the published copy. The source's lease is still
/// held, so whoever broke it opens the source only once it is back.
fn roll_back(
    fs: &dyn FsOps,
    source: &Source,
    retired_parent: &OwnedFd,
    published: &Published,
) -> Result<(), Abort> {
    let name = source.name;
    fs.rename_noreplace(retired_parent.as_fd(), name, source.dir.as_fd(), name)
        .context("roll back: rename source back")?;
    fs.fsync(source.dir.as_fd())
        .context("fsync source directory")?;
    fs.fsync(retired_parent.as_fd())
        .context("fsync staging directory on the cache branch")?;
    published.remove(fs).context("roll back")?;
    Ok(())
}

/// Opens `branch`'s staging directory and, below it, the mirror of `parent`,
/// creating both as needed.
fn staging_dirs(fs: &dyn FsOps, branch: &Branch, parent: &Path) -> io::Result<(OwnedFd, OwnedFd)> {
    let staging = make_dirs(fs, &branch.fd, Path::new(STAGING), STAGING_MODE)?;
    let mirrored_parent = make_dirs(fs, &staging, parent, STAGING_MODE)?;
    Ok((staging, mirrored_parent))
}

/// The source of a move, held open under a lease for the whole move.
struct Source<'a> {
    fs: &'a dyn FsOps,
    /// The source's directory on the cache branch.
    dir: OwnedFd,
    name: &'a Path,
    file: OwnedFd,
    lease: Lease,
    /// The source as it was when the lease was taken.
    identity: Identity,
}

/// What must stay the same about a source for its copy to be current.
#[derive(Debug, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    size: i64,
    mtime: (i64, u64),
    ctime: (i64, u64),
}

impl Identity {
    fn of(stat: &Stat) -> Identity {
        Identity {
            dev: stat.st_dev,
            ino: stat.st_ino,
            size: stat.st_size,
            mtime: (stat.st_mtime, stat.st_mtime_nsec),
            ctime: (stat.st_ctime, stat.st_ctime_nsec),
        }
    }

    /// Whether the source changed (by its later of mtime and ctime) less than
    /// `minimum_age` ago, or claims to have changed in the future.
    fn is_younger_than(&self, minimum_age: Duration) -> bool {
        let (secs, nanos) = self.mtime.max(self.ctime);
        // Times before the epoch are old enough either way.
        let since_epoch = Duration::new(secs.max(0) as u64, nanos as u32);
        match UNIX_EPOCH.checked_add(since_epoch) {
            Some(last_change) => match SystemTime::now().duration_since(last_change) {
                Ok(age) => age < minimum_age,
                Err(_) => true,
            },
            None => true,
        }
    }
}

impl<'a> Source<'a> {
    /// Opens `parent/name` on the cache branch and takes a lease on it. Skips
    /// it if another open conflicts with the lease, or if it is younger than
    /// the minimum age.
    fn open(
        fs: &'a dyn FsOps,
        cache_branch: &Branch,
        parent: &Path,
        name: &'a Path,
        guard: &Guard,
    ) -> Result<Source<'a>, Abort> {
        let dir = open_dirs(fs, &cache_branch.fd, parent).context("open source directory")?;
        let file = fs.open_read(dir.as_fd(), name).context("open source")?;
        let lease = guard.lease;
        match fs.set_lease(file.as_fd(), lease) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(Abort::Skip(Skip::InUse));
            }
            Err(e) => return Err(anyhow::Error::from(e).context("take lease").into()),
        }
        let identity = Identity::of(&fs.fstat(file.as_fd()).context("fstat source")?);
        if identity.is_younger_than(guard.minimum_age) {
            return Err(Abort::Skip(Skip::TooYoung));
        }
        Ok(Source {
            fs,
            dir,
            name,
            file,
            lease,
            identity,
        })
    }

    /// Checks, right before publishing, that the copy is still current: the
    /// lease is held, the source is unchanged, and its path still names it.
    /// An atomic save during the copy fails the last check.
    fn revalidate(&self) -> Result<(), Abort> {
        self.check_lease()?;
        let now = self.fs.fstat(self.file.as_fd()).context("fstat source")?;
        if Identity::of(&now) != self.identity {
            return Err(Abort::Skip(Skip::Changed));
        }
        match self.fs.stat_at(self.dir.as_fd(), self.name) {
            Ok(at_path) if (at_path.st_dev, at_path.st_ino) == (now.st_dev, now.st_ino) => Ok(()),
            Ok(_) => Err(Abort::Skip(Skip::Changed)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Abort::Skip(Skip::Changed)),
            Err(e) => Err(anyhow::Error::from(e).context("stat source path").into()),
        }
    }

    /// Skips the move as in use once a conflicting open has started to break
    /// the lease. The opener stays blocked until the source is closed.
    fn check_lease(&self) -> Result<(), Abort> {
        let lease = self.fs.lease(self.file.as_fd()).context("query lease")?;
        if lease != self.lease {
            return Err(Abort::Skip(Skip::InUse));
        }
        Ok(())
    }
}

/// Copies the source into `dst`, aborting as soon as the source's lease
/// breaks.
fn copy(fs: &dyn FsOps, src: &Source, dst: &OwnedFd) -> Result<(), Abort> {
    let mut buf = vec![0; COPY_BUF];
    loop {
        src.check_lease()?;
        let n = fs
            .read(src.file.as_fd(), &mut buf)
            .context("copy: read source")?;
        if n == 0 {
            return Ok(());
        }
        let mut chunk = &buf[..n];
        while !chunk.is_empty() {
            let written = fs
                .write(dst.as_fd(), chunk)
                .context("copy: write temp file")?;
            chunk = &chunk[written..];
        }
    }
}

/// Opens `path` below `root`, creating missing directories. Each created
/// directory's parent is fsynced so the new entry is durable.
fn make_dirs(fs: &dyn FsOps, root: &OwnedFd, path: &Path, mode: u32) -> io::Result<OwnedFd> {
    let mut dir = fs.open_dir(root.as_fd(), Path::new("."))?;
    for component in normal_components(path) {
        let created = match fs.mkdir(dir.as_fd(), component, mode) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(e),
        };
        let child = fs.open_dir(dir.as_fd(), component)?;
        if created {
            fs.fsync(dir.as_fd())?;
        }
        dir = child;
    }
    Ok(dir)
}

fn open_dirs(fs: &dyn FsOps, root: &OwnedFd, path: &Path) -> io::Result<OwnedFd> {
    let mut dir = fs.open_dir(root.as_fd(), Path::new("."))?;
    for component in normal_components(path) {
        dir = fs.open_dir(dir.as_fd(), component)?;
    }
    Ok(dir)
}

/// Removes `path` below `root` and its ancestors (but not `root`) while they
/// are empty.
fn remove_empty_dirs(fs: &dyn FsOps, root: &OwnedFd, path: &Path) {
    for dir in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        if fs.rmdir(root.as_fd(), dir).is_err() {
            return;
        }
    }
}

fn normal_components(path: &Path) -> impl Iterator<Item = &Path> {
    path.components().filter_map(|c| match c {
        Component::Normal(name) => Some(Path::new(name)),
        _ => None,
    })
}

fn split(rel: &Path) -> (&Path, &Path) {
    let parent = rel.parent().unwrap_or(Path::new(""));
    let name = Path::new(rel.file_name().expect("a move unit's relpath names a file"));
    (parent, name)
}
