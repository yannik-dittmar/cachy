//! Log lines on stderr: `cachy: <level>: <message>`, one per event, never
//! coloured. When stderr is connected to the journal, as systemd connects a
//! service's, each line starts with its syslog priority (`<3>` for an error),
//! which journald strips and records as the entry's priority.

use std::fmt;
use std::io::Write;
use std::os::fd::AsFd;
use std::sync::OnceLock;

#[derive(Clone, Copy)]
pub enum Level {
    Error,
    Warning,
    Info,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warning => "warning",
            Level::Info => "info",
        }
    }

    /// The syslog priority: `LOG_ERR`, `LOG_WARNING` or `LOG_INFO`.
    fn priority(self) -> u8 {
        match self {
            Level::Error => 3,
            Level::Warning => 4,
            Level::Info => 6,
        }
    }
}

/// Writes one log line at `level`.
pub fn emit(level: Level, message: fmt::Arguments) {
    let line = if to_journal() {
        format!("<{}>cachy: {}: {message}\n", level.priority(), level.name())
    } else {
        format!("cachy: {}: {message}\n", level.name())
    };
    // One write per line, so that lines from different threads never mix.
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

/// Whether stderr is connected to the journal: systemd then sets
/// `JOURNAL_STREAM` to the device and inode numbers of the stream, which
/// stderr keeps unless something redirected it.
fn to_journal() -> bool {
    static TO_JOURNAL: OnceLock<bool> = OnceLock::new();
    *TO_JOURNAL.get_or_init(|| {
        let Some(stream) = std::env::var_os("JOURNAL_STREAM") else {
            return false;
        };
        let Some((dev, ino)) = stream.to_str().and_then(|s| s.split_once(':')) else {
            return false;
        };
        match rustix::fs::fstat(std::io::stderr().as_fd()) {
            Ok(stat) => dev.parse() == Ok(stat.st_dev) && ino.parse() == Ok(stat.st_ino),
            Err(_) => false,
        }
    })
}

macro_rules! error {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Error, format_args!($($arg)*))
    };
}

macro_rules! warning {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Warning, format_args!($($arg)*))
    };
}

macro_rules! info {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Info, format_args!($($arg)*))
    };
}

pub(crate) use {error, info, warning};
