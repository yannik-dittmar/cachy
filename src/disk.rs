//! Disk state: whether the backing disks are awake, that is, every one of
//! them spun up.
//!
//! The daemon reads it through [`DiskState`], so that a fault-injection build
//! can hand it a scripted one (`fault::ScriptedDisks`) instead of the real
//! disks. The real one, [`SystemDisks`], asks each disk for its power mode
//! with ATA CHECK POWER MODE ([`ata`]), which does not wake it. Where that
//! cannot work, such as behind a USB bridge or without `CAP_SYS_RAWIO`, it
//! falls back, for that disk alone, to the disk's I/O counters in
//! `/sys/block/<disk>/stat` and its configured spindown timeout
//! ([`IoActivity`]).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;

use crate::ata::{self, Answer, PowerMode};
use crate::config::BackingDisk;
use crate::log;

pub trait DiskState {
    /// The backing disks, by ID, that are not spun up at `now`; none means
    /// that the backing disks are awake.
    fn asleep(&self, now: Timestamp) -> Vec<PathBuf>;
}

/// The real backing disks.
pub struct SystemDisks {
    disks: Vec<Disk>,
}

struct Disk {
    id: PathBuf,
    /// The whole disk's device, such as `/dev/sdb`.
    dev: PathBuf,
    /// Its I/O counters, such as `/sys/block/sdb/stat`.
    stat: PathBuf,
    /// Whether its power mode can be read; once it cannot, the I/O counters
    /// tell instead.
    power_mode: RefCell<bool>,
    activity: RefCell<IoActivity>,
}

impl SystemDisks {
    /// Finds each of `disks` by its ID; an error names a disk that is not
    /// there, or not a disk.
    pub fn open(disks: &[BackingDisk]) -> Result<SystemDisks> {
        let disks = disks
            .iter()
            .map(|disk| {
                Disk::open(disk).with_context(|| format!("backing disk {}", disk.id.display()))
            })
            .collect::<Result<_>>()?;
        Ok(SystemDisks { disks })
    }
}

impl Disk {
    fn open(disk: &BackingDisk) -> Result<Disk> {
        use std::os::unix::fs::FileTypeExt;

        let dev = match std::fs::canonicalize(&disk.id) {
            Ok(dev) => dev,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("no such disk"),
            Err(e) => return Err(e).context("cannot resolve its ID"),
        };
        if !std::fs::metadata(&dev)?.file_type().is_block_device() {
            bail!("{} is not a block device", dev.display());
        }
        let name = dev.file_name().context("no device name")?;
        // A partition's sysfs node is in its disk's.
        let node = std::fs::canonicalize(Path::new("/sys/class/block").join(name))
            .with_context(|| format!("{} is not in /sys/class/block", dev.display()))?;
        let node = if node.join("partition").exists() {
            node.parent()
                .context("a partition without a disk")?
                .to_owned()
        } else {
            node
        };
        let name = node.file_name().context("no disk name")?;
        Ok(Disk {
            id: disk.id.clone(),
            dev: Path::new("/dev").join(name),
            stat: node.join("stat"),
            power_mode: RefCell::new(true),
            activity: RefCell::new(IoActivity::new(disk.spindown_timeout.unwrap_or_default())),
        })
    }

    fn awake(&self, now: Timestamp) -> bool {
        if *self.power_mode.borrow() {
            match ata::check_power_mode(&self.dev) {
                Answer::PowerMode(PowerMode::SpunUp(_)) => return true,
                Answer::PowerMode(PowerMode::Standby) => return false,
                // A disk in the sleep state does not answer.
                Answer::Silent(_) => return false,
                Answer::Unsupported(why) => {
                    log::warning!(
                        "cannot read the power mode of backing disk {} ({why}); judging by its I/O and spindown timeout instead",
                        self.id.display()
                    );
                    *self.power_mode.borrow_mut() = false;
                }
            }
        }
        match std::fs::read_to_string(&self.stat)
            .ok()
            .and_then(|text| Counters::parse(&text))
        {
            Some(counters) => self.activity.borrow_mut().awake(counters, now),
            None => {
                log::warning!(
                    "cannot read the I/O counters of backing disk {} in {}",
                    self.id.display(),
                    self.stat.display()
                );
                false
            }
        }
    }
}

impl DiskState for SystemDisks {
    fn asleep(&self, now: Timestamp) -> Vec<PathBuf> {
        self.disks
            .iter()
            .filter(|disk| !disk.awake(now))
            .map(|disk| disk.id.clone())
            .collect()
    }
}

/// A disk's I/O counters, from `/sys/block/<disk>/stat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counters {
    /// Requests completed: reads, writes, discards and flushes.
    pub requests: u64,
    /// Requests in flight.
    pub in_flight: u64,
}

impl Counters {
    /// Parses the contents of a `stat` file: reads, writes and in-flight
    /// requests always, discards and flushes where the kernel has them.
    pub fn parse(text: &str) -> Option<Counters> {
        let fields = text
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<Vec<u64>, _>>()
            .ok()?;
        if fields.len() < 11 {
            return None;
        }
        let optional = |i: usize| fields.get(i).copied().unwrap_or(0);
        Some(Counters {
            requests: fields[0] + fields[4] + optional(11) + optional(15),
            in_flight: fields[8],
        })
    }
}

/// I/O since the previous look within this long counts as recent, whatever
/// the spindown timeout.
const RECENT: Duration = Duration::from_secs(60);

/// When a disk last did I/O, as far as its counters tell, and so whether it
/// is spun up: it is if it did I/O recently, or more recently than its
/// spindown timeout.
pub struct IoActivity {
    spindown_timeout: Duration,
    /// The counters at the previous look, and when that was.
    previous: Option<(Counters, Timestamp)>,
    /// The earliest that the last I/O can have been.
    last_io: Option<Timestamp>,
}

impl IoActivity {
    pub fn new(spindown_timeout: Duration) -> IoActivity {
        IoActivity {
            spindown_timeout,
            previous: None,
            last_io: None,
        }
    }

    /// Whether the disk is spun up at `now`, when its counters are
    /// `counters`. Until the counters have been seen twice, only I/O in
    /// flight tells.
    pub fn awake(&mut self, counters: Counters, now: Timestamp) -> bool {
        if let Some((previous, then)) = self.previous
            && previous.requests != counters.requests
        {
            // Somewhere between then and now; then, to be safe.
            self.last_io = Some(then);
        }
        if counters.in_flight > 0 {
            self.last_io = Some(now);
        }
        self.previous = Some((counters, now));
        let window = self.spindown_timeout.max(RECENT);
        self.last_io.is_some_and(|last_io| {
            Duration::try_from(now.duration_since(last_io)).is_ok_and(|since| since < window)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> Timestamp {
        Timestamp::from_second(1_767_225_600 + 60 * minutes).unwrap()
    }

    fn requests(requests: u64) -> Counters {
        Counters {
            requests,
            in_flight: 0,
        }
    }

    #[test]
    fn stat_files_give_completed_and_in_flight_requests() {
        // Linux 5.5 and later: 17 fields.
        let text = "    1200        3    40000      500      300        5     8000      200        2     1000     1500       10        0      800        4       20       30\n";
        assert_eq!(
            Counters::parse(text),
            Some(Counters {
                requests: 1200 + 300 + 10 + 20,
                in_flight: 2,
            })
        );
        // Before discards: 11 fields.
        assert_eq!(
            Counters::parse("7 0 56 1 9 0 72 3 0 4 4"),
            Some(Counters {
                requests: 16,
                in_flight: 0,
            })
        );
        assert_eq!(Counters::parse("7 0 56"), None);
        assert_eq!(Counters::parse("garbage"), None);
    }

    #[test]
    fn io_since_a_recent_look_means_spun_up() {
        let mut disk = IoActivity::new(Duration::ZERO);
        assert!(!disk.awake(requests(10), at(0)), "nothing known yet");
        assert!(disk.awake(requests(12), at(0) + Duration::from_secs(30)));
        // No I/O since: no longer recent.
        assert!(!disk.awake(requests(12), at(2)));
    }

    #[test]
    fn io_in_flight_means_spun_up() {
        let mut disk = IoActivity::new(Duration::ZERO);
        let busy = Counters {
            requests: 10,
            in_flight: 1,
        };
        assert!(disk.awake(busy, at(0)));
    }

    #[test]
    fn a_disk_is_spun_up_until_its_spindown_timeout_after_its_last_io() {
        let mut disk = IoActivity::new(Duration::from_secs(20 * 60));
        disk.awake(requests(10), at(0));
        assert!(disk.awake(requests(11), at(1)));
        assert!(disk.awake(requests(11), at(15)));
        // The I/O was between minutes 0 and 1: by minute 20, it may have spun
        // down.
        assert!(!disk.awake(requests(11), at(20)));
    }

    #[test]
    fn io_at_an_unknown_time_long_ago_does_not_mean_spun_up() {
        let mut disk = IoActivity::new(Duration::from_secs(20 * 60));
        disk.awake(requests(10), at(0));
        // Some I/O in the last six hours: it may have spun down since.
        assert!(!disk.awake(requests(50), at(6 * 60)));
        // More I/O within the last half minute.
        assert!(disk.awake(requests(51), at(6 * 60) + Duration::from_secs(30)));
    }
}
