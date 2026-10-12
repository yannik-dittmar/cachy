//! The scanner: walks the cache branch on one filesystem and lists its regular
//! files, oldest mtime first.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::transaction::STAGING;

pub struct MoveUnit {
    /// Path relative to the cache branch root.
    pub rel: PathBuf,
    mtime: (i64, i64),
}

/// Regular files under `root`, ordered by mtime, then relpath. Directories
/// that cannot be read are skipped with a warning.
pub fn scan(root: &Path) -> std::io::Result<Vec<MoveUnit>> {
    let dev = fs::metadata(root)?.dev();
    let mut units = Vec::new();
    walk(root, Path::new(""), dev, &mut units);
    units.sort_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.rel.cmp(&b.rel)));
    Ok(units)
}

fn walk(root: &Path, rel_dir: &Path, dev: u64, units: &mut Vec<MoveUnit>) {
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
        if meta.is_dir() && meta.dev() == dev {
            walk(root, &rel, dev, units);
        } else if meta.is_file() {
            units.push(MoveUnit {
                rel,
                mtime: (meta.mtime(), meta.mtime_nsec()),
            });
        }
    }
}
