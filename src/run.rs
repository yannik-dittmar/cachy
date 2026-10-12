//! The run orchestrator: performs the check and, if it finds work, moves files
//! until free space reaches the target.

use std::os::fd::AsFd;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{Config, LeaseType};
use crate::fsops::{FsOps, Lease};
use crate::recovery::recover;
use crate::scan::scan;
use crate::transaction::{Branch, Guard, MoveOutcome, Mover, STAGING};

pub struct Outcome {
    /// Number of moves that were attempted and failed.
    pub failed: usize,
    /// Number of staging-directory leftovers that recovery could not resolve.
    pub unresolved: usize,
    /// The run stopped early on an error outside any move.
    pub stopped: bool,
}

impl Outcome {
    /// Every attempted move succeeded and the run was not cut short.
    pub fn succeeded(&self) -> bool {
        self.failed == 0 && self.unresolved == 0 && !self.stopped
    }
}

/// Recovers from unfinished moves, then performs a check and, if free space
/// is below the threshold, a run. An error means cachy refused to run; once
/// recovery has started, errors are reported in the outcome instead.
pub fn run(fs: &dyn FsOps, config: &Config) -> Result<Outcome> {
    let cache_branch = Branch::open(&config.cache_branch)
        .with_context(|| format!("cannot open cache branch {}", config.cache_branch.display()))?;
    let backing_root = &config.backing_branches[0];
    let backing_branch = Branch::open(backing_root)
        .with_context(|| format!("cannot open backing branch {}", backing_root.display()))?;

    let unresolved = recover(fs, &cache_branch, &backing_branch);
    let mut free = match free_space(fs, &cache_branch) {
        Ok(free) => free,
        Err(e) => {
            eprintln!("cachy: error: run stopped: {e:#}");
            return Ok(Outcome {
                failed: 0,
                unresolved,
                stopped: true,
            });
        }
    };
    if free >= config.threshold {
        eprintln!(
            "cachy: free space {free} bytes is at or above the threshold {} bytes; nothing to do",
            config.threshold
        );
        return Ok(Outcome {
            failed: 0,
            unresolved,
            stopped: false,
        });
    }
    eprintln!(
        "cachy: free space {free} bytes is below the threshold {} bytes; moving until {} bytes are free",
        config.threshold, config.target
    );

    ignore_lease_break_signal();
    let guard = Guard {
        lease: match config.lease_type {
            LeaseType::Read => Lease::Read,
            LeaseType::Write => Lease::Write,
        },
        minimum_age: config.minimum_age,
    };
    let units = scan(&cache_branch.path)
        .with_context(|| format!("cannot scan cache branch {}", cache_branch.path.display()))?;
    let mut tally = Tally::default();
    let mut stopped = false;
    let mut mover = Mover::new(fs, &cache_branch, &backing_branch, &guard);
    for unit in &units {
        tally.record(mover.finished());
        // Retiring moves free their space once their sources are unlinked.
        if free + mover.retiring_bytes() >= config.target {
            break;
        }
        mover.start(&unit.rel);
        match free_space(fs, &cache_branch) {
            Ok(now) => free = now,
            Err(e) => {
                eprintln!("cachy: error: run stopped: {e:#}");
                stopped = true;
                break;
            }
        }
    }
    mover.finish();
    tally.record(mover.finished());
    if let Ok(now) = free_space(fs, &cache_branch) {
        free = now;
    }

    for branch in [&cache_branch, &backing_branch] {
        // Fails harmlessly if the staging directory is absent or not empty.
        let _ = fs.rmdir(branch.fd.as_fd(), Path::new(STAGING));
    }
    if !stopped && free < config.target {
        eprintln!(
            "cachy: warning: target not reached: {free} bytes free, target {} bytes",
            config.target
        );
    }
    let Tally {
        moved,
        skipped,
        failed,
    } = tally;
    eprintln!("cachy: run finished: {moved} moved, {skipped} skipped, {failed} failed");
    Ok(Outcome {
        failed,
        unresolved,
        stopped,
    })
}

/// Counts finished moves by outcome, logging skips and failures.
#[derive(Default)]
struct Tally {
    moved: usize,
    skipped: usize,
    failed: usize,
}

impl Tally {
    fn record(&mut self, finished: Vec<(&Path, Result<MoveOutcome>)>) {
        for (rel, outcome) in finished {
            match outcome {
                Ok(MoveOutcome::Moved) => self.moved += 1,
                Ok(MoveOutcome::Skipped(reason)) => {
                    self.skipped += 1;
                    eprintln!("cachy: skipped {}: {reason}", rel.display());
                }
                Err(e) => {
                    self.failed += 1;
                    eprintln!("cachy: error: move of {} failed: {e:#}", rel.display());
                }
            }
        }
    }
}

/// A lease break signals its holder with `SIGIO`, which would terminate cachy.
/// Moves notice a break by querying their lease instead.
fn ignore_lease_break_signal() {
    // SAFETY: setting a signal to SIG_IGN installs no handler code.
    unsafe { libc::signal(libc::SIGIO, libc::SIG_IGN) };
}

fn free_space(fs: &dyn FsOps, cache_branch: &Branch) -> Result<u64> {
    fs.free_space(cache_branch.fd.as_fd()).with_context(|| {
        format!(
            "cannot measure free space of cache branch {}",
            cache_branch.path.display()
        )
    })
}
