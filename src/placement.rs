//! Placement: chooses the backing branch that a move unit moves to.
//!
//! A branch is eligible if it takes new files (it is neither `RO` nor `NC`)
//! and, after taking the whole move unit, still has its `minfreespace` free.
//! Among the eligible branches, those that already have the move unit's
//! parent directory win (for a hardlink group, the parent of the name it is
//! published under first), and among those, the one with the most free
//! space; ties go to the branch listed first. Directories that this run
//! placed move units in count as existing, so a directory that a run creates
//! keeps its files together.
//!
//! A branch that already holds a file at one of the move unit's relpaths, a
//! stale copy, wins if it is eligible, since the move replaces the stale copy
//! in place; among several, the usual rules choose. The move removes the
//! stale copies on the other branches. cachy never writes to a branch that
//! takes no new files, so a move unit with a stale copy on one is not placed
//! at all: the cache pool would show the stale copy once the cache branch's
//! copy is gone.
//!
//! Free space and directories are looked up outside the
//! filesystem-operations port, like every check that changes nothing. A
//! branch's free space is the least of what it reports now and what it
//! reported when the run started less the move units placed on it since:
//! moves in flight have not used up their space yet, and on ZFS, written
//! data shows in free space only once its transaction group syncs. A move
//! unit that does not move gives its space back.

use std::collections::HashMap;
use std::fmt;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use crate::fsops::FsStat;
use crate::layout::BackingBranch;
use crate::transaction::split;

pub struct Placement<'a> {
    branches: &'a [BackingBranch],
    /// Per branch: its free space when the run started, or `None` if it
    /// could not be measured, which keeps any move unit off it.
    start_free: Vec<Option<u64>>,
    /// Per branch: the bytes of the move units placed on it.
    placed: Vec<u64>,
    /// Whether a branch (by index) has a directory (by relpath), as found or
    /// as a move unit placed in it made it.
    dirs: HashMap<(usize, PathBuf), bool>,
}

/// Why a move unit has no branch to move to.
#[derive(Debug)]
pub enum Unplaced {
    /// No eligible branch.
    NoRoom,
    /// A stale copy at `rel` is on the backing branch at `branch`, which
    /// takes no new files, so it cannot be removed.
    StaleCopy { rel: PathBuf, branch: PathBuf },
}

impl fmt::Display for Unplaced {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Unplaced::NoRoom => f.write_str("no backing branch has room"),
            Unplaced::StaleCopy { rel, branch } => write!(
                f,
                "a stale copy of {} on backing branch {}, which takes no new files, cannot be removed",
                rel.display(),
                branch.display()
            ),
        }
    }
}

impl std::error::Error for Unplaced {}

impl<'a> Placement<'a> {
    pub fn new(branches: &'a [BackingBranch]) -> Placement<'a> {
        Placement {
            branches,
            start_free: branches.iter().map(free).collect(),
            placed: vec![0; branches.len()],
            dirs: HashMap::new(),
        }
    }

    /// The index of the branch that the move unit with the names `rels`,
    /// which needs at most `size` bytes, moves to. The move unit counts as
    /// placed there until [`Placement::release`] gives its space back.
    pub fn place(&mut self, rels: &[PathBuf], size: u64) -> Result<usize, Unplaced> {
        let eligible: Vec<(usize, u64)> = (0..self.branches.len())
            .filter_map(|i| Some((i, self.room(i, size)?)))
            .collect();
        let holders: Vec<(usize, &PathBuf)> = (0..self.branches.len())
            .filter_map(|i| {
                let branch = &self.branches[i].branch;
                rels.iter().find(|rel| branch.has(rel)).map(|rel| (i, rel))
            })
            .collect();
        if let Some(&(no_new_files, rel)) = holders
            .iter()
            .find(|&&(i, _)| !self.branches[i].takes_new_files())
        {
            return Err(Unplaced::StaleCopy {
                rel: rel.clone(),
                branch: self.branches[no_new_files].branch.path.clone(),
            });
        }
        let (eligible_holders, eligible): (Vec<_>, Vec<_>) = eligible
            .into_iter()
            .partition(|&(i, _)| holders.iter().any(|&(holder, _)| holder == i));
        let candidates = if eligible_holders.is_empty() {
            eligible
        } else {
            eligible_holders
        };
        let chosen = self
            .choose(candidates, split(&rels[0]).0)
            .ok_or(Unplaced::NoRoom)?;
        self.placed[chosen] += size;
        for rel in rels {
            self.dirs.insert((chosen, split(rel).0.to_owned()), true);
        }
        Ok(chosen)
    }

    /// Gives back the `size` bytes of a move unit placed on the branch
    /// `index` that did not move.
    pub fn release(&mut self, index: usize, size: u64) {
        self.placed[index] -= size;
    }

    /// The free space of the branch `index` if it is eligible for a move unit
    /// of `size` bytes.
    fn room(&self, index: usize, size: u64) -> Option<u64> {
        let branch = &self.branches[index];
        if !branch.takes_new_files() {
            return None;
        }
        let free = self.start_free[index]?
            .saturating_sub(self.placed[index])
            .min(free(branch)?);
        let left = free.checked_sub(size)?;
        (left >= branch.minfreespace).then_some(free)
    }

    /// Of the `eligible` branches, with their free space, one that has the
    /// directory `parent` if any does, and of those the one with the most
    /// free space.
    fn choose(&mut self, eligible: Vec<(usize, u64)>, parent: &Path) -> Option<usize> {
        let with_parent: Vec<(usize, u64)> = eligible
            .iter()
            .copied()
            .filter(|&(i, _)| self.has_dir(i, parent))
            .collect();
        let candidates = if with_parent.is_empty() {
            eligible
        } else {
            with_parent
        };
        // The most free space; of equals, the first listed.
        let (chosen, _) = candidates.into_iter().rev().max_by_key(|&(_, free)| free)?;
        Some(chosen)
    }

    fn has_dir(&mut self, index: usize, rel: &Path) -> bool {
        let branch = &self.branches[index].branch;
        *self
            .dirs
            .entry((index, rel.to_owned()))
            .or_insert_with(|| branch.has_dir(rel))
    }
}

fn free(branch: &BackingBranch) -> Option<u64> {
    FsStat::of(branch.branch.fd.as_fd())
        .ok()
        .map(|stat| stat.free)
}
