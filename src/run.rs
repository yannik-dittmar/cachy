//! The run orchestrator: performs the check and, if it finds work, moves files
//! until the run's byte budget is consumed.

use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{Config, LeaseType, Order};
use crate::fsops::{FsOps, FsStat, Lease};
use crate::metadata::AllowedLosses;
use crate::recovery::recover;
use crate::scan::{Entry, scan};
use crate::transaction::{Branch, Guard, MoveOutcome, Mover, STAGING, would_skip};

pub struct Outcome {
    /// Number of moves that were attempted and failed; in a dry run, the
    /// number of move units that could not be checked.
    pub failed: usize,
    /// Number of staging-directory leftovers that recovery could not resolve.
    pub unresolved: usize,
    /// Number of directories created on the backing branch whose times could
    /// not be set.
    pub undated_dirs: usize,
    /// The run stopped early on an error outside any move.
    pub stopped: bool,
}

impl Outcome {
    /// Every attempted move succeeded and the run was not cut short.
    pub fn succeeded(&self) -> bool {
        self.failed == 0 && self.unresolved == 0 && self.undated_dirs == 0 && !self.stopped
    }
}

/// How `cachy run` was invoked.
#[derive(Clone, Copy)]
pub struct Options {
    /// Write nothing; print what a run would do instead.
    pub dry_run: bool,
    /// Skip the check and move every eligible move unit.
    pub drain: bool,
}

/// Free space should grow by at least the bytes moved divided by this; less
/// means that something, such as a snapshot, still holds the moved data.
const FREED_DIVISOR: u64 = 2;

/// Recovers from unfinished moves, then performs a check and, if free space
/// is below the threshold, a run, which stops once its byte budget is
/// consumed. A drain skips the check and has no byte budget; a dry run only
/// prints what a run would do. An error means cachy refused to run; once
/// recovery has started, errors are reported in the outcome instead.
pub fn run(fs: &dyn FsOps, config: &Config, options: Options) -> Result<Outcome> {
    let cache_branch = Branch::open(&config.cache_branch)
        .with_context(|| format!("cannot open cache branch {}", config.cache_branch.display()))?;
    let backing_root = &config.backing_branches[0];
    let backing_branch = Branch::open(backing_root)
        .with_context(|| format!("cannot open backing branch {}", backing_root.display()))?;

    let cache_fs = FsStat::of(cache_branch.fd.as_fd()).with_context(|| {
        format!(
            "cannot measure the size of cache branch {}",
            cache_branch.path.display()
        )
    })?;
    let threshold = config.threshold.resolve(cache_fs.size);
    let target = config.target.resolve(cache_fs.size);
    if target <= threshold {
        bail!(
            "target ({target} bytes, from {}) must be above the threshold ({threshold} bytes, from {}) on the cache branch's {} bytes",
            config.target,
            config.threshold,
            cache_fs.size
        );
    }

    ignore_lease_break_signal();
    let guard = Guard {
        lease: match config.lease_type {
            LeaseType::Read => Lease::Read,
            LeaseType::Write => Lease::Write,
        },
        minimum_age: config.minimum_age,
    };
    let allowed_losses = AllowedLosses::of(&config.allow_metadata_loss);
    let unresolved = if options.dry_run {
        warn_of_leftovers(&cache_branch, &backing_branch);
        0
    } else {
        recover(
            fs,
            &cache_branch,
            &backing_branch,
            guard.lease,
            allowed_losses,
        )
    };
    let mut outcome = Outcome {
        failed: 0,
        unresolved,
        undated_dirs: 0,
        stopped: false,
    };
    let free_before = match stat_fs(fs, &cache_branch) {
        Ok(stat) => stat.free,
        Err(e) => {
            eprintln!("cachy: error: run stopped: {e:#}");
            outcome.stopped = true;
            return Ok(outcome);
        }
    };
    let budget = if options.drain {
        eprintln!("cachy: draining: moving every eligible move unit");
        None
    } else if free_before >= threshold {
        eprintln!(
            "cachy: free space {free_before} bytes is at or above the threshold {threshold} bytes; nothing to do"
        );
        if options.dry_run {
            println!("byte budget: none (free space is at or above the threshold)");
        }
        return Ok(outcome);
    } else {
        eprintln!(
            "cachy: free space {free_before} bytes is below the threshold {threshold} bytes; moving until {target} bytes are free"
        );
        Some(target - free_before)
    };

    let order = match config.order {
        Order::Atime if cache_fs.noatime => {
            eprintln!(
                "cachy: warning: the cache branch is mounted noatime, so move units are ordered by mtime instead of atime"
            );
            Order::Mtime
        }
        order => order,
    };
    let entries = scan(&cache_branch.path, &config.exclude, order)
        .with_context(|| format!("cannot scan cache branch {}", cache_branch.path.display()))?;
    if options.dry_run {
        outcome.failed = dry_run(fs, &cache_branch, &guard, &entries, budget);
        return Ok(outcome);
    }

    let mut tally = Tally::default();
    let mut mover = Mover::new(
        fs,
        &cache_branch,
        &backing_branch,
        &guard,
        allowed_losses,
        config.verify,
    );
    let consumed = |mover: &Mover| budget.is_some_and(|budget| mover.allocated_bytes() >= budget);
    let mut entries = entries.iter();
    loop {
        tally.record(mover.finished());
        if consumed(&mover) {
            // A retiring move that is rolled back hands its bytes back.
            mover.finish();
            tally.record(mover.finished());
            if consumed(&mover) {
                break;
            }
        }
        let Some(entry) = entries.next() else {
            break;
        };
        mover.start(&entry.rels, entry.kind);
    }
    mover.finish();
    tally.record(mover.finished());
    let undated = mover.date_created_dirs();
    for (rel, e) in &undated {
        eprintln!(
            "cachy: error: cannot set the times of created directory {}: {e:#}",
            rel.display()
        );
    }
    outcome.undated_dirs = undated.len();
    let moved_bytes = mover.allocated_bytes();

    for branch in [&cache_branch, &backing_branch] {
        // Fails harmlessly if the staging directory is absent or not empty.
        let _ = fs.rmdir(branch.fd.as_fd(), Path::new(STAGING));
    }
    if moved_bytes > 0 {
        // Best effort: only makes the frees show sooner.
        let _ = fs.sync_fs(cache_branch.fd.as_fd());
    }
    match stat_fs(fs, &cache_branch) {
        Ok(stat) => {
            let free = stat.free;
            if budget.is_some() && free < target {
                eprintln!(
                    "cachy: warning: target not reached: {free} bytes free, target {target} bytes"
                );
            }
            let freed = free.saturating_sub(free_before);
            if freed < moved_bytes / FREED_DIVISOR {
                eprintln!(
                    "cachy: warning: free space grew by only {freed} bytes, but the moves took up {moved_bytes} bytes on it; snapshots of the cache branch may be holding the space"
                );
            }
        }
        Err(e) => eprintln!("cachy: warning: {e:#}"),
    }
    let Tally {
        moved,
        skipped,
        failed,
    } = tally;
    eprintln!("cachy: run finished: {moved} moved, {skipped} skipped, {failed} failed");
    outcome.failed = failed;
    Ok(outcome)
}

/// Prints the byte budget, then the move units that a run would move, in
/// order, and every skip with its reason, until the byte budget is consumed.
/// Returns the number of move units that could not be checked.
fn dry_run(
    fs: &dyn FsOps,
    cache_branch: &Branch,
    guard: &Guard,
    entries: &[Entry],
    budget: Option<u64>,
) -> usize {
    match budget {
        Some(budget) => println!("byte budget: {budget} bytes"),
        None => println!("byte budget: unlimited (drain)"),
    }
    let (mut planned, mut moves, mut skips, mut failed) = (0, 0, 0, 0);
    for entry in entries {
        if budget.is_some_and(|budget| planned >= budget) {
            break;
        }
        let rel = names(&entry.rels);
        match would_skip(fs, cache_branch, guard, &entry.rels, entry.kind) {
            Ok(None) => {
                println!("would move {rel}");
                planned += entry.allocated;
                moves += 1;
            }
            Ok(Some(skip)) => {
                println!("would skip {rel}: {skip}");
                skips += 1;
            }
            Err(e) => {
                eprintln!("cachy: error: cannot check {rel}: {e:#}");
                failed += 1;
            }
        }
    }
    eprintln!(
        "cachy: dry run finished: {moves} would move ({planned} bytes), {skips} would skip, {failed} failed"
    );
    failed
}

/// A dry run does not recover, so it notes the staging directories that the
/// next run's recovery will resolve.
fn warn_of_leftovers(cache_branch: &Branch, backing_branch: &Branch) {
    for branch in [cache_branch, backing_branch] {
        let staging = branch.path.join(STAGING);
        if staging.symlink_metadata().is_ok() {
            eprintln!(
                "cachy: warning: {} holds leftovers of an unfinished move, which the next run will recover",
                staging.display()
            );
        }
    }
}

/// Counts finished moves by outcome, logging skips and failures.
#[derive(Default)]
struct Tally {
    moved: usize,
    skipped: usize,
    failed: usize,
}

impl Tally {
    fn record(&mut self, finished: Vec<(&[PathBuf], Result<MoveOutcome>)>) {
        for (rels, outcome) in finished {
            let rel = names(rels);
            match outcome {
                Ok(MoveOutcome::Moved(hash)) => {
                    self.moved += 1;
                    eprintln!("cachy: moved {rel} (xxh3 {hash})");
                }
                Ok(MoveOutcome::Skipped(reason)) => {
                    self.skipped += 1;
                    eprintln!("cachy: skipped {rel}: {reason}");
                }
                Err(e) => {
                    self.failed += 1;
                    eprintln!("cachy: error: move of {rel} failed: {e:#}");
                }
            }
        }
    }
}

/// The names of a move unit, as one line: a hardlink group's joined by
/// commas.
fn names(rels: &[PathBuf]) -> String {
    rels.iter()
        .map(|rel| rel.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A lease break signals its holder with `SIGIO`, which would terminate cachy.
/// Moves notice a break by querying their lease instead.
fn ignore_lease_break_signal() {
    // SAFETY: setting a signal to SIG_IGN installs no handler code.
    unsafe { libc::signal(libc::SIGIO, libc::SIG_IGN) };
}

fn stat_fs(fs: &dyn FsOps, cache_branch: &Branch) -> Result<FsStat> {
    fs.stat_fs(cache_branch.fd.as_fd()).with_context(|| {
        format!(
            "cannot measure free space of cache branch {}",
            cache_branch.path.display()
        )
    })
}
