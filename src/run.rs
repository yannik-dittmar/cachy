//! The run orchestrator: performs the check and, if it finds work, moves files
//! until free space reaches the target.

use std::os::fd::AsFd;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::fsops::FsOps;
use crate::scan::scan;
use crate::transaction::{Branch, STAGING, move_file};

pub struct Outcome {
    /// Number of moves that were attempted and failed.
    pub failed: usize,
}

/// Performs a check and, if free space is below the threshold, a run. An
/// error means cachy refused to run.
pub fn run(fs: &dyn FsOps, config: &Config) -> Result<Outcome> {
    let cache = Branch::open(&config.cache_branch)
        .with_context(|| format!("cannot open cache branch {}", config.cache_branch.display()))?;
    let backing_path = &config.backing_branches[0];
    let backing = Branch::open(backing_path)
        .with_context(|| format!("cannot open backing branch {}", backing_path.display()))?;

    let mut free = free_space(fs, &cache)?;
    if free >= config.threshold {
        eprintln!(
            "cachy: free space {free} bytes is at or above the threshold {} bytes; nothing to do",
            config.threshold
        );
        return Ok(Outcome { failed: 0 });
    }
    eprintln!(
        "cachy: free space {free} bytes is below the threshold {} bytes; moving until {} bytes are free",
        config.threshold, config.target
    );

    let units = scan(&cache.path)
        .with_context(|| format!("cannot scan cache branch {}", cache.path.display()))?;
    let mut moved = 0;
    let mut failed = 0;
    for unit in &units {
        if free >= config.target {
            break;
        }
        match move_file(fs, &cache, &backing, &unit.rel) {
            Ok(()) => moved += 1,
            Err(e) => {
                failed += 1;
                eprintln!("cachy: error: move of {} failed: {e:#}", unit.rel.display());
            }
        }
        free = free_space(fs, &cache)?;
    }

    for branch in [&cache, &backing] {
        // Fails harmlessly if the staging directory is absent or not empty.
        let _ = fs.rmdir(branch.fd.as_fd(), Path::new(STAGING));
    }
    if free < config.target {
        eprintln!(
            "cachy: warning: target not reached: {free} bytes free, target {} bytes",
            config.target
        );
    }
    eprintln!("cachy: run finished: {moved} moved, {failed} failed");
    Ok(Outcome { failed })
}

fn free_space(fs: &dyn FsOps, cache: &Branch) -> Result<u64> {
    fs.free_space(cache.fd.as_fd()).with_context(|| {
        format!(
            "cannot measure free space of cache branch {}",
            cache.path.display()
        )
    })
}
