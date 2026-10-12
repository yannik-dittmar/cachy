//! The scanner: walks the cache branch on one filesystem and lists its regular
//! files, symlinks and special files, oldest mtime first. Symlinks are never
//! followed.

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::exclude::Exclusions;
use crate::transaction::STAGING;

/// A file that the walk found: a move unit, or a special file that a run
/// skips.
pub struct Entry {
    /// Path relative to the cache branch root.
    pub rel: PathBuf,
    pub kind: Kind,
    mtime: (i64, i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file of any size, which is a move unit.
    Regular,
    /// A symlink, which is a move unit whatever it points at.
    Symlink,
    /// A FIFO, socket or device node, which is never moved.
    Special,
}

/// Regular files, symlinks and special files under `root` that `exclusions` leave, ordered by
/// mtime, then relpath. The walk prunes excluded directories, the staging
/// directory and directories on other filesystems. Directories that cannot be
/// read are skipped with a warning.
pub fn scan(root: &Path, exclusions: &Exclusions) -> std::io::Result<Vec<Entry>> {
    let dev = fs::metadata(root)?.dev();
    let mut entries = Vec::new();
    let walk = Walk {
        root,
        dev,
        exclusions,
    };
    walk.dir(Path::new(""), &mut entries);
    entries.sort_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.rel.cmp(&b.rel)));
    Ok(entries)
}

struct Walk<'a> {
    root: &'a Path,
    dev: u64,
    exclusions: &'a Exclusions,
}

impl Walk<'_> {
    fn dir(&self, rel_dir: &Path, found: &mut Vec<Entry>) {
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
                found.push(Entry {
                    rel,
                    kind,
                    mtime: (meta.mtime(), meta.mtime_nsec()),
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
