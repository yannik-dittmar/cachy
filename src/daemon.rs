//! `cachy daemon`: a long-running service that performs a check at each time
//! of its schedules, through the same orchestrator and instance lock as
//! `cachy run`, until it is asked to stop.
//!
//! With a wake gate, a scheduled check first waits until the backing disks
//! are awake, looking at their state every [`DISK_POLL`] on the daemon's
//! clock. At the gate's time limit, the check goes on only if free space is
//! below the threshold; otherwise it is skipped.
//!
//! A check that finds the instance lock held, such as by a manual run, is
//! skipped with a warning, and one that cachy refuses for another reason is
//! logged; either way, the daemon goes on to its next check.

use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::Zoned;

use crate::clock::Clock;
use crate::config::{Config, WakeGate};
use crate::disk::DiskState;
use crate::fsops::{FsOps, FsStat};
use crate::hook;
use crate::lock;
use crate::log;
use crate::run::{Options, run};
use crate::schedule::Schedule;
use crate::stop;
use crate::summary::Reason;
use crate::transaction::Branch;

/// Where `systemctl enable` links the timers that `timers.target` wants.
const SYSTEMD_DIR: &str = "/etc/systemd/system";
/// The shipped timer, relative to [`SYSTEMD_DIR`], once it is enabled.
const ENABLED_TIMER: &str = "timers.target.wants/cachy.timer";
/// How often the wake gate looks at the state of the backing disks.
const DISK_POLL: Duration = Duration::from_secs(30);

/// What the wake gate decided for a scheduled check.
enum Gate {
    /// The check goes on.
    Open,
    /// The check is skipped.
    Skip,
    /// cachy was asked to stop while the check waited.
    Stop,
}

/// What the daemon needs to perform checks: the filesystem operations of its
/// runs, the config, its clock and, if the config has backing disks, their
/// state.
pub struct Daemon<'a> {
    pub fs: &'a dyn FsOps,
    pub probe_fs: &'a dyn FsOps,
    pub config: &'a Config,
    pub clock: &'a dyn Clock,
    pub disks: Option<&'a dyn DiskState>,
}

impl Daemon<'_> {
    /// Performs the scheduled checks until cachy is asked to stop. An error
    /// means that the daemon refused to start.
    pub fn serve(&self) -> Result<()> {
        let (schedules, wake_gate) = match &self.config.daemon {
            Some(daemon) if !daemon.schedules.is_empty() => {
                (&daemon.schedules, daemon.wake_gate.as_ref())
            }
            _ => bail!("the daemon has nothing to do: set daemon.schedules in the config"),
        };
        stop::install();
        warn_if_timer_enabled(&systemd_dir());
        loop {
            let Some((time, due)) = next_check(schedules, &self.clock.now()) else {
                bail!("no schedule has a time left");
            };
            log::info!(
                "next scheduled check at {}",
                time.strftime("%Y-%m-%d %H:%M %Z")
            );
            if !self.clock.sleep_until(time.timestamp()) {
                break;
            }
            log::info!("scheduled check ({})", due.join(", "));
            let gate = match (wake_gate, self.disks) {
                (Some(gate), Some(disks)) => self.wait_at_wake_gate(gate, disks),
                _ => Gate::Open,
            };
            match gate {
                Gate::Open => self.check(Reason::Schedule),
                Gate::Skip => {}
                Gate::Stop => break,
            }
            if stop::requested() {
                break;
            }
        }
        log::info!("daemon stopped");
        Ok(())
    }

    /// Holds a scheduled check until the backing disks are awake, or until
    /// the gate's time limit; then, the check goes on only if free space is
    /// below the threshold.
    fn wait_at_wake_gate(&self, gate: &WakeGate, disks: &dyn DiskState) -> Gate {
        let start = self.clock.now();
        let mut asleep = disks.asleep(start.timestamp());
        if asleep.is_empty() {
            log::info!("the backing disks are awake");
            return Gate::Open;
        }
        let deadline = start.checked_add(gate.time_limit).unwrap_or(start);
        log::info!(
            "waiting for the backing disks to wake until {} (asleep: {})",
            deadline.strftime("%Y-%m-%d %H:%M %Z"),
            ids(&asleep)
        );
        loop {
            let now = self.clock.now();
            if now >= deadline {
                break;
            }
            let poll = now.checked_add(DISK_POLL).unwrap_or(now);
            if !self
                .clock
                .sleep_until(poll.min(deadline.clone()).timestamp())
            {
                return Gate::Stop;
            }
            let still = disks.asleep(self.clock.now().timestamp());
            if still.is_empty() {
                log::info!("the backing disks are awake");
                return Gate::Open;
            }
            if still != asleep {
                log::info!(
                    "the backing disks are not awake yet; still asleep: {}",
                    ids(&still)
                );
                asleep = still;
            }
        }
        match self.free_space_and_threshold() {
            Ok((free, threshold)) if free < threshold => {
                log::info!(
                    "the backing disks did not wake within the wake gate's time limit, but free space is below the threshold ({free} < {threshold} bytes), so the scheduled check goes on"
                );
                Gate::Open
            }
            Ok((free, threshold)) => {
                log::info!(
                    "skipped the scheduled check: the backing disks did not wake within the wake gate's time limit, and free space is at or above the threshold ({free} >= {threshold} bytes)"
                );
                Gate::Skip
            }
            Err(e) => {
                log::error!("the scheduled check refused to run: {e:#}");
                Gate::Skip
            }
        }
    }

    /// The cache branch's free space and threshold, in bytes.
    fn free_space_and_threshold(&self) -> Result<(u64, u64)> {
        let path = &self.config.cache_branch;
        let branch = Branch::open(path)
            .with_context(|| format!("cannot open cache branch {}", path.display()))?;
        let stat = FsStat::of(branch.fd.as_fd())
            .with_context(|| format!("cannot measure cache branch {}", path.display()))?;
        Ok((stat.free, self.config.threshold.resolve(stat.size)))
    }

    /// Performs a check and, if it finds work, a run, started for `reason`.
    /// Whatever happens is logged; the daemon goes on either way.
    fn check(&self, reason: Reason) {
        let options = Options {
            dry_run: false,
            reason,
        };
        match run(self.fs, self.probe_fs, self.config, options) {
            Ok(outcome) => hook::after(self.config, &outcome),
            Err(e) if e.downcast_ref::<lock::Held>().is_some() => {
                log::warning!("skipped the {}: {e:#}", check_name(reason));
            }
            Err(e) => log::error!("the {} refused to run: {e:#}", check_name(reason)),
        }
    }
}

/// What the log calls a check started for `reason`.
fn check_name(reason: Reason) -> &'static str {
    match reason {
        Reason::Schedule => "scheduled check",
        Reason::Manual | Reason::Drain => "check",
    }
}

/// Disk IDs, for the log.
fn ids(ids: &[PathBuf]) -> String {
    let ids: Vec<_> = ids.iter().map(|id| id.display().to_string()).collect();
    ids.join(", ")
}

/// The next time after `now` that any of `schedules` gives, with the
/// schedules that give it.
fn next_check(schedules: &[Schedule], now: &Zoned) -> Option<(Zoned, Vec<String>)> {
    let times: Vec<_> = schedules
        .iter()
        .filter_map(|s| Some((s.next_after(now)?, s)))
        .collect();
    let first = times.iter().map(|(time, _)| time).min()?.clone();
    let due = times
        .iter()
        .filter(|(time, _)| *time == first)
        .map(|(_, s)| format!("`{s}`"))
        .collect();
    Some((first, due))
}

/// The directory of the system's own systemd units.
fn systemd_dir() -> PathBuf {
    #[cfg(feature = "fault-injection")]
    if let Some(dir) = crate::fault::systemd_dir() {
        return dir;
    }
    PathBuf::from(SYSTEMD_DIR)
}

/// Warns if `cachy.timer` is enabled in `systemd_dir`: it starts runs on its
/// own schedule, besides the daemon's checks.
fn warn_if_timer_enabled(systemd_dir: &Path) {
    let timer = systemd_dir.join(ENABLED_TIMER);
    if timer.symlink_metadata().is_ok() {
        log::warning!(
            "cachy.timer is enabled ({}), so it starts runs besides the daemon's checks; \
             the instance lock keeps them from overlapping, but use one or the other: \
             systemctl disable --now cachy.timer",
            timer.display()
        );
    }
}
