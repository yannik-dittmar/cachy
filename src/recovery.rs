//! Recovery: resolves what unfinished moves left in the staging directories,
//! so that a crash at any step of a move loses nothing.
//!
//! Recovery lists only the staging directories, never the rest of a branch,
//! and deletes nothing outside them. Each leftover is resolved against its
//! published counterpart, the file at the same relpath on a backing branch:
//!
//! - A retired source (in the cache branch's staging directory) is deleted if
//!   it is identical to its published counterpart and not in use. Otherwise
//!   it is renamed back to its relpath on the cache branch, which never
//!   replaces a file. A crash releases the lease that kept openers away from
//!   it, so recovery takes a lease of its own before deciding. A retired
//!   name of a hardlink group is resolved on its own: once deleted, the
//!   group's names left on the cache branch move as a smaller group, which
//!   adopts the published copy.
//! - A retired symlink is deleted if its published counterpart is an
//!   identical symlink, and renamed back otherwise. Nothing can have a
//!   symlink open, so it takes no lease.
//! - A temp file (in a backing branch's staging directory) is deleted. A move
//!   retires its source only once the copy is published, so a temp file is
//!   never the only copy of anything. A stale copy that a move swapped out
//!   for its copy waits under the temp file's name, and is deleted too: the
//!   copy replaced it. Recovery never writes to a branch that
//!   takes no new files (`RO` or `NC`), so leftovers there stay, with a
//!   warning, until the branch takes new files again.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use anyhow::{Context, Result};

use crate::fsops::{FsOps, Lease};
use crate::layout::BackingBranch;
use crate::metadata::AllowedLosses;
use crate::transaction::{
    Branch, STAGING, identical, identical_symlinks, make_dirs_like, open_dirs, split,
};

/// Resolves the leftovers in the staging directories of the cache branch and
/// of the backing branches that take new files, then removes the staging
/// directories once empty.
/// A retired source counts as in use if another open conflicts with `lease`.
/// Returns how many leftovers could not be resolved; they stay where they are.
pub fn recover(
    fs: &dyn FsOps,
    cache_branch: &Branch,
    backing_branches: &[BackingBranch],
    lease: Lease,
    allowed_losses: AllowedLosses,
) -> usize {
    let mut recovery = Recovery {
        fs,
        cache_branch,
        backing_branches,
        lease,
        allowed_losses,
        unresolved: 0,
    };
    recovery.resolve_staging_directory(cache_branch, Leftover::RetiredSource);
    for backing in backing_branches {
        if backing.takes_new_files() {
            recovery.resolve_staging_directory(&backing.branch, Leftover::TempFile);
        } else {
            // cachy never writes to it, not even to recover.
            let staging = backing.branch.path.join(STAGING);
            if staging.symlink_metadata().is_ok() {
                eprintln!(
                    "cachy: warning: {} holds leftovers of an unfinished move, which recovery leaves alone while the branch takes no new files",
                    staging.display()
                );
            }
        }
    }
    recovery.unresolved
}

/// What a regular file or symlink in a staging directory is left over from.
#[derive(Clone, Copy)]
enum Leftover {
    /// A source that was renamed into the cache branch's staging directory to
    /// retire it.
    RetiredSource,
    /// A copy that was being written in a backing branch's staging directory.
    TempFile,
}

struct Recovery<'a> {
    fs: &'a dyn FsOps,
    cache_branch: &'a Branch,
    backing_branches: &'a [BackingBranch],
    lease: Lease,
    allowed_losses: AllowedLosses,
    unresolved: usize,
}

impl Recovery<'_> {
    fn resolve_staging_directory(&mut self, branch: &Branch, kind: Leftover) {
        let staging = match self.fs.open_dir(branch.fd.as_fd(), Path::new(STAGING)) {
            Ok(staging) => staging,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(e) => {
                self.unresolved(&branch.path.join(STAGING), anyhow::Error::from(e));
                return;
            }
        };
        self.resolve_directory(branch, &staging, Path::new(""), kind);
        // Fails harmlessly if a leftover could not be resolved.
        let _ = self.fs.rmdir(branch.fd.as_fd(), Path::new(STAGING));
    }

    /// Resolves the leftovers below `rel_dir`, the directory `dir` in
    /// `branch`'s staging directory, and removes the directories they leave
    /// empty.
    fn resolve_directory(
        &mut self,
        branch: &Branch,
        dir: &OwnedFd,
        rel_dir: &Path,
        kind: Leftover,
    ) {
        let staged = |rel: &Path| branch.path.join(STAGING).join(rel);
        let names = match self.fs.list_dir(dir.as_fd()) {
            Ok(names) => names,
            Err(e) => {
                self.unresolved(&staged(rel_dir), anyhow::Error::from(e).context("list"));
                return;
            }
        };
        for name in names {
            let name = Path::new(&name);
            let rel = rel_dir.join(name);
            let stat = match self.fs.stat_at(dir.as_fd(), name) {
                Ok(stat) => stat,
                Err(e) => {
                    self.unresolved(&staged(&rel), anyhow::Error::from(e).context("stat"));
                    continue;
                }
            };
            match stat.st_mode & libc::S_IFMT {
                libc::S_IFDIR => {
                    match self.fs.open_dir(dir.as_fd(), name) {
                        Ok(child) => self.resolve_directory(branch, &child, &rel, kind),
                        Err(e) => {
                            self.unresolved(&staged(&rel), anyhow::Error::from(e).context("open"));
                            continue;
                        }
                    }
                    // Fails harmlessly if a leftover could not be resolved.
                    let _ = self.fs.rmdir(dir.as_fd(), name);
                }
                file_type @ (libc::S_IFREG | libc::S_IFLNK) => {
                    let resolved = match kind {
                        Leftover::RetiredSource if file_type == libc::S_IFLNK => {
                            self.resolve_retired_symlink(dir, &rel)
                        }
                        Leftover::RetiredSource => self.resolve_retired_source(dir, &rel),
                        Leftover::TempFile => self.resolve_temp_file(dir, &rel),
                    };
                    match resolved {
                        Ok(how) => eprintln!("cachy: recovery: {}: {how}", staged(&rel).display()),
                        Err(e) => self.unresolved(&staged(&rel), e),
                    }
                }
                _ => self.unresolved(
                    &staged(&rel),
                    anyhow::anyhow!("not a regular file, symlink or directory"),
                ),
            }
        }
    }

    /// Deletes the retired source `rel` (in `staged_dir`) if its published
    /// counterpart is identical and it is not in use, and renames it back to
    /// the cache branch otherwise.
    fn resolve_retired_source(&self, staged_dir: &OwnedFd, rel: &Path) -> Result<&'static str> {
        let fs = self.fs;
        let (parent, name) = split(rel);
        // Anything short of proof that the retired source is redundant keeps
        // it, which renaming it back always does. The lease is held until the
        // retired source is deleted or back in place.
        let retired = fs.open_read(staged_dir.as_fd(), name);
        let redundant = retired.as_ref().is_ok_and(|retired| {
            fs.set_lease(retired.as_fd(), self.lease).is_ok()
                && self.backing_branches.iter().any(|backing| {
                    open_dirs(fs, &backing.branch.fd, parent)
                        .and_then(|dir| fs.open_read(dir.as_fd(), name))
                        .and_then(|published| identical(fs, retired, &published))
                        .unwrap_or(false)
                })
                // A conflicting open since the lease was taken.
                && fs.lease(retired.as_fd()).is_ok_and(|lease| lease == self.lease)
        });
        self.delete_or_rename_back(staged_dir, rel, redundant)
    }

    /// Deletes the retired symlink `rel` (in `staged_dir`) if its published
    /// counterpart is an identical symlink, and renames it back to the cache
    /// branch otherwise.
    fn resolve_retired_symlink(&self, staged_dir: &OwnedFd, rel: &Path) -> Result<&'static str> {
        let (parent, name) = split(rel);
        let redundant = self.backing_branches.iter().any(|backing| {
            open_dirs(self.fs, &backing.branch.fd, parent)
                .and_then(|dir| identical_symlinks(self.fs, staged_dir, name, &dir, name))
                .unwrap_or(false)
        });
        self.delete_or_rename_back(staged_dir, rel, redundant)
    }

    /// Deletes the retired source `rel` (in `staged_dir`) if it is
    /// `redundant`, and renames it back to the cache branch otherwise.
    fn delete_or_rename_back(
        &self,
        staged_dir: &OwnedFd,
        rel: &Path,
        redundant: bool,
    ) -> Result<&'static str> {
        let fs = self.fs;
        let (parent, name) = split(rel);
        if redundant {
            fs.unlink(staged_dir.as_fd(), name)
                .context("delete retired source")?;
            fs.fsync(staged_dir.as_fd())
                .context("fsync staging directory")?;
            return Ok("deleted, identical to its published copy");
        }
        // A directory recreated on the cache branch takes the metadata of its
        // counterpart on a backing branch, if there is one: on the first
        // backing branch that has the parent directory. Looked up outside the
        // port, like placement.
        let template = self
            .backing_branches
            .iter()
            .find(|backing| backing.branch.has_dir(parent))
            .unwrap_or(&self.backing_branches[0]);
        let dest = make_dirs_like(
            fs,
            self.cache_branch,
            parent,
            &template.branch.fd,
            self.allowed_losses,
            &mut Vec::new(),
        )
        .context("create directory on the cache branch")?;
        fs.rename_noreplace(staged_dir.as_fd(), name, dest.as_fd(), name)
            .context("rename retired source back")?;
        fs.fsync(dest.as_fd())
            .context("fsync directory on the cache branch")?;
        fs.fsync(staged_dir.as_fd())
            .context("fsync staging directory")?;
        Ok("renamed back to the cache branch")
    }

    fn resolve_temp_file(&self, staged_dir: &OwnedFd, rel: &Path) -> Result<&'static str> {
        let name = split(rel).1;
        self.fs
            .unlink(staged_dir.as_fd(), name)
            .context("delete temp file")?;
        self.fs
            .fsync(staged_dir.as_fd())
            .context("fsync staging directory")?;
        Ok("deleted temp file")
    }

    fn unresolved(&mut self, path: &Path, e: anyhow::Error) {
        self.unresolved += 1;
        eprintln!(
            "cachy: error: cannot resolve staging leftover {}: {e:#}",
            path.display()
        );
    }
}
