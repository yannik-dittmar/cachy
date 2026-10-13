//! The scanner: walks the cache branch on one filesystem and lists its regular
//! files, hardlink groups, symlinks and special files, in the configured
//! order. Symlinks are never followed.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::FileType;

use crate::config::Order;
use crate::exclude::Exclusions;
use crate::fsops::{FsOps, Stat};
use crate::transaction::{Branch, STAGING, time_since};

/// What the walk found: a move unit, or files that a run skips.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    /// Paths relative to the cache branch root, sorted: one, or each name of
    /// a hardlink group that the walk found.
    pub rels: Vec<PathBuf>,
    pub kind: Kind,
    /// Space it takes up on the cache branch (`st_blocks`), in bytes; a
    /// hardlink group's once.
    pub allocated: u64,
    atime: (i64, u64),
    mtime: (i64, u64),
    /// Its last change: the later of its mtime and ctime.
    changed: (i64, u64),
}

impl Entry {
    /// Whether it is a move unit that has not changed for longer than
    /// `max_age`.
    pub fn is_past_max_age(&self, max_age: Duration) -> bool {
        matches!(self.kind, Kind::Regular | Kind::Symlink)
            && time_since(self.changed).is_some_and(|age| age > max_age)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file of any size, or a hardlink group: the names of one
    /// inode, which move together. Either is a move unit.
    Regular,
    /// A hardlink group whose link count exceeds the names that the walk
    /// found, for example because one is in an excluded directory. It is
    /// never moved.
    PartlyExcluded,
    /// A symlink, which is a move unit whatever it points at.
    Symlink,
    /// A FIFO, socket or device node, which is never moved.
    Special,
}

/// Regular files, hardlink groups, symlinks and special files on the cache
/// branch that `exclusions` leave, oldest first by `order`, then by mtime,
/// then by relpath. The walk prunes excluded directories, the staging
/// directory and directories on other filesystems. Directories that cannot be
/// read are skipped with a warning.
pub fn scan(
    fs: &dyn FsOps,
    cache_branch: &Branch,
    exclusions: &Exclusions,
    order: Order,
) -> io::Result<Vec<Entry>> {
    let dev = fs.fstat(cache_branch.fd.as_fd())?.st_dev;
    let mut found = Vec::new();
    let walk = Walk {
        fs,
        root: &cache_branch.path,
        dev,
        exclusions,
    };
    walk.dir(cache_branch.fd.as_fd(), Path::new(""), &mut found);
    Ok(group(found, order))
}

/// A file that the walk found.
struct Found {
    rel: PathBuf,
    kind: Kind,
    allocated: u64,
    atime: (i64, u64),
    mtime: (i64, u64),
    changed: (i64, u64),
    /// `st_dev` and `st_ino`, which identify its inode.
    dev: u64,
    ino: u64,
    /// Its link count.
    nlink: u64,
}

/// Turns what the walk found into entries, oldest first by `order`, then by
/// mtime, then by relpath.
/// Regular files with more than one link that share `(st_dev, st_ino)` form
/// one hardlink group; it is partly excluded if the walk found fewer names
/// than its link count.
fn group(found: Vec<Found>, order: Order) -> Vec<Entry> {
    let mut entries = Vec::new();
    // Each group, with its link count, by `(st_dev, st_ino)`.
    let mut groups: BTreeMap<(u64, u64), (Entry, u64)> = BTreeMap::new();
    for f in found {
        if f.kind == Kind::Regular && f.nlink > 1 {
            let (group, _) = groups.entry((f.dev, f.ino)).or_insert_with(|| {
                let rels = Vec::new();
                (
                    Entry {
                        rels,
                        kind: Kind::Regular,
                        allocated: f.allocated,
                        atime: f.atime,
                        mtime: f.mtime,
                        changed: f.changed,
                    },
                    f.nlink,
                )
            });
            group.rels.push(f.rel);
        } else {
            entries.push(Entry {
                rels: vec![f.rel],
                kind: f.kind,
                allocated: f.allocated,
                atime: f.atime,
                mtime: f.mtime,
                changed: f.changed,
            });
        }
    }
    for (mut group, nlink) in groups.into_values() {
        group.rels.sort();
        if (group.rels.len() as u64) < nlink {
            group.kind = Kind::PartlyExcluded;
        }
        entries.push(group);
    }
    let key = |e: &Entry| match order {
        Order::Atime => e.atime,
        Order::Mtime => e.mtime,
    };
    entries.sort_by(|a, b| {
        key(a)
            .cmp(&key(b))
            .then_with(|| a.mtime.cmp(&b.mtime))
            .then_with(|| a.rels.cmp(&b.rels))
    });
    entries
}

struct Walk<'a> {
    fs: &'a dyn FsOps,
    root: &'a Path,
    dev: u64,
    exclusions: &'a Exclusions,
}

impl Walk<'_> {
    /// Walks the directory `dir`, at `rel_dir` relative to the root.
    fn dir(&self, dir: BorrowedFd, rel_dir: &Path, found: &mut Vec<Found>) {
        let names = match self.fs.list_dir(dir) {
            Ok(names) => names,
            Err(e) => {
                self.warn(rel_dir, e);
                return;
            }
        };
        for name in names {
            let rel = rel_dir.join(&name);
            if rel.as_os_str() == STAGING {
                continue;
            }
            let stat = match self.fs.stat_at(dir, Path::new(&name)) {
                Ok(stat) => stat,
                Err(e) => {
                    self.warn(&rel, e);
                    continue;
                }
            };
            let file_type = FileType::from_raw_mode(stat.st_mode);
            let is_dir = file_type == FileType::Directory;
            if self.exclusions.excludes(&rel, is_dir) {
                continue;
            }
            if is_dir {
                if stat.st_dev == self.dev {
                    self.subdir(dir, &name, &rel, found);
                }
            } else if let Some(kind) = kind(file_type) {
                found.push(Found::new(rel, kind, &stat));
            }
        }
    }

    /// Walks the subdirectory `name` of `dir`, at `rel`.
    fn subdir(&self, dir: BorrowedFd, name: &OsStr, rel: &Path, found: &mut Vec<Found>) {
        match self.fs.open_dir(dir, Path::new(name)) {
            Ok(subdir) => self.dir(subdir.as_fd(), rel, found),
            Err(e) => self.warn(rel, e),
        }
    }

    /// Warns that the walk skips `rel`.
    fn warn(&self, rel: &Path, e: io::Error) {
        eprintln!(
            "cachy: warning: skipping {}: {e}",
            self.root.join(rel).display()
        );
    }
}

impl Found {
    /// A file at `rel` with the given `stat`, not following symlinks.
    fn new(rel: PathBuf, kind: Kind, stat: &Stat) -> Found {
        let mtime = (stat.st_mtime, stat.st_mtime_nsec);
        let ctime = (stat.st_ctime, stat.st_ctime_nsec);
        Found {
            rel,
            kind,
            allocated: stat.st_blocks as u64 * 512,
            atime: (stat.st_atime, stat.st_atime_nsec),
            mtime,
            changed: mtime.max(ctime),
            dev: stat.st_dev,
            ino: stat.st_ino,
            nlink: stat.st_nlink,
        }
    }
}

fn kind(file_type: FileType) -> Option<Kind> {
    match file_type {
        FileType::RegularFile => Some(Kind::Regular),
        FileType::Symlink => Some(Kind::Symlink),
        FileType::Fifo | FileType::Socket | FileType::BlockDevice | FileType::CharacterDevice => {
            Some(Kind::Special)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regular(rel: &str, dev: u64, ino: u64, nlink: u64) -> Found {
        Found {
            rel: PathBuf::from(rel),
            kind: Kind::Regular,
            allocated: 0,
            atime: (1_000, 0),
            mtime: (1_000, 0),
            changed: (1_000, 0),
            dev,
            ino,
            nlink,
        }
    }

    fn entry(rels: &[&str], kind: Kind) -> Entry {
        Entry {
            rels: rels.iter().map(PathBuf::from).collect(),
            kind,
            allocated: 0,
            atime: (1_000, 0),
            mtime: (1_000, 0),
            changed: (1_000, 0),
        }
    }

    /// The walk never leaves the cache branch's filesystem, but a file
    /// bind-mounted into it is on another device and may share an inode
    /// number with a file of the cache branch; the two are never one group.
    #[test]
    fn files_on_different_devices_with_the_same_inode_number_are_not_grouped() {
        let found = vec![
            regular("cache", 1, 42, 2),
            regular("bind-mounted", 2, 42, 2),
        ];

        assert_eq!(
            group(found, Order::Atime),
            [
                entry(&["bind-mounted"], Kind::PartlyExcluded),
                entry(&["cache"], Kind::PartlyExcluded),
            ]
        );
    }

    #[test]
    fn names_of_one_inode_form_one_group() {
        let found = vec![
            regular("b", 1, 42, 2),
            regular("single", 1, 7, 1),
            regular("a", 1, 42, 2),
        ];

        assert_eq!(
            group(found, Order::Atime),
            [
                entry(&["a", "b"], Kind::Regular),
                entry(&["single"], Kind::Regular),
            ]
        );
    }
}
