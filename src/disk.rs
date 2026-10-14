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
//!
//! The wake trigger asks a different question: whether the backing disks are
//! awake on others' account. A disk that only cachy's own I/O keeps spun up
//! is not, so that the trigger cannot keep itself firing; the disk's I/O
//! counters tell when others use it again, once cachy's own I/O has settled
//! ([`OwnIo`]).

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

    /// Like [`DiskState::asleep`], but a disk that is spun up only by
    /// cachy's own I/O, as told by [`DiskState::own_io`], counts as asleep
    /// too; none means that the backing disks are awake on others' account.
    fn asleep_but_for_own_io(&self, now: Timestamp) -> Vec<PathBuf>;

    /// Tells that cachy did I/O on the backing disks until now: until they
    /// do I/O of others, it is no sign that they are in use.
    fn own_io(&self);

    /// Tells that cachy's own I/O has reached the backing disks by `now`, so
    /// that their I/O from here on is others'. Until then, or until the next
    /// look if that comes first, all of it counts as cachy's.
    fn own_io_settled(&self, now: Timestamp);
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
    power_mode_readable: RefCell<bool>,
    activity: RefCell<IoActivity>,
    own_io: RefCell<OwnIo>,
}

/// What a look at a disk tells.
struct Look {
    spun_up: bool,
    /// Whether its I/O since cachy's own, if any, is others'.
    others: bool,
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
            power_mode_readable: RefCell::new(true),
            activity: RefCell::new(IoActivity::new(disk.spindown_timeout)),
            own_io: RefCell::new(OwnIo::default()),
        })
    }

    fn counters(&self) -> Option<Counters> {
        std::fs::read_to_string(&self.stat)
            .ok()
            .and_then(|text| Counters::parse(&text))
    }

    fn look(&self, now: Timestamp) -> Look {
        let counters = self.counters();
        let others = {
            let mut own_io = self.own_io.borrow_mut();
            match counters {
                Some(counters) => own_io.look(counters),
                None => own_io.others(),
            }
        };
        Look {
            spun_up: self.spun_up(counters, now),
            others,
        }
    }

    fn spun_up(&self, counters: Option<Counters>, now: Timestamp) -> bool {
        if *self.power_mode_readable.borrow() {
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
                    *self.power_mode_readable.borrow_mut() = false;
                }
            }
        }
        match counters {
            Some(counters) => self.activity.borrow_mut().spun_up(counters, now),
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
            .filter(|disk| !disk.look(now).spun_up)
            .map(|disk| disk.id.clone())
            .collect()
    }

    fn asleep_but_for_own_io(&self, now: Timestamp) -> Vec<PathBuf> {
        self.disks
            .iter()
            .filter(|disk| {
                let look = disk.look(now);
                !(look.spun_up && look.others)
            })
            .map(|disk| disk.id.clone())
            .collect()
    }

    fn own_io(&self) {
        for disk in &self.disks {
            disk.own_io.borrow_mut().did_io();
        }
    }

    fn own_io_settled(&self, _now: Timestamp) {
        for disk in &self.disks {
            if let Some(counters) = disk.counters() {
                disk.own_io.borrow_mut().look(counters);
            }
        }
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

/// When a disk last did I/O, as far as its counters tell, and so whether it
/// is spun up: it is if it did I/O since the previous look, or more recently
/// than its spindown timeout.
pub struct IoActivity {
    spindown_timeout: Option<Duration>,
    /// The counters at the previous look, and when that was.
    previous: Option<(Counters, Timestamp)>,
    /// The earliest that the last I/O can have been.
    last_io: Option<Timestamp>,
}

impl IoActivity {
    pub fn new(spindown_timeout: Option<Duration>) -> IoActivity {
        IoActivity {
            spindown_timeout,
            previous: None,
            last_io: None,
        }
    }

    /// Whether the disk is spun up at `now`, when its counters are
    /// `counters`.
    ///
    /// I/O since the previous look means that it is, unless that look is
    /// longer ago than the spindown timeout: then the I/O may be too. Without
    /// I/O since, it is spun up only within the spindown timeout of the last
    /// I/O, and never without a spindown timeout.
    ///
    /// The first look has no previous one to compare the counters with, so
    /// only I/O in flight tells: taking an idle disk for spun up could make
    /// cachy wake it, which is what the wake gate is there to avoid, while
    /// taking it for asleep costs only the time until the next look.
    pub fn spun_up(&mut self, counters: Counters, now: Timestamp) -> bool {
        let mut io_since_previous = false;
        if let Some((previous, then)) = self.previous
            && previous.requests != counters.requests
        {
            // Somewhere between then and now; then, to be safe.
            self.last_io = Some(then);
            io_since_previous = true;
        }
        if counters.in_flight > 0 {
            self.last_io = Some(now);
        }
        self.previous = Some((counters, now));
        let within_timeout = |last_io: Timestamp| {
            self.spindown_timeout.is_some_and(|timeout| {
                Duration::try_from(now.duration_since(last_io)).is_ok_and(|since| since < timeout)
            })
        };
        counters.in_flight > 0
            || (io_since_previous && self.spindown_timeout.is_none())
            || self.last_io.is_some_and(within_timeout)
    }
}

/// Whether a disk's I/O since cachy's own is others', as far as its I/O
/// counters tell. cachy's I/O can reach the disk after cachy is done with
/// it, such as when a filesystem writes its journal back, so the counters'
/// changes up to the first look after it, such as one once it has settled,
/// count as cachy's too.
#[derive(Default)]
pub struct OwnIo {
    settled: Option<Settled>,
}

/// Where a disk's I/O counters were once the changes from cachy's own I/O
/// had settled.
enum Settled {
    /// Not yet: the next look settles them.
    No,
    /// At this many requests completed.
    At(u64),
}

impl OwnIo {
    /// cachy did I/O on the disk, until now.
    pub fn did_io(&mut self) {
        self.settled = Some(Settled::No);
    }

    /// Looks at the disk's counters: whether its I/O since cachy's own, if
    /// any, is others'.
    pub fn look(&mut self, counters: Counters) -> bool {
        match self.settled {
            Some(Settled::No) => self.settled = Some(Settled::At(counters.requests)),
            Some(Settled::At(requests))
                if counters.requests != requests || counters.in_flight > 0 =>
            {
                self.settled = None;
            }
            _ => {}
        }
        self.others()
    }

    /// Whether the disk did I/O of others since cachy's own, or cachy did
    /// none, as of the last look.
    pub fn others(&self) -> bool {
        self.settled.is_none()
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
    fn without_a_spindown_timeout_io_since_the_previous_look_means_spun_up() {
        let mut disk = IoActivity::new(None);
        disk.spun_up(requests(10), at(0));
        // Looks five minutes apart, as the wake trigger's.
        assert!(disk.spun_up(requests(12), at(5)));
        assert!(!disk.spun_up(requests(12), at(10)));
        assert!(disk.spun_up(requests(13), at(15)));
    }

    #[test]
    fn the_first_look_tells_only_by_io_in_flight() {
        assert!(!IoActivity::new(None).spun_up(requests(10), at(0)));
        let busy = Counters {
            requests: 10,
            in_flight: 1,
        };
        assert!(IoActivity::new(None).spun_up(busy, at(0)));
    }

    #[test]
    fn with_a_spindown_timeout_io_since_a_look_within_it_means_spun_up() {
        let mut disk = IoActivity::new(Some(Duration::from_secs(20 * 60)));
        disk.spun_up(requests(10), at(0));
        assert!(disk.spun_up(requests(11), at(5)));
        assert!(disk.spun_up(requests(11), at(10)));
        // The I/O was between minutes 0 and 5: by minute 20, it may have
        // spun down.
        assert!(!disk.spun_up(requests(11), at(20)));
    }

    #[test]
    fn io_is_others_until_cachy_does_some() {
        let mut own_io = OwnIo::default();
        assert!(own_io.others());
        assert!(own_io.look(requests(10)));
        assert!(own_io.look(requests(10)));
    }

    #[test]
    fn cachys_own_io_and_what_follows_it_until_the_next_look_are_not_others() {
        let mut own_io = OwnIo::default();
        own_io.look(requests(10));
        own_io.did_io();
        assert!(!own_io.others());
        // cachy's writes, and the filesystem's writeback after it.
        assert!(!own_io.look(requests(500)));
        assert!(!own_io.look(requests(500)));
        assert!(!own_io.look(requests(500)));
        // Others' I/O.
        assert!(own_io.look(requests(501)));
        assert!(own_io.look(requests(501)));
    }

    #[test]
    fn io_in_flight_after_cachys_own_has_settled_is_others() {
        let mut own_io = OwnIo::default();
        own_io.did_io();
        assert!(!own_io.look(requests(500)));
        let busy = Counters {
            requests: 500,
            in_flight: 1,
        };
        assert!(own_io.look(busy));
    }

    #[test]
    fn io_in_flight_means_spun_up() {
        let mut disk = IoActivity::new(None);
        let busy = Counters {
            requests: 10,
            in_flight: 1,
        };
        assert!(disk.spun_up(busy, at(0)));
    }

    #[test]
    fn a_disk_is_spun_up_until_its_spindown_timeout_after_its_last_io() {
        let mut disk = IoActivity::new(Some(Duration::from_secs(20 * 60)));
        disk.spun_up(requests(10), at(0));
        assert!(disk.spun_up(requests(11), at(1)));
        assert!(disk.spun_up(requests(11), at(15)));
        // The I/O was between minutes 0 and 1: by minute 20, it may have spun
        // down.
        assert!(!disk.spun_up(requests(11), at(20)));
    }

    #[test]
    fn io_at_an_unknown_time_long_ago_does_not_mean_spun_up() {
        let mut disk = IoActivity::new(Some(Duration::from_secs(20 * 60)));
        disk.spun_up(requests(10), at(0));
        // Some I/O in the last six hours: it may have spun down since.
        assert!(!disk.spun_up(requests(50), at(6 * 60)));
        // More I/O within the last half minute.
        assert!(disk.spun_up(requests(51), at(6 * 60) + Duration::from_secs(30)));
    }
}
