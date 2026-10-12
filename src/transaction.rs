//! The move transaction: relocates one regular file from the cache branch to a
//! backing branch, so that afterwards it exists on exactly one of them.
//!
//! The source is held under a lease for the whole move, so a move unit that is
//! in use is never moved.
//!
//! Each staging directory mirrors relpaths (`<root>/.cachy-staging/<relpath>`),
//! so anything left in it after a crash names the move it belongs to.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::fsops::{FsOps, Lease, Stat};

/// Name of the staging directory at the root of each filesystem cachy writes to.
pub const STAGING: &str = ".cachy-staging";

/// How long a retired source stays in the staging directory before it is
/// unlinked. An open that looked up the source before it was retired, but
/// reaches the lease check only after this, is not caught.
const RETIRE_WAIT: Duration = Duration::from_secs(1);
/// How many moves may be retiring at once. Each holds four descriptors open,
/// which keeps cachy well below the usual limit of 1024.
const MAX_RETIRING: usize = 128;
/// How often retiring moves are serviced while nothing else is running.
const RETIRING_POLL: Duration = Duration::from_millis(10);
const COPY_BUF: usize = 1 << 20;
pub const DIR_MODE: u32 = 0o755;
const STAGING_MODE: u32 = 0o700;
const TEMP_MODE: u32 = 0o600;

/// [`RETIRE_WAIT`], unless a fault-injection build is told otherwise.
fn retire_wait() -> Duration {
    #[cfg(feature = "fault-injection")]
    if let Some(wait) = crate::fault::retire_wait() {
        return wait;
    }
    RETIRE_WAIT
}

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

pub enum MoveOutcome {
    Moved,
    /// The move unit was left where it is, untouched.
    Skipped(Skip),
}

/// Why a move unit was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Another process had it open in a way that conflicts with the lease, or
    /// opened it so during the move.
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

/// Runs moves one after another. A move's last step, the wait before a
/// retired source is unlinked, overlaps with the moves after it: such moves
/// stay in a queue of retiring moves, which is serviced between moves and
/// between the chunks of every copy.
pub struct Mover<'a> {
    fs: &'a dyn FsOps,
    cache_branch: &'a Branch,
    backing_branch: &'a Branch,
    guard: &'a Guard,
    retiring: VecDeque<Retiring<'a>>,
    finished: Vec<(&'a Path, Result<MoveOutcome>)>,
}

/// A move whose source is retired but not yet unlinked.
struct Retiring<'a> {
    source: Source<'a>,
    /// The mirror of the source's parent in the cache branch's staging
    /// directory, which holds the retired source.
    retired_parent: OwnedFd,
    published: Published<'a>,
    retired_at: Instant,
}

impl<'a> Mover<'a> {
    pub fn new(
        fs: &'a dyn FsOps,
        cache_branch: &'a Branch,
        backing_branch: &'a Branch,
        guard: &'a Guard,
    ) -> Mover<'a> {
        Mover {
            fs,
            cache_branch,
            backing_branch,
            guard,
            retiring: VecDeque::new(),
            finished: Vec::new(),
        }
    }

    /// Moves the regular file `rel` from the cache branch to the backing
    /// branch, unless the guard finds it in use. The outcome is reported by
    /// [`Mover::finished`], possibly only after later moves have started.
    ///
    /// A skipped move, or one that fails before publish, leaves the source
    /// where it was and no copy behind. A stale copy at `rel` on the backing
    /// branch is not replaced yet: the publish fails and the move with it.
    pub fn start(&mut self, rel: &'a Path) {
        self.service();
        while self.retiring.len() >= MAX_RETIRING {
            thread::sleep(RETIRING_POLL);
            self.service();
        }
        match self.transact(rel) {
            Ok(()) => {}
            Err(Abort::Skip(skip)) => self.finished.push((rel, Ok(MoveOutcome::Skipped(skip)))),
            Err(Abort::Fail(e)) => self.finished.push((rel, Err(e))),
        }
    }

    /// Space on the cache branch that retiring moves free once their sources
    /// are unlinked.
    pub fn retiring_bytes(&self) -> u64 {
        self.retiring.iter().map(|r| r.source.allocated).sum()
    }

    /// Waits until every retiring move has finished.
    pub fn finish(&mut self) {
        self.service();
        while !self.retiring.is_empty() {
            thread::sleep(RETIRING_POLL);
            self.service();
        }
    }

    /// The outcomes of the moves that finished since the last call.
    pub fn finished(&mut self) -> Vec<(&'a Path, Result<MoveOutcome>)> {
        std::mem::take(&mut self.finished)
    }

    /// Finishes retiring moves: one whose lease broke is rolled back at once,
    /// because the opener that broke it stays blocked until then; one whose
    /// wait is over has its source unlinked.
    fn service(&mut self) {
        for retiring in std::mem::take(&mut self.retiring) {
            let rel = retiring.source.rel;
            let finished = match retiring.source.check_lease() {
                Err(abort) => Some(retiring.roll_back(self.fs).and(Err(abort))),
                Ok(()) if retiring.retired_at.elapsed() >= retire_wait() => {
                    Some(retiring.unlink(self.fs))
                }
                Ok(()) => {
                    self.retiring.push_back(retiring);
                    None
                }
            };
            if let Some(result) = finished {
                self.finished.push((rel, outcome(result)));
            }
        }
    }

    /// Runs a move up to its retire, after which it waits in the queue of
    /// retiring moves.
    fn transact(&mut self, rel: &'a Path) -> Result<(), Abort> {
        let fs = self.fs;
        let (parent, name) = split(rel);

        let source = Source::open(fs, self.cache_branch, rel, self.guard)?;
        let (backing_staging, staged_parent) = staging_dirs(fs, self.backing_branch, parent)
            .context("create staging directory on the backing branch")?;

        let temp = fs
            .create_new(staged_parent.as_fd(), name, TEMP_MODE)
            .context("create temp file")?;
        let staged = self
            .copy(&source, &temp)
            .and_then(|()| Ok(fs.fsync(temp.as_fd()).context("fsync temp file")?))
            .and_then(|()| source.revalidate());
        let published = staged.and_then(|()| {
            Ok(publish(
                fs,
                self.backing_branch,
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

        let retiring = retire(fs, source, published?)?;
        self.retiring.push_back(retiring);
        Ok(())
    }

    /// Copies the source into `dst`, aborting as soon as the source's lease
    /// breaks. Retiring moves are serviced between chunks, so a long copy
    /// does not hold them up.
    fn copy(&mut self, src: &Source, dst: &OwnedFd) -> Result<(), Abort> {
        let mut buf = vec![0; COPY_BUF];
        loop {
            self.service();
            src.check_lease()?;
            let n = self
                .fs
                .read(src.file.as_fd(), &mut buf)
                .context("copy: read source")?;
            if n == 0 {
                return Ok(());
            }
            let mut chunk = &buf[..n];
            while !chunk.is_empty() {
                let written = self
                    .fs
                    .write(dst.as_fd(), chunk)
                    .context("copy: write temp file")?;
                chunk = &chunk[written..];
            }
        }
    }
}

fn outcome(result: Result<(), Abort>) -> Result<MoveOutcome> {
    match result {
        Ok(()) => Ok(MoveOutcome::Moved),
        Err(Abort::Skip(skip)) => Ok(MoveOutcome::Skipped(skip)),
        Err(Abort::Fail(e)) => Err(e),
    }
}

/// A published copy, identified so that a rollback removes it only while it
/// is still cachy's own.
struct Published<'a> {
    dir: OwnedFd,
    name: &'a Path,
    /// The copy as it was right after publishing.
    identity: Identity,
    /// The copy was already on the backing branch, not written by this move.
    adopted: bool,
}

impl Published<'_> {
    /// Removes the copy, unless it was adopted or is no longer cachy's own.
    fn remove(&self, fs: &dyn FsOps) -> Result<()> {
        if self.adopted {
            return Ok(());
        }
        match fs.stat_at(self.dir.as_fd(), self.name) {
            Ok(stat) if Identity::of(&stat) == self.identity => {}
            // Replaced, changed or removed since: no longer cachy's own.
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
/// `<backing branch>/<parent>/<name>`, never replacing an existing file. An
/// existing file identical to the copy is adopted as the published copy
/// instead.
fn publish<'a>(
    fs: &dyn FsOps,
    backing_branch: &Branch,
    parent: &Path,
    name: &'a Path,
    staged_parent: &OwnedFd,
    copy: &OwnedFd,
) -> Result<Published<'a>> {
    let dest_parent = make_dirs(fs, &backing_branch.fd, parent, DIR_MODE)
        .context("create destination directory")?;
    match fs.rename_noreplace(staged_parent.as_fd(), name, dest_parent.as_fd(), name) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return match adopt_identical(fs, dest_parent, name, staged_parent) {
                Ok(Some(published)) => Ok(published),
                _ => Err(e).context("publish"),
            };
        }
        Err(e) => return Err(e).context("publish"),
    }
    let identity = fs
        .fsync(dest_parent.as_fd())
        .context("fsync destination directory")
        .and_then(|()| {
            fs.fsync(staged_parent.as_fd())
                .context("fsync staging directory on the backing branch")
        })
        // After the rename, which may have changed the copy's ctime.
        .and_then(|()| {
            Ok(Identity::of(
                &fs.fstat(copy.as_fd()).context("fstat published copy")?,
            ))
        });
    match identity {
        Ok(identity) => Ok(Published {
            dir: dest_parent,
            name,
            identity,
            adopted: false,
        }),
        Err(e) => {
            unpublish(fs, &dest_parent, name, copy);
            Err(e)
        }
    }
}

/// Removes the copy published as `<dest_parent>/<name>` after a failure, if
/// that name still refers to it. If it cannot, the copy stays as a stale copy,
/// identical to the source.
fn unpublish(fs: &dyn FsOps, dest_parent: &OwnedFd, name: &Path, copy: &OwnedFd) {
    let inode = |stat: Stat| (stat.st_dev, stat.st_ino);
    let ours = match (
        fs.fstat(copy.as_fd()),
        fs.stat_at(dest_parent.as_fd(), name),
    ) {
        (Ok(copy), Ok(at_name)) => inode(copy) == inode(at_name),
        _ => false,
    };
    if ours && fs.unlink(dest_parent.as_fd(), name).is_ok() {
        let _ = fs.fsync(dest_parent.as_fd());
    }
}

/// Adopts the existing file `<dest_parent>/<name>` as the published copy if
/// it is identical to the staged copy, which is then deleted. A run that was
/// interrupted between publish and retire leaves such a file behind.
///
/// A rollback keeps an adopted copy, as a stale copy identical to the source
/// it puts back: cachy did not write it.
fn adopt_identical<'a>(
    fs: &dyn FsOps,
    dest_parent: OwnedFd,
    name: &'a Path,
    staged_parent: &OwnedFd,
) -> io::Result<Option<Published<'a>>> {
    let before = Identity::of(&fs.stat_at(dest_parent.as_fd(), name)?);
    let staged = fs.open_read(staged_parent.as_fd(), name)?;
    let existing = fs.open_read(dest_parent.as_fd(), name)?;
    if !same_content(fs, &staged, &existing)? {
        return Ok(None);
    }
    let identity = Identity::of(&fs.stat_at(dest_parent.as_fd(), name)?);
    if identity != before {
        return Ok(None);
    }
    fs.unlink(staged_parent.as_fd(), name)?;
    fs.fsync(staged_parent.as_fd())?;
    Ok(Some(Published {
        dir: dest_parent,
        name,
        identity,
        adopted: true,
    }))
}

/// Starts removing the source from the cache branch: renames it into the
/// cache branch's staging directory, where it waits for [`RETIRE_WAIT`] so an
/// open that looked up the source before the rename reaches the lease check
/// in time to roll the move back.
fn retire<'a>(
    fs: &dyn FsOps,
    source: Source<'a>,
    published: Published<'a>,
) -> Result<Retiring<'a>, Abort> {
    let (parent, name) = split(source.rel);
    let renamed = staging_dirs(fs, source.cache_branch, parent)
        .context("create staging directory on the cache branch")
        .and_then(|(_, retired_parent)| {
            fs.rename_noreplace(source.dir.as_fd(), name, retired_parent.as_fd(), name)
                .context("retire: rename source into staging directory")?;
            Ok(retired_parent)
        });
    let retired_parent = match renamed {
        Ok(retired_parent) => retired_parent,
        Err(e) => {
            // The source is still in place. A published copy that cannot be
            // removed stays as a stale copy, identical to the source.
            let _ = published.remove(fs);
            return Err(Abort::Fail(e));
        }
    };
    let retiring = Retiring {
        source,
        retired_parent,
        published,
        retired_at: Instant::now(),
    };
    // From here on, every way out either unlinks the retired source or puts
    // it back, so no opener blocked on the lease is left with a hidden file.
    if let Err(e) = fs
        .fsync(retiring.source.dir.as_fd())
        .context("fsync source directory")
    {
        retiring.roll_back(fs)?;
        return Err(Abort::Fail(e));
    }
    Ok(retiring)
}

impl Retiring<'_> {
    fn unlink(self, fs: &dyn FsOps) -> Result<(), Abort> {
        let name = split(self.source.rel).1;
        let unlinked = fs
            .unlink(self.retired_parent.as_fd(), name)
            .context("retire: unlink source")
            .and_then(|()| {
                fs.fsync(self.retired_parent.as_fd())
                    .context("fsync staging directory on the cache branch")
            });
        self.remove_empty_staging_dirs(fs);
        Ok(unlinked?)
    }

    /// Undoes the move: renames the source back and removes the published
    /// copy. The source's lease is still held, so whoever broke it opens the
    /// source only once it is back.
    fn roll_back(self, fs: &dyn FsOps) -> Result<(), Abort> {
        let name = split(self.source.rel).1;
        let rolled_back = fs
            .rename_noreplace(
                self.retired_parent.as_fd(),
                name,
                self.source.dir.as_fd(),
                name,
            )
            .context("roll back: rename source back")
            .and_then(|()| {
                fs.fsync(self.source.dir.as_fd())
                    .context("fsync source directory")
            })
            .and_then(|()| {
                fs.fsync(self.retired_parent.as_fd())
                    .context("fsync staging directory on the cache branch")
            })
            .and_then(|()| self.published.remove(fs).context("roll back"));
        self.remove_empty_staging_dirs(fs);
        Ok(rolled_back?)
    }

    fn remove_empty_staging_dirs(&self, fs: &dyn FsOps) {
        let staging = fs.open_dir(self.source.cache_branch.fd.as_fd(), Path::new(STAGING));
        if let Ok(staging) = staging {
            remove_empty_dirs(fs, &staging, split(self.source.rel).0);
        }
    }
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
    cache_branch: &'a Branch,
    rel: &'a Path,
    /// The parent directory, as opened when the move started.
    dir: OwnedFd,
    file: OwnedFd,
    lease: Lease,
    /// The source as it was when the lease was taken.
    identity: Identity,
    /// Space the source takes up on the cache branch.
    allocated: u64,
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
    /// Opens `rel` on the cache branch and takes a lease on it. Skips it if
    /// another open conflicts with the lease, or if it is younger than the
    /// minimum age.
    fn open(
        fs: &'a dyn FsOps,
        cache_branch: &'a Branch,
        rel: &'a Path,
        guard: &Guard,
    ) -> Result<Source<'a>, Abort> {
        let (parent, name) = split(rel);
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
        let stat = fs.fstat(file.as_fd()).context("fstat source")?;
        let identity = Identity::of(&stat);
        if identity.is_younger_than(guard.minimum_age) {
            return Err(Abort::Skip(Skip::TooYoung));
        }
        Ok(Source {
            fs,
            cache_branch,
            rel,
            dir,
            file,
            lease,
            identity,
            allocated: stat.st_blocks.max(0) as u64 * 512,
        })
    }

    /// Checks, right before publishing, that the copy is still current: the
    /// lease is held, the source is unchanged, and its relpath, looked up
    /// afresh from the cache branch root, still names it. An atomic save or a
    /// renamed directory during the copy fails the last check.
    fn revalidate(&self) -> Result<(), Abort> {
        self.check_lease()?;
        let now = self.fs.fstat(self.file.as_fd()).context("fstat source")?;
        if Identity::of(&now) != self.identity {
            return Err(Abort::Skip(Skip::Changed));
        }
        let (parent, name) = split(self.rel);
        let at_path = open_dirs(self.fs, &self.cache_branch.fd, parent)
            .and_then(|dir| self.fs.stat_at(dir.as_fd(), name));
        match at_path {
            Ok(stat) if Identity::of(&stat) == self.identity => Ok(()),
            Ok(_) => Err(Abort::Skip(Skip::Changed)),
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
                ) =>
            {
                Err(Abort::Skip(Skip::Changed))
            }
            Err(e) => Err(anyhow::Error::from(e).context("look up source path").into()),
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

/// Whether the open files `a` and `b`, read from their start, are regular
/// files with the same content.
pub fn same_content(fs: &dyn FsOps, a: &OwnedFd, b: &OwnedFd) -> io::Result<bool> {
    let (a_stat, b_stat) = (fs.fstat(a.as_fd())?, fs.fstat(b.as_fd())?);
    let regular = |stat: &Stat| stat.st_mode & libc::S_IFMT == libc::S_IFREG;
    if !regular(&a_stat) || !regular(&b_stat) || a_stat.st_size != b_stat.st_size {
        return Ok(false);
    }
    let (mut a_buf, mut b_buf) = (vec![0; COPY_BUF], vec![0; COPY_BUF]);
    loop {
        let n = read_full(fs, a, &mut a_buf)?;
        if n != read_full(fs, b, &mut b_buf)? || a_buf[..n] != b_buf[..n] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// Reads into `buf` until it is full or the file ends.
fn read_full(fs: &dyn FsOps, fd: &OwnedFd, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match fs.read(fd.as_fd(), &mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Opens `path` below `root`, creating missing directories. Each created
/// directory's parent is fsynced so the new entry is durable.
pub fn make_dirs(fs: &dyn FsOps, root: &OwnedFd, path: &Path, mode: u32) -> io::Result<OwnedFd> {
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

pub fn open_dirs(fs: &dyn FsOps, root: &OwnedFd, path: &Path) -> io::Result<OwnedFd> {
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

pub fn split(rel: &Path) -> (&Path, &Path) {
    let parent = rel.parent().unwrap_or(Path::new(""));
    let name = Path::new(rel.file_name().expect("a move unit's relpath names a file"));
    (parent, name)
}
