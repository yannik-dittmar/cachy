//! `cachy daemon`: a long-running service that performs a check at each time
//! of its schedules, through the same orchestrator and instance lock as
//! `cachy run`, until it is asked to stop.
//!
//! With a wake gate, a scheduled check first waits until the backing disks
//! are awake, looking at their state every [`DISK_POLL`] on the daemon's
//! clock. At the gate's time limit, the check goes on only if free space is
//! below the threshold; otherwise it is skipped.
//!
//! With a wake trigger, the daemon also looks at the state of the backing
//! disks every check interval, and performs a check whenever they are awake.
//! A disk that only cachy's own I/O keeps spun up counts as asleep to the
//! trigger, so that its checks cannot keep it firing. cachy's own I/O counts
//! as settled [`OWN_IO_SETTLE`] after a check that reached the backing disks;
//! from then on, their I/O is others'.
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
/// How long after a check its I/O can still reach the backing disks, such as
/// when a filesystem writes its journal and metadata back.
const OWN_IO_SETTLE: Duration = Duration::from_secs(60);
/// How the log gives a time.
const LOG_TIME: &str = "%Y-%m-%d %H:%M %Z";

/// A check that the daemon performs.
#[derive(Clone, Copy)]
enum Check {
    Scheduled,
    WakeTriggered,
}

impl Check {
    /// The reason that the run summary gives.
    fn reason(self) -> Reason {
        match self {
            Check::Scheduled => Reason::Schedule,
            Check::WakeTriggered => Reason::WakeTrigger,
        }
    }

    /// What the log calls it.
    fn name(self) -> &'static str {
        match self {
            Check::Scheduled => "scheduled check",
            Check::WakeTriggered => "wake-triggered check",
        }
    }
}

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
    /// Performs the scheduled and wake-triggered checks until cachy is asked
    /// to stop. An error means that the daemon refused to start.
    pub fn serve(&self) -> Result<()> {
        let nothing_to_do = "the daemon has nothing to do: set daemon.schedules or daemon.wake_trigger in the config";
        let Some(daemon) = &self.config.daemon else {
            bail!(nothing_to_do);
        };
        let schedules = &daemon.schedules;
        let trigger = daemon.wake_trigger.as_ref().zip(self.disks);
        if schedules.is_empty() && trigger.is_none() {
            bail!(nothing_to_do);
        }
        stop::install();
        warn_if_timer_enabled(&systemd_dir());
        if let Some((trigger, _)) = trigger {
            log::info!(
                "wake trigger: a check every {} while the backing disks are awake",
                minutes(trigger.check_interval)
            );
        }
        // The next scheduled check, once known, and the time last logged.
        let mut scheduled: Option<(Zoned, Vec<String>)> = None;
        let mut announced: Option<Zoned> = None;
        // When the wake trigger looks at the backing disks next: at once.
        let mut next_look = trigger.map(|_| self.clock.now());
        // The disks that kept the last look from a check, once logged.
        let mut not_awake: Option<Vec<PathBuf>> = None;
        // When cachy's own I/O of its last check has settled, for the wake
        // trigger.
        let mut settle: Option<Zoned> = None;
        loop {
            if scheduled.is_none() {
                scheduled = next_check(schedules, &self.clock.now());
            }
            if let Some((time, _)) = &scheduled
                && announced.as_ref() != Some(time)
            {
                log::info!("next scheduled check at {}", time.strftime(LOG_TIME));
                announced = Some(time.clone());
            }
            // There is something to settle only with a wake trigger, which
            // always has a look to come.
            let Some(wake) = [
                scheduled.as_ref().map(|(time, _)| time),
                next_look.as_ref(),
                settle.as_ref(),
            ]
            .into_iter()
            .flatten()
            .min()
            .cloned() else {
                bail!("no schedule has a time left");
            };
            if !self.clock.sleep_until(wake.timestamp()) {
                break;
            }
            let now = self.clock.now();
            if let Some(disks) = self.disks
                && settle.take_if(|settle| *settle <= now).is_some()
            {
                disks.own_io_settled(now.timestamp());
            }
            // When cachy's own I/O of a check ended, if one reached the disks.
            let mut own_io = None;
            if let Some((_, due)) = scheduled.take_if(|(time, _)| *time <= now) {
                log::info!("scheduled check ({})", due.join(", "));
                let gate = match (&daemon.wake_gate, self.disks) {
                    (Some(gate), Some(disks)) => self.wait_at_wake_gate(gate, disks),
                    _ => Gate::Open,
                };
                match gate {
                    Gate::Open => own_io = self.check(Check::Scheduled),
                    Gate::Skip => {}
                    Gate::Stop => break,
                }
                // A look that came due meanwhile would be for a check that
                // just happened.
                let after = self.clock.now();
                if let (Some((trigger, _)), Some(look)) = (trigger, &mut next_look)
                    && *look <= after
                {
                    *look = later(&after, trigger.check_interval);
                }
            } else if let Some((trigger, disks)) = trigger
                && next_look.as_ref().is_some_and(|look| *look <= now)
            {
                next_look = Some(later(&now, trigger.check_interval));
                own_io = self.look_for_wake_trigger(disks, &now, &mut not_awake);
            }
            if let Some(end) = own_io
                && trigger.is_some()
            {
                settle = Some(later(&end, OWN_IO_SETTLE));
            }
            if stop::requested() {
                break;
            }
        }
        log::info!("daemon stopped");
        Ok(())
    }

    /// Looks at the backing disks for the wake trigger, at `now`, and
    /// performs a check if they are awake on others' account; when that
    /// check's own I/O on them ended, as [`Daemon::check`] tells. Logs the
    /// disks that keep it from a check when they differ from `not_awake`,
    /// those it logged last.
    fn look_for_wake_trigger(
        &self,
        disks: &dyn DiskState,
        now: &Zoned,
        not_awake: &mut Option<Vec<PathBuf>>,
    ) -> Option<Zoned> {
        let asleep = disks.asleep_but_for_own_io(now.timestamp());
        if asleep.is_empty() {
            *not_awake = None;
            log::info!("wake-triggered check: the backing disks are awake");
            return self.check(Check::WakeTriggered);
        }
        if not_awake.as_ref() != Some(&asleep) {
            log::info!(
                "no wake-triggered checks until the backing disks are awake; asleep, or spun up only by cachy's own I/O: {}",
                ids(&asleep)
            );
            *not_awake = Some(asleep);
        }
        None
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
        let deadline = later(&start, gate.time_limit);
        log::info!(
            "waiting for the backing disks to wake until {} (asleep: {})",
            deadline.strftime(LOG_TIME),
            ids(&asleep)
        );
        loop {
            let now = self.clock.now();
            if now >= deadline {
                break;
            }
            let poll = later(&now, DISK_POLL);
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

    /// Performs `check` and, if it finds work, a run. Whatever happens is
    /// logged; the daemon goes on either way. When the check's own I/O on
    /// the backing disks ended, if it reached them: one that ran probed them,
    /// and one that was refused may have; one that found the instance lock
    /// held did not.
    fn check(&self, check: Check) -> Option<Zoned> {
        let options = Options {
            dry_run: false,
            reason: check.reason(),
        };
        let did_io = match run(self.fs, self.probe_fs, self.config, options) {
            Ok(outcome) => {
                hook::after(self.config, &outcome);
                true
            }
            Err(e) if e.downcast_ref::<lock::Held>().is_some() => {
                log::warning!("skipped the {}: {e:#}", check.name());
                false
            }
            Err(e) => {
                log::error!("the {} refused to run: {e:#}", check.name());
                true
            }
        };
        // What cachy did on the backing disks, such as probing them, is no
        // sign that they are in use.
        if !did_io {
            return None;
        }
        // The settle is measured from this time, which the disks are told,
        // so that it does not depend on how long telling them takes.
        let end = self.clock.now();
        if let Some(disks) = self.disks {
            disks.own_io(end.timestamp());
        }
        Some(end)
    }
}

/// `duration` after `time`, or `time` itself if that is past the end of
/// time.
fn later(time: &Zoned, duration: Duration) -> Zoned {
    time.checked_add(duration).unwrap_or_else(|_| time.clone())
}

/// `duration`, for the log, in whole minutes where it is.
fn minutes(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
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
