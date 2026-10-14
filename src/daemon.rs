//! `cachy daemon`: a long-running service that performs a check at each time
//! of its schedules, through the same orchestrator and instance lock as
//! `cachy run`, until it is asked to stop.
//!
//! A check that finds the instance lock held, such as by a manual run, is
//! skipped with a warning, and one that cachy refuses for another reason is
//! logged; either way, the daemon goes on to its next check.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use jiff::Zoned;

use crate::clock::Clock;
use crate::config::Config;
use crate::fsops::FsOps;
use crate::hook;
use crate::lock;
use crate::log;
use crate::run::{Options, run};
use crate::schedule::Schedule;
use crate::stop;
use crate::summary::Reason;

/// Where `systemctl enable` links the timers that `timers.target` wants.
const SYSTEMD_DIR: &str = "/etc/systemd/system";
/// The shipped timer, relative to [`SYSTEMD_DIR`], once it is enabled.
const ENABLED_TIMER: &str = "timers.target.wants/cachy.timer";

/// What the daemon needs to perform checks: the filesystem operations of its
/// runs, the config and its clock.
pub struct Daemon<'a> {
    pub fs: &'a dyn FsOps,
    pub probe_fs: &'a dyn FsOps,
    pub config: &'a Config,
    pub clock: &'a dyn Clock,
}

impl Daemon<'_> {
    /// Performs the scheduled checks until cachy is asked to stop. An error
    /// means that the daemon refused to start.
    pub fn serve(&self) -> Result<()> {
        let schedules = match &self.config.daemon {
            Some(daemon) if !daemon.schedules.is_empty() => &daemon.schedules,
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
            self.check(Reason::Schedule);
            if stop::requested() {
                break;
            }
        }
        log::info!("daemon stopped");
        Ok(())
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
