//! The daemon's clock: what time it is, and waiting for a time to come.
//!
//! The daemon takes its clock as a [`Clock`], so that a fault-injection build
//! can hand it one that tests set (`fault::ManualClock`) instead of the
//! system clock.

use std::thread;
use std::time::Duration;

use jiff::{Timestamp, Zoned};

use crate::stop;

/// The longest the system clock sleeps before it looks at the time and for a
/// stop request again. The wall clock may jump, such as after a suspend.
const SLICE: Duration = Duration::from_millis(500);

pub trait Clock {
    /// The current time, in the system's time zone.
    fn now(&self) -> Zoned;

    /// Waits until `time`; `false` if cachy was asked to stop first.
    fn sleep_until(&self, time: Timestamp) -> bool;
}

/// The system's clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Zoned {
        Zoned::now()
    }

    fn sleep_until(&self, time: Timestamp) -> bool {
        sleep_until(time, SLICE, Timestamp::now)
    }
}

/// Waits until `now()` reaches `time`, looking every `slice` at most; `false`
/// if cachy was asked to stop first.
pub fn sleep_until(time: Timestamp, slice: Duration, now: impl Fn() -> Timestamp) -> bool {
    loop {
        if stop::requested() {
            return false;
        }
        let left = time.duration_since(now());
        if left.is_zero() || left.is_negative() {
            return true;
        }
        let left = Duration::try_from(left).unwrap_or(slice);
        thread::sleep(left.min(slice));
    }
}
