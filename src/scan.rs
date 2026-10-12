//! The scanner: walks the cache branch on one filesystem and lists its regular
//! files, hardlink groups, symlinks and special files, oldest mtime first.
//! Symlinks are never followed.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::exclude::Exclusions;
use crate::transaction::STAGING;

/// What the walk found: a move unit, or files that a run skips.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    /// Paths relative to the cache branch root, sorted: one, or each name of
    /// a hardlink group that the walk found.
    pub rels: Vec<PathBuf>,
    pub kind: Kind,
    mtime: (i64, i64),
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

/// Regular files, hardlink groups, symlinks and special files under `root`
/// that `exclusions` leave, ordered by mtime, then relpath. The walk prunes
/// excluded directories, the staging directory and directories on other
/// filesystems. Directories that cannot be read are skipped with a warning.
pub fn scan(root: &Path, exclusions: &Exclusions) -> std::io::Result<Vec<Entry>> {
    let dev = fs::metadata(root)?.dev();
    let mut found = Vec::new();
    let walk = Walk {
        root,
        dev,
        exclusions,
    };
    walk.dir(Path::new(""), &mut found);
    Ok(group(found))
}

/// A file that the walk found.
struct Found {
    rel: PathBuf,
    kind: Kind,
    mtime: (i64, i64),
    /// `st_dev` and `st_ino`, which identify its inode.
    dev: u64,
    ino: u64,
    /// Its link count.
    nlink: u64,
}

/// Turns what the walk found into entries, ordered by mtime, then relpath.
/// Regular files with more than one link that share `(st_dev, st_ino)` form
/// one hardlink group; it is partly excluded if the walk found fewer names
/// than its link count.
fn group(found: Vec<Found>) -> Vec<Entry> {
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
                        mtime: f.mtime,
                    },
                    f.nlink,
                )
            });
            group.rels.push(f.rel);
        } else {
            entries.push(Entry {
                rels: vec![f.rel],
                kind: f.kind,
                mtime: f.mtime,
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
    entries.sort_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.rels.cmp(&b.rels)));
    entries
}

struct Walk<'a> {
    root: &'a Path,
    dev: u64,
    exclusions: &'a Exclusions,
}

impl Walk<'_> {
    fn dir(&self, rel_dir: &Path, found: &mut Vec<Found>) {
        let root = self.root;
        let entries = match fs::read_dir(root.join(rel_dir)) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!(
                    "cachy: warning: skipping {}: {e}",
                    root.join(rel_dir).display()
                );
                return;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    eprintln!(
                        "cachy: warning: skipping entry in {}: {e}",
                        root.join(rel_dir).display()
                    );
                    continue;
                }
            };
            let rel = rel_dir.join(entry.file_name());
            if rel.as_os_str() == STAGING {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) => {
                    eprintln!(
                        "cachy: warning: skipping {}: {e}",
                        root.join(&rel).display()
                    );
                    continue;
                }
            };
            if self.exclusions.excludes(&rel, meta.is_dir()) {
                continue;
            }
            if meta.is_dir() && meta.dev() == self.dev {
                self.dir(&rel, found);
            } else if let Some(kind) = kind(meta.file_type()) {
                found.push(Found {
                    rel,
                    kind,
                    mtime: (meta.mtime(), meta.mtime_nsec()),
                    dev: meta.dev(),
                    ino: meta.ino(),
                    nlink: meta.nlink(),
                });
            }
        }
    }
}

fn kind(file_type: fs::FileType) -> Option<Kind> {
    if file_type.is_file() {
        Some(Kind::Regular)
    } else if file_type.is_symlink() {
        Some(Kind::Symlink)
    } else if file_type.is_fifo()
        || file_type.is_socket()
        || file_type.is_block_device()
        || file_type.is_char_device()
    {
        Some(Kind::Special)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regular(rel: &str, dev: u64, ino: u64, nlink: u64) -> Found {
        Found {
            rel: PathBuf::from(rel),
            kind: Kind::Regular,
            mtime: (1_000, 0),
            dev,
            ino,
            nlink,
        }
    }

    fn entry(rels: &[&str], kind: Kind) -> Entry {
        Entry {
            rels: rels.iter().map(PathBuf::from).collect(),
            kind,
            mtime: (1_000, 0),
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
            group(found),
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
            group(found),
            [
                entry(&["a", "b"], Kind::Regular),
                entry(&["single"], Kind::Regular),
            ]
        );
    }
}
