//! The run orchestrator: performs the check and, if it finds work, moves files
//! until free space reaches the target.

use std::os::fd::AsFd;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::fsops::FsOps;
use crate::scan::scan;
use crate::transaction::{Branch, Guard, Outcome as MoveOutcome, STAGING, move_file};

pub struct Outcome {
    /// Number of moves that were attempted and failed.
    pub failed: usize,
    /// The run stopped early on an error outside any move.
    pub stopped: bool,
}

impl Outcome {
    /// Every attempted move succeeded and the run was not cut short.
    pub fn succeeded(&self) -> bool {
        self.failed == 0 && !self.stopped
    }
}

/// Performs a check and, if free space is below the threshold, a run. An
/// error means cachy refused to run; once moving has started, errors are
/// reported in the outcome instead.
pub fn run(fs: &dyn FsOps, config: &Config) -> Result<Outcome> {
    let cache_branch = Branch::open(&config.cache_branch)
        .with_context(|| format!("cannot open cache branch {}", config.cache_branch.display()))?;
    let backing_root = &config.backing_branches[0];
    let backing_branch = Branch::open(backing_root)
        .with_context(|| format!("cannot open backing branch {}", backing_root.display()))?;

    let mut free = free_space(fs, &cache_branch)?;
    if free >= config.threshold {
        eprintln!(
            "cachy: free space {free} bytes is at or above the threshold {} bytes; nothing to do",
            config.threshold
        );
        return Ok(Outcome {
            failed: 0,
            stopped: false,
        });
    }
    eprintln!(
        "cachy: free space {free} bytes is below the threshold {} bytes; moving until {} bytes are free",
        config.threshold, config.target
    );

    ignore_lease_break_signal();
    let guard = Guard {
        minimum_age: config.minimum_age,
    };
    let units = scan(&cache_branch.path)
        .with_context(|| format!("cannot scan cache branch {}", cache_branch.path.display()))?;
    let mut moved = 0;
    let mut skipped = 0;
    let mut failed = 0;
    let mut stopped = false;
    for unit in &units {
        if free >= config.target {
            break;
        }
        match move_file(fs, &cache_branch, &backing_branch, &unit.rel, &guard) {
            Ok(MoveOutcome::Moved) => moved += 1,
            Ok(MoveOutcome::Skipped(reason)) => {
                skipped += 1;
                eprintln!("cachy: skipped {}: {reason}", unit.rel.display());
            }
            Err(e) => {
                failed += 1;
                eprintln!("cachy: error: move of {} failed: {e:#}", unit.rel.display());
            }
        }
        match free_space(fs, &cache_branch) {
            Ok(now) => free = now,
            Err(e) => {
                eprintln!("cachy: error: run stopped: {e:#}");
                stopped = true;
                break;
            }
        }
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
    eprintln!("cachy: run finished: {moved} moved, {skipped} skipped, {failed} failed");
    Ok(Outcome { failed, stopped })
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
