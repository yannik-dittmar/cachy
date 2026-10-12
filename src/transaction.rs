//! The move transaction: relocates one move unit (a regular file, a hardlink
//! group or a symlink) from the cache branch to a backing branch, so that
//! afterwards it exists on exactly one of them.
//!
//! A regular file is held under a lease for the whole move, so a move unit
//! that is in use is never moved. A hardlink group moves as one inode: it is
//! copied once, published under its first name and linked under the others,
//! and all its names are retired together under the one lease, which any
//! open through any name breaks. A symlink cannot be opened, so it is never
//! in use; it is recreated with its link text verbatim.
//!
//! Each staging directory mirrors relpaths (`<root>/.cachy-staging/<relpath>`),
//! so anything left in it after a crash names the move it belongs to.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use xxhash_rust::xxh3::{Xxh3, xxh3_128};

use crate::config::Verify;
use crate::fsops::{FsOps, Lease, Stat};
use crate::metadata::{self, AllowedLosses};
use crate::scan::Kind;

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
    /// The move unit was moved; its content has this hash.
    Moved(Hash),
    /// The move unit was left where it is, untouched.
    Skipped(Skip),
}

/// The XXH3-128 hash of a file's content, computed while copying it, or of
/// a symlink's link text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hash(u128);

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
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
    /// It is a FIFO, socket or device node, which cachy never moves.
    SpecialFile,
    /// It is a hardlink group with names that the walk did not find.
    PartlyExcluded,
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Skip::InUse => "in use",
            Skip::Changed => "changed",
            Skip::TooYoung => "too young",
            Skip::SpecialFile => "special file",
            Skip::PartlyExcluded => "hardlink group partly excluded",
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
    allowed_losses: AllowedLosses,
    verify: Verify,
    /// Directories that moves created on the backing branch, by relpath.
    created_dirs: Vec<PathBuf>,
    retiring: VecDeque<Retiring<'a>>,
    finished: Vec<(&'a [PathBuf], Result<MoveOutcome>)>,
}

/// A move whose source is retired but not yet unlinked.
struct Retiring<'a> {
    source: Source<'a>,
    /// For each of the source's names that is retired, in order, the mirror
    /// of its parent in the cache branch's staging directory, which holds it.
    retired_parents: Vec<OwnedFd>,
    published: Published<'a>,
    hash: Hash,
    retired_at: Instant,
}

impl<'a> Mover<'a> {
    pub fn new(
        fs: &'a dyn FsOps,
        cache_branch: &'a Branch,
        backing_branch: &'a Branch,
        guard: &'a Guard,
        allowed_losses: AllowedLosses,
        verify: Verify,
    ) -> Mover<'a> {
        Mover {
            fs,
            cache_branch,
            backing_branch,
            guard,
            allowed_losses,
            verify,
            created_dirs: Vec::new(),
            retiring: VecDeque::new(),
            finished: Vec::new(),
        }
    }

    /// Moves the move unit of the given kind at `rels` (its names, as the
    /// scanner lists them) from the cache branch to the backing branch, unless
    /// the guard finds it in use; skips it if it is a special file or a partly
    /// excluded hardlink group. The outcome is reported by
    /// [`Mover::finished`], possibly only after later moves have started.
    ///
    /// A skipped move, or one that fails before publish, leaves the source
    /// where it was and no copy behind. A stale copy at one of `rels` on the
    /// backing branch is not replaced yet: the publish fails and the move
    /// with it.
    pub fn start(&mut self, rels: &'a [PathBuf], kind: Kind) {
        self.service();
        let finished = match kind {
            Kind::Regular => {
                while self.retiring.len() >= MAX_RETIRING {
                    thread::sleep(RETIRING_POLL);
                    self.service();
                }
                // A move that reaches its retire finishes once serviced.
                self.transact(rels).err().map(Err)
            }
            Kind::Symlink => Some(self.transact_symlink(&rels[0])),
            Kind::Special => Some(Err(Abort::Skip(Skip::SpecialFile))),
            Kind::PartlyExcluded => Some(Err(Abort::Skip(Skip::PartlyExcluded))),
        };
        if let Some(result) = finished {
            self.finished.push((rels, outcome(result)));
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

    /// Gives each directory that moves created on the backing branch the
    /// atime and mtime of its cache directory, now that no move adds to it
    /// any more. Call it after [`Mover::finish`]. Returns the relpaths of the
    /// directories whose times could not be set, with the error.
    pub fn date_created_dirs(&mut self) -> Vec<(PathBuf, anyhow::Error)> {
        let mut failed = Vec::new();
        for rel in std::mem::take(&mut self.created_dirs) {
            let dated = open_dirs(self.fs, &self.cache_branch.fd, &rel)
                .and_then(|cache_dir| self.fs.fstat(cache_dir.as_fd()))
                .context("stat cache directory")
                .and_then(|stat| {
                    let created = open_dirs(self.fs, &self.backing_branch.fd, &rel)
                        .context("open created directory")?;
                    metadata::copy_times(self.fs, &stat, created.as_fd()).context("metadata")
                });
            if let Err(e) = dated {
                failed.push((rel, e));
            }
        }
        failed
    }

    /// The outcomes of the moves that finished since the last call.
    pub fn finished(&mut self) -> Vec<(&'a [PathBuf], Result<MoveOutcome>)> {
        std::mem::take(&mut self.finished)
    }

    /// Finishes retiring moves: one whose lease broke is rolled back at once,
    /// because the opener that broke it stays blocked until then; one whose
    /// wait is over has its source unlinked.
    fn service(&mut self) {
        for retiring in std::mem::take(&mut self.retiring) {
            let (rels, hash) = (retiring.source.rels, retiring.hash);
            let finished = match retiring.source.check_lease() {
                Err(abort) => Some(retiring.roll_back(self.fs).and_then(|()| Err(abort))),
                Ok(()) if retiring.retired_at.elapsed() >= retire_wait() => {
                    Some(retiring.unlink(self.fs).map(|()| hash))
                }
                Ok(()) => {
                    self.retiring.push_back(retiring);
                    None
                }
            };
            if let Some(result) = finished {
                self.finished.push((rels, outcome(result)));
            }
        }
    }

    /// Runs a move of the regular file or hardlink group `rels` up to its
    /// retire, after which it waits in the queue of retiring moves.
    fn transact(&mut self, rels: &'a [PathBuf]) -> Result<(), Abort> {
        let fs = self.fs;
        let rel = &rels[0];
        let (parent, name) = split(rel);

        let source = Source::open(fs, self.cache_branch, rels, self.guard)?;
        let (backing_staging, staged_parent) = staging_dirs(fs, self.backing_branch, parent)
            .context("create staging directory on the backing branch")?;

        let temp = fs
            .create_new(staged_parent.as_fd(), name, TEMP_MODE)
            .context("create temp file")?;
        let staged = self.stage(&source, &temp, rel);
        let published = staged.and_then(|hash| {
            let dest_parent = make_dirs_like(
                fs,
                self.backing_branch,
                parent,
                &self.cache_branch.fd,
                self.allowed_losses,
                &mut self.created_dirs,
            )
            .context("create destination directory")?;
            let mut published = publish(fs, dest_parent, name, &staged_parent, &temp)?;
            for rel in &rels[1..] {
                if let Err(e) = self.link(&mut published, rel) {
                    // A link that cannot be removed stays as a stale copy,
                    // identical to the source.
                    let _ = published.remove(fs);
                    return Err(e.into());
                }
            }
            Ok((published, hash))
        });
        drop(temp);
        if published.is_err() {
            let _ = fs.unlink(staged_parent.as_fd(), name);
        }
        remove_empty_dirs(fs, &backing_staging, parent);

        let (published, hash) = published?;
        let retiring = retire(fs, source, published, hash)?;
        self.retiring.push_back(retiring);
        Ok(())
    }

    /// Publishes the copy of a hardlink group under its further name `rel`,
    /// as a link to the name it was published under first. A link to the
    /// copy that is already there, left by a crash, is adopted.
    fn link(&mut self, published: &mut Published<'a>, rel: &'a Path) -> Result<()> {
        let fs = self.fs;
        let (parent, name) = split(rel);
        let dest_parent = make_dirs_like(
            fs,
            self.backing_branch,
            parent,
            &self.cache_branch.fd,
            self.allowed_losses,
            &mut self.created_dirs,
        )
        .context("create destination directory")?;
        let first = &published.names[0];
        let linked = match fs.link(first.dir.as_fd(), first.name, dest_parent.as_fd(), name) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(e).with_context(|| format!("publish {}", rel.display())),
        };
        let stat = (|| {
            if linked {
                fs.fsync(dest_parent.as_fd())
                    .context("fsync destination directory")?;
            }
            fs.stat_at(dest_parent.as_fd(), name)
                .context("stat published link")
        })();
        let stat = match stat {
            Ok(stat) if published.identity.same_inode(&stat) => stat,
            Ok(_) if !linked => {
                return Err(anyhow!("publish {}: File exists", rel.display()));
            }
            // The first name was replaced before it was linked, so the link
            // is not to the copy.
            Ok(_) => {
                let _ = fs.unlink(dest_parent.as_fd(), name);
                return Err(anyhow!(
                    "publish {}: the published copy was replaced",
                    rel.display()
                ));
            }
            Err(e) => {
                if linked {
                    unpublish_inode(fs, &dest_parent, name, &published.identity);
                    // Linking and unlinking changed the copy's ctime, which
                    // would make a rollback take it for no longer its own.
                    let first = &published.names[0];
                    if let Ok(stat) = fs.stat_at(first.dir.as_fd(), first.name)
                        && published.identity.same_inode(&stat)
                    {
                        published.identity = Identity::of(&stat);
                    }
                }
                return Err(e);
            }
        };
        // Linking changed the copy's ctime.
        published.identity = Identity::of(&stat);
        published.names.push(PublishedName {
            dir: dest_parent,
            name,
            adopted: !linked,
        });
        Ok(())
    }

    /// Moves the symlink `rel`: recreates it with the same link text in the
    /// staging directory, gives it the source's owner and timestamps,
    /// publishes it and retires the source. Nothing can have a symlink open,
    /// so the move needs no lease and no retire wait.
    fn transact_symlink(&mut self, rel: &'a Path) -> Result<Hash, Abort> {
        let fs = self.fs;
        let (parent, name) = split(rel);

        let source = Link::open(fs, self.cache_branch, rel, self.guard)?;
        let (backing_staging, staged_parent) = staging_dirs(fs, self.backing_branch, parent)
            .context("create staging directory on the backing branch")?;

        let published = fs
            .symlink(&source.target, staged_parent.as_fd(), name)
            .context("create temp file")
            .and_then(|()| {
                metadata::copy_symlink(fs, &source.stat, staged_parent.as_fd(), name)
                    .context("metadata")
            })
            .map_err(Abort::from)
            .and_then(|()| source.revalidate())
            .and_then(|()| {
                let dest_parent = make_dirs_like(
                    fs,
                    self.backing_branch,
                    parent,
                    &self.cache_branch.fd,
                    self.allowed_losses,
                    &mut self.created_dirs,
                )
                .context("create destination directory")?;
                Ok(publish_symlink(fs, dest_parent, name, &staged_parent)?)
            });
        if published.is_err() {
            let _ = fs.unlink(staged_parent.as_fd(), name);
        }
        remove_empty_dirs(fs, &backing_staging, parent);

        let published = published?;
        let hash = Hash(xxh3_128(source.target.as_encoded_bytes()));
        source.retire(published)?;
        Ok(hash)
    }

    /// Writes the staged copy of `source` into the temp file `temp`, makes it
    /// durable and checks that it is still current. Returns the hash of its
    /// content.
    fn stage(&mut self, source: &Source, temp: &OwnedFd, rel: &Path) -> Result<Hash, Abort> {
        let fs = self.fs;
        let hash = self.copy(source, temp)?;
        if self.verify == Verify::Readback {
            // Before the metadata: reading the copy changes its atime.
            fs.fsync(temp.as_fd()).context("fsync temp file")?;
            self.read_back(source, temp, hash)?;
        }
        metadata::copy_file(
            fs,
            &source.stat,
            source.file.as_fd(),
            temp.as_fd(),
            self.allowed_losses,
            &self.backing_branch.path.join(rel),
        )
        .context("metadata")?;
        fs.fsync(temp.as_fd()).context("fsync temp file")?;
        source.revalidate()?;
        Ok(hash)
    }

    /// Copies the source into `dst`, leaving its holes as holes, and returns
    /// the hash of its content (holes read as zeros). Aborts as soon as the
    /// source's lease breaks. Retiring moves are serviced between chunks, so
    /// a long copy does not hold them up.
    fn copy(&mut self, src: &Source, dst: &OwnedFd) -> Result<Hash, Abort> {
        let fs = self.fs;
        let size = src.stat.st_size.max(0) as u64;
        let mut hasher = Xxh3::new();
        let mut buf = vec![0; COPY_BUF];
        let mut offset = 0;
        while offset < size {
            let data = fs
                .seek_data(src.file.as_fd(), offset)
                .context("copy: find data in source")?;
            let Some(data) = data.filter(|&data| data < size) else {
                break;
            };
            let hole = fs
                .seek_hole(src.file.as_fd(), data)
                .context("copy: find hole in source")?
                .min(size);
            hash_zeros(&mut hasher, data - offset);
            offset = data;
            while offset < hole {
                self.service();
                src.check_lease()?;
                let len = (hole - offset).min(COPY_BUF as u64) as usize;
                let n = fs
                    .read_at(src.file.as_fd(), &mut buf[..len], offset)
                    .context("copy: read source")?;
                if n == 0 {
                    // Truncated, which only a writer can do.
                    return Err(Abort::Skip(Skip::Changed));
                }
                let mut written = 0;
                while written < n {
                    match fs
                        .write_at(dst.as_fd(), &buf[written..n], offset + written as u64)
                        .context("copy: write temp file")?
                    {
                        0 => return Err(anyhow!("copy: write temp file: wrote nothing").into()),
                        w => written += w,
                    }
                }
                hasher.update(&buf[..n]);
                offset += n as u64;
            }
        }
        hash_zeros(&mut hasher, size - offset);
        // Gives the copy the source's size, even if it ends in a hole.
        fs.set_len(dst.as_fd(), size)
            .context("copy: set size of temp file")?;
        Ok(Hash(hasher.digest128()))
    }

    /// Reads the fsynced copy in the temp file `temp` back from storage, and
    /// fails the move unless its hash is `hash`.
    fn read_back(&mut self, src: &Source, temp: &OwnedFd, hash: Hash) -> Result<(), Abort> {
        let fs = self.fs;
        fs.drop_cache(temp.as_fd())
            .context("read back: drop cached pages of temp file")?;
        let mut hasher = Xxh3::new();
        let mut buf = vec![0; COPY_BUF];
        let mut offset = 0;
        loop {
            self.service();
            src.check_lease()?;
            let n = fs
                .read_at(temp.as_fd(), &mut buf, offset)
                .context("read back: read temp file")?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            offset += n as u64;
        }
        let read = Hash(hasher.digest128());
        if read != hash {
            return Err(Abort::Fail(anyhow!(
                "read back: the copy's hash {read} differs from the source's {hash}"
            )));
        }
        Ok(())
    }
}

/// Feeds `len` zero bytes to `hasher`.
fn hash_zeros(hasher: &mut Xxh3, mut len: u64) {
    static ZEROS: [u8; 64 * 1024] = [0; 64 * 1024];
    while len > 0 {
        let n = len.min(ZEROS.len() as u64) as usize;
        hasher.update(&ZEROS[..n]);
        len -= n as u64;
    }
}

fn outcome(result: Result<Hash, Abort>) -> Result<MoveOutcome> {
    match result {
        Ok(hash) => Ok(MoveOutcome::Moved(hash)),
        Err(Abort::Skip(skip)) => Ok(MoveOutcome::Skipped(skip)),
        Err(Abort::Fail(e)) => Err(e),
    }
}

/// A published copy, identified so that a rollback removes it only while it
/// is still cachy's own.
struct Published<'a> {
    /// The names it is published under: one, or each name of a hardlink
    /// group, in the order of the source's names.
    names: Vec<PublishedName<'a>>,
    /// The copy as it was right after publishing.
    identity: Identity,
}

struct PublishedName<'a> {
    dir: OwnedFd,
    name: &'a Path,
    /// The name was already on the backing branch, not created by this move.
    adopted: bool,
}

impl<'a> Published<'a> {
    fn one(dir: OwnedFd, name: &'a Path, identity: Identity, adopted: bool) -> Published<'a> {
        Published {
            names: vec![PublishedName { dir, name, adopted }],
            identity,
        }
    }

    /// Removes the names that this move created, unless the copy is no
    /// longer cachy's own under them. All are checked before any is removed,
    /// since removing one changes the copy's ctime.
    fn remove(&self, fs: &dyn FsOps) -> Result<()> {
        let mut ours = Vec::new();
        for published in self.names.iter().filter(|n| !n.adopted) {
            match fs.stat_at(published.dir.as_fd(), published.name) {
                Ok(stat) if Identity::of(&stat) == self.identity => ours.push(published),
                // Replaced, changed or removed since: no longer cachy's own.
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("stat published copy"),
            }
        }
        for published in ours {
            fs.unlink(published.dir.as_fd(), published.name)
                .context("remove published copy")?;
            fs.fsync(published.dir.as_fd())
                .context("fsync destination directory")?;
        }
        Ok(())
    }
}

/// Makes the staged temp file `copy` visible as `<dest_parent>/<name>`,
/// never replacing an existing file. An existing file identical to the copy
/// is adopted as the published copy instead.
fn publish<'a>(
    fs: &dyn FsOps,
    dest_parent: OwnedFd,
    name: &'a Path,
    staged_parent: &OwnedFd,
    copy: &OwnedFd,
) -> Result<Published<'a>> {
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
        Ok(identity) => Ok(Published::one(dest_parent, name, identity, false)),
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
    if let Ok(copy) = fs.fstat(copy.as_fd()) {
        unpublish_inode(fs, dest_parent, name, &Identity::of(&copy));
    }
}

/// Removes `<dest_parent>/<name>` if it is still the inode of `copy`.
fn unpublish_inode(fs: &dyn FsOps, dest_parent: &OwnedFd, name: &Path, copy: &Identity) {
    let ours = fs
        .stat_at(dest_parent.as_fd(), name)
        .is_ok_and(|at_name| copy.same_inode(&at_name));
    if ours && fs.unlink(dest_parent.as_fd(), name).is_ok() {
        let _ = fs.fsync(dest_parent.as_fd());
    }
}

/// Makes the staged symlink `<staged_parent>/<name>` visible as
/// `<dest_parent>/<name>`, never replacing an existing file. An existing
/// identical symlink is adopted as the published copy instead.
fn publish_symlink<'a>(
    fs: &dyn FsOps,
    dest_parent: OwnedFd,
    name: &'a Path,
    staged_parent: &OwnedFd,
) -> Result<Published<'a>> {
    let staged = fs
        .stat_at(staged_parent.as_fd(), name)
        .context("stat temp file")?;
    match fs.rename_noreplace(staged_parent.as_fd(), name, dest_parent.as_fd(), name) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let identical = identical_symlinks(fs, staged_parent, name, &dest_parent, name);
            if !identical.unwrap_or(false) {
                return Err(e).context("publish");
            }
            let identity = Identity::of(
                &fs.stat_at(dest_parent.as_fd(), name)
                    .context("stat existing symlink")?,
            );
            fs.unlink(staged_parent.as_fd(), name)
                .context("delete temp file")?;
            fs.fsync(staged_parent.as_fd())
                .context("fsync staging directory on the backing branch")?;
            return Ok(Published::one(dest_parent, name, identity, true));
        }
        Err(e) => return Err(e).context("publish"),
    }
    // A symlink cannot be fsynced; fsyncing its directory makes it durable.
    // Its owner and timestamps may not be: on ZFS, the directory's fsync
    // need not commit the symlink's own changes (see ADR 0002).
    let identity = fs
        .fsync(dest_parent.as_fd())
        .context("fsync destination directory")
        .and_then(|()| {
            fs.fsync(staged_parent.as_fd())
                .context("fsync staging directory on the backing branch")
        })
        .and_then(|()| {
            Ok(Identity::of(
                &fs.stat_at(dest_parent.as_fd(), name)
                    .context("stat published copy")?,
            ))
        });
    match identity {
        Ok(identity) if identity.same_inode(&staged) => {
            Ok(Published::one(dest_parent, name, identity, false))
        }
        Ok(_) => Err(anyhow!("publish: the published copy was replaced")),
        Err(e) => {
            unpublish_inode(fs, &dest_parent, name, &Identity::of(&staged));
            Err(e)
        }
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
    if !identical(fs, &staged, &existing)? {
        return Ok(None);
    }
    let identity = Identity::of(&fs.stat_at(dest_parent.as_fd(), name)?);
    if identity != before {
        return Ok(None);
    }
    fs.unlink(staged_parent.as_fd(), name)?;
    fs.fsync(staged_parent.as_fd())?;
    Ok(Some(Published::one(dest_parent, name, identity, true)))
}

/// Starts removing the source from the cache branch: renames each of its
/// names into the cache branch's staging directory, where they wait for
/// [`RETIRE_WAIT`] so an open that looked up the source before the rename
/// reaches the lease check in time to roll the move back.
fn retire<'a>(
    fs: &dyn FsOps,
    source: Source<'a>,
    published: Published<'a>,
    hash: Hash,
) -> Result<Retiring<'a>, Abort> {
    let mut retiring = Retiring {
        source,
        retired_parents: Vec::new(),
        published,
        hash,
        retired_at: Instant::now(),
    };
    // From here on, every way out either unlinks the retired names or puts
    // them back, so no opener blocked on the lease is left with a hidden file.
    let retired = (|| {
        for (rel, dir) in retiring.source.rels.iter().zip(&retiring.source.dirs) {
            let (parent, name) = split(rel);
            let (_, retired_parent) = staging_dirs(fs, retiring.source.cache_branch, parent)
                .context("create staging directory on the cache branch")?;
            fs.rename_noreplace(dir.as_fd(), name, retired_parent.as_fd(), name)
                .context("retire: rename source into staging directory")?;
            retiring.retired_parents.push(retired_parent);
        }
        for dir in &retiring.source.dirs {
            fs.fsync(dir.as_fd()).context("fsync source directory")?;
        }
        Ok(())
    })();
    match retired {
        Ok(()) => {
            retiring.retired_at = Instant::now();
            Ok(retiring)
        }
        Err(e) => {
            // A published copy that cannot be removed stays as a stale copy,
            // identical to the source.
            retiring.roll_back(fs)?;
            Err(Abort::Fail(e))
        }
    }
}

impl Retiring<'_> {
    /// The retired names, each with the mirror of its parent that holds it.
    fn retired(&self) -> impl Iterator<Item = (&Path, &OwnedFd)> {
        self.source
            .rels
            .iter()
            .map(|rel| split(rel).1)
            .zip(&self.retired_parents)
    }

    /// Unlinks every retired name. One that cannot be unlinked stays in the
    /// staging directory, for recovery to delete.
    fn unlink(self, fs: &dyn FsOps) -> Result<(), Abort> {
        let mut unlinked = Ok(());
        for (name, retired_parent) in self.retired() {
            let result = fs
                .unlink(retired_parent.as_fd(), name)
                .context("retire: unlink source")
                .and_then(|()| {
                    fs.fsync(retired_parent.as_fd())
                        .context("fsync staging directory on the cache branch")
                });
            if unlinked.is_ok() {
                unlinked = result;
            }
        }
        self.remove_empty_staging_dirs(fs);
        Ok(unlinked?)
    }

    /// Undoes the move: renames the retired names back and removes the
    /// published copy. The source's lease is still held, so whoever broke it
    /// opens the source only once it is back.
    fn roll_back(self, fs: &dyn FsOps) -> Result<(), Abort> {
        let rolled_back = self
            .retired()
            .zip(&self.source.dirs)
            .try_for_each(|((name, retired_parent), dir)| {
                fs.rename_noreplace(retired_parent.as_fd(), name, dir.as_fd(), name)
                    .context("roll back: rename source back")?;
                fs.fsync(dir.as_fd()).context("fsync source directory")?;
                fs.fsync(retired_parent.as_fd())
                    .context("fsync staging directory on the cache branch")
            })
            .and_then(|()| self.published.remove(fs).context("roll back"));
        self.remove_empty_staging_dirs(fs);
        Ok(rolled_back?)
    }

    fn remove_empty_staging_dirs(&self, fs: &dyn FsOps) {
        for rel in self.source.rels {
            remove_empty_retired_dirs(fs, self.source.cache_branch, rel);
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
    /// Its names: one, or each name of a hardlink group.
    rels: &'a [PathBuf],
    /// The parent directory of each name, as opened when the move started.
    dirs: Vec<OwnedFd>,
    /// The source, opened through its first name.
    file: OwnedFd,
    lease: Lease,
    /// The source as it was when the lease was taken.
    identity: Identity,
    /// Its metadata then, before the copy read it and changed its atime.
    stat: Stat,
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

    /// Whether `stat` is of the same inode, changed or not.
    fn same_inode(&self, stat: &Stat) -> bool {
        (self.dev, self.ino) == (stat.st_dev, stat.st_ino)
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
    /// Opens the regular file or hardlink group `rels` on the cache branch
    /// and takes a lease on it. Skips it if another open conflicts with the
    /// lease, if it is younger than the minimum age, or if `rels` are no
    /// longer all of its names.
    fn open(
        fs: &'a dyn FsOps,
        cache_branch: &'a Branch,
        rels: &'a [PathBuf],
        guard: &Guard,
    ) -> Result<Source<'a>, Abort> {
        let (parent, name) = split(&rels[0]);
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
        // A link added or removed since the scan.
        if stat.st_nlink != rels.len() as u64 {
            return Err(Abort::Skip(Skip::Changed));
        }
        let mut dirs = vec![dir];
        for rel in &rels[1..] {
            let (parent, name) = split(rel);
            let dir = open_dirs(fs, &cache_branch.fd, parent).context("open source directory")?;
            match fs.stat_at(dir.as_fd(), name) {
                Ok(at_name) if identity.same_inode(&at_name) => dirs.push(dir),
                Ok(_) => return Err(Abort::Skip(Skip::Changed)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(Abort::Skip(Skip::Changed));
                }
                Err(e) => return Err(anyhow::Error::from(e).context("stat source").into()),
            }
        }
        Ok(Source {
            fs,
            cache_branch,
            rels,
            dirs,
            file,
            lease,
            identity,
            allocated: stat.st_blocks.max(0) as u64 * 512,
            stat,
        })
    }

    /// Checks, right before publishing, that the copy is still current: the
    /// lease is held, the source is unchanged, and each of its relpaths,
    /// looked up afresh from the cache branch root, still names it. An atomic
    /// save or a renamed directory during the copy fails the last check.
    fn revalidate(&self) -> Result<(), Abort> {
        self.check_lease()?;
        let now = self.fs.fstat(self.file.as_fd()).context("fstat source")?;
        if Identity::of(&now) != self.identity {
            return Err(Abort::Skip(Skip::Changed));
        }
        for rel in self.rels {
            still_at_relpath(self.fs, self.cache_branch, rel, &self.identity)?;
        }
        Ok(())
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

/// Skips the move as changed unless `rel`, looked up afresh from the cache
/// branch root, still names the source as `identity` describes it.
fn still_at_relpath(
    fs: &dyn FsOps,
    cache_branch: &Branch,
    rel: &Path,
    identity: &Identity,
) -> Result<(), Abort> {
    let (parent, name) = split(rel);
    let at_path =
        open_dirs(fs, &cache_branch.fd, parent).and_then(|dir| fs.stat_at(dir.as_fd(), name));
    match at_path {
        Ok(stat) if Identity::of(&stat) == *identity => Ok(()),
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

/// Removes the mirror of `rel`'s parent in the cache branch's staging
/// directory, and its ancestors, while they are empty.
fn remove_empty_retired_dirs(fs: &dyn FsOps, cache_branch: &Branch, rel: &Path) {
    if let Ok(staging) = fs.open_dir(cache_branch.fd.as_fd(), Path::new(STAGING)) {
        remove_empty_dirs(fs, &staging, split(rel).0);
    }
}

/// The source of a symlink's move.
struct Link<'a> {
    fs: &'a dyn FsOps,
    cache_branch: &'a Branch,
    rel: &'a Path,
    /// The parent directory, as opened when the move started.
    dir: OwnedFd,
    target: OsString,
    /// The symlink as it was when the move started.
    identity: Identity,
    stat: Stat,
}

impl<'a> Link<'a> {
    /// Reads the symlink `rel` on the cache branch. Skips it if it is younger
    /// than the minimum age.
    fn open(
        fs: &'a dyn FsOps,
        cache_branch: &'a Branch,
        rel: &'a Path,
        guard: &Guard,
    ) -> Result<Link<'a>, Abort> {
        let (parent, name) = split(rel);
        let dir = open_dirs(fs, &cache_branch.fd, parent).context("open source directory")?;
        let stat = fs.stat_at(dir.as_fd(), name).context("stat source")?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFLNK {
            // Replaced since the scan.
            return Err(Abort::Skip(Skip::Changed));
        }
        let identity = Identity::of(&stat);
        if identity.is_younger_than(guard.minimum_age) {
            return Err(Abort::Skip(Skip::TooYoung));
        }
        let target = fs.read_link(dir.as_fd(), name).context("read source")?;
        // A symlink's link text never changes, but it may have been replaced.
        if Identity::of(&fs.stat_at(dir.as_fd(), name).context("stat source")?) != identity {
            return Err(Abort::Skip(Skip::Changed));
        }
        Ok(Link {
            fs,
            cache_branch,
            rel,
            dir,
            target,
            identity,
            stat,
        })
    }

    /// Checks, right before publishing, that the copy is still current: the
    /// relpath, looked up afresh from the cache branch root, still names the
    /// symlink, unchanged.
    fn revalidate(&self) -> Result<(), Abort> {
        still_at_relpath(self.fs, self.cache_branch, self.rel, &self.identity)
    }

    /// Removes the symlink from the cache branch once `published` is: renames
    /// it into the cache branch's staging directory and unlinks it there if
    /// it is still the source. Otherwise, or if the rename fails, the move is
    /// undone and the symlink stays where it was.
    fn retire(self, published: Published) -> Result<(), Abort> {
        let fs = self.fs;
        let (parent, name) = split(self.rel);
        let renamed = staging_dirs(fs, self.cache_branch, parent)
            .context("create staging directory on the cache branch")
            .and_then(|(_, retired_parent)| {
                fs.rename_noreplace(self.dir.as_fd(), name, retired_parent.as_fd(), name)
                    .context("retire: rename source into staging directory")?;
                Ok(retired_parent)
            });
        let retired_parent = match renamed {
            Ok(retired_parent) => retired_parent,
            Err(e) => {
                let _ = published.remove(fs);
                remove_empty_retired_dirs(fs, self.cache_branch, self.rel);
                return Err(Abort::Fail(e));
            }
        };
        let retired = fs
            .stat_at(retired_parent.as_fd(), name)
            .context("retire: stat retired source");
        // The rename changed its ctime, so the retired symlink's link text is
        // checked too: a symlink that replaced the source between the
        // revalidation and the rename may reuse its inode. The published copy
        // is not read, which would change its atime.
        let is_source = retired.and_then(|stat| {
            if !self.identity.same_inode(&stat) || !metadata::same(&stat, &self.stat) {
                return Ok(false);
            }
            let target = fs
                .read_link(retired_parent.as_fd(), name)
                .context("retire: read retired source")?;
            Ok(target == self.target)
        });
        let unlinked = match is_source {
            Ok(true) => fs
                .unlink(retired_parent.as_fd(), name)
                .context("retire: unlink source")
                .map_err(Abort::from),
            Ok(false) => Err(Abort::Skip(Skip::Changed)),
            Err(e) => Err(Abort::Fail(e)),
        };
        if let Err(abort) = unlinked {
            let rolled_back = fs
                .rename_noreplace(retired_parent.as_fd(), name, self.dir.as_fd(), name)
                .context("roll back: rename source back")
                .and_then(|()| fs.fsync(self.dir.as_fd()).context("fsync source directory"))
                .and_then(|()| {
                    fs.fsync(retired_parent.as_fd())
                        .context("fsync staging directory on the cache branch")
                })
                .and_then(|()| published.remove(fs).context("roll back"));
            remove_empty_retired_dirs(fs, self.cache_branch, self.rel);
            rolled_back?;
            return Err(abort);
        }
        let synced = fs
            .fsync(retired_parent.as_fd())
            .context("fsync staging directory on the cache branch")
            .and_then(|()| fs.fsync(self.dir.as_fd()).context("fsync source directory"));
        remove_empty_retired_dirs(fs, self.cache_branch, self.rel);
        Ok(synced?)
    }
}

/// Whether `<a_dir>/<a>` and `<b_dir>/<b>` are symlinks with the same link
/// text, owner, group and mtime: either can stand in for the other.
pub fn identical_symlinks(
    fs: &dyn FsOps,
    a_dir: &OwnedFd,
    a: &Path,
    b_dir: &OwnedFd,
    b: &Path,
) -> io::Result<bool> {
    let (a_stat, b_stat) = (fs.stat_at(a_dir.as_fd(), a)?, fs.stat_at(b_dir.as_fd(), b)?);
    let symlink = |stat: &Stat| stat.st_mode & libc::S_IFMT == libc::S_IFLNK;
    if !symlink(&a_stat) || !symlink(&b_stat) || !metadata::same(&a_stat, &b_stat) {
        return Ok(false);
    }
    Ok(fs.read_link(a_dir.as_fd(), a)? == fs.read_link(b_dir.as_fd(), b)?)
}

/// Whether the open files `a` and `b`, read from their start, are regular
/// files with the same content, owner, group, mode and mtime: either can
/// stand in for the other without losing anything a move preserves (xattrs
/// and ACLs aside).
pub fn identical(fs: &dyn FsOps, a: &OwnedFd, b: &OwnedFd) -> io::Result<bool> {
    let (a_stat, b_stat) = (fs.fstat(a.as_fd())?, fs.fstat(b.as_fd())?);
    let regular = |stat: &Stat| stat.st_mode & libc::S_IFMT == libc::S_IFREG;
    if !regular(&a_stat)
        || !regular(&b_stat)
        || a_stat.st_size != b_stat.st_size
        || !metadata::same(&a_stat, &b_stat)
    {
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

/// Opens `path` below `branch`, creating missing directories. Each one it
/// creates gets the owner, mode, xattrs and ACLs of the directory at the same
/// relpath below `template`, or mode [`DIR_MODE`] if there is none there, and
/// its relpath is added to `created`. Existing directories are left as they
/// are.
pub fn make_dirs_like(
    fs: &dyn FsOps,
    branch: &Branch,
    path: &Path,
    template: &OwnedFd,
    allowed_losses: AllowedLosses,
    created: &mut Vec<PathBuf>,
) -> Result<OwnedFd> {
    let mut dir = fs.open_dir(branch.fd.as_fd(), Path::new("."))?;
    let mut template = Some(fs.open_dir(template.as_fd(), Path::new("."))?);
    let mut rel = PathBuf::new();
    for component in normal_components(path) {
        rel.push(component);
        template = match template.map(|t| fs.open_dir(t.as_fd(), component)) {
            Some(Ok(t)) => Some(t),
            Some(Err(e)) if e.kind() != io::ErrorKind::NotFound => {
                return Err(e).context("open template directory");
            }
            _ => None,
        };
        match fs.open_dir(dir.as_fd(), component) {
            Ok(child) => {
                dir = child;
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let new = match &template {
            Some(template) => {
                create_dir_like(fs, branch, &dir, component, template, allowed_losses, &rel)?
            }
            None => match fs.mkdir(dir.as_fd(), component, DIR_MODE) {
                Ok(()) => {
                    fs.fsync(dir.as_fd())?;
                    true
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
                Err(e) => return Err(e.into()),
            },
        };
        if new {
            created.push(rel.clone());
        }
        dir = fs.open_dir(dir.as_fd(), component)?;
    }
    Ok(dir)
}

/// Creates the directory `<parent>/<name>` on `branch` with the metadata of
/// `template`, except its times. It is built in the staging directory and
/// renamed into place, so that it never shows without its metadata, not
/// even after a crash: recovery removes an empty directory left in the
/// staging directory. Returns false if `<parent>/<name>` appeared meanwhile.
///
/// The directories are built in `<staging>/.cachy-staging`, which mirrors no
/// relpath, since the scanner never descends into the cache branch's staging
/// directory.
fn create_dir_like(
    fs: &dyn FsOps,
    branch: &Branch,
    parent: &OwnedFd,
    name: &Path,
    template: &OwnedFd,
    allowed_losses: AllowedLosses,
    rel: &Path,
) -> Result<bool> {
    let new_dirs_path = Path::new(STAGING).join(STAGING);
    let new_dirs = make_dirs(fs, &branch.fd, &new_dirs_path, STAGING_MODE)
        .context("create staging directory")?;
    match fs.mkdir(new_dirs.as_fd(), name, STAGING_MODE) {
        Ok(()) => {}
        // Left by a crash and not recovered.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            fs.rmdir(new_dirs.as_fd(), name)
                .context("remove leftover directory")?;
            fs.mkdir(new_dirs.as_fd(), name, STAGING_MODE)
                .context("create directory")?;
        }
        Err(e) => return Err(e).context("create directory"),
    }
    let created = (|| {
        let new = fs.open_dir(new_dirs.as_fd(), name)?;
        let stat = fs
            .fstat(template.as_fd())
            .context("stat template directory")?;
        metadata::copy_attributes(
            fs,
            &stat,
            template.as_fd(),
            new.as_fd(),
            allowed_losses,
            &branch.path.join(rel),
        )
        .context("metadata")?;
        fs.fsync(new.as_fd()).context("fsync new directory")?;
        match fs.rename_noreplace(new_dirs.as_fd(), name, parent.as_fd(), name) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(e).context("rename new directory into place"),
        }
        fs.fsync(parent.as_fd()).context("fsync parent directory")?;
        fs.fsync(new_dirs.as_fd())
            .context("fsync staging directory")?;
        Ok(true)
    })();
    // Fails harmlessly once the new directory is in place.
    let _ = fs.rmdir(new_dirs.as_fd(), name);
    let _ = fs.rmdir(branch.fd.as_fd(), &new_dirs_path);
    created
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
