//! The run summary: what a run did, as the JSON document that the run hook
//! receives. docs/run-summary.md documents its schema; a change to it that
//! breaks a reader bumps [`SCHEMA_VERSION`].

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::transaction::{Fate, Skip};

/// The version of the summary's schema.
const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
pub struct Summary {
    schema_version: u32,
    cachy_version: &'static str,
    reason: Reason,
    pub outcome: RunOutcome,
    cache_branch: String,
    /// Free space on the cache branch when the run started, if measured.
    pub free_before: Option<u64>,
    /// Free space on the cache branch when the run ended, if measured.
    pub free_after: Option<u64>,
    /// `None` for a drain, which has none.
    pub byte_budget: Option<u64>,
    pub moved: Vec<MovedUnit>,
    pub skipped: Vec<SkippedUnit>,
    pub failed: Vec<FailedUnit>,
    pub stale_copies: Vec<StaleCopy>,
    /// The sizes (`st_size`) of the moved move units, added up.
    moved_bytes: u64,
    /// From the start of the run, the capability probe included, to its
    /// end.
    duration_seconds: f64,
    /// `moved_bytes` per second of moving, which leaves out the steps before
    /// the first move and after the last.
    throughput_bytes_per_second: f64,
    pub stopped: bool,
    pub unresolved_leftovers: usize,
    pub undated_dirs: usize,
}

/// Why a run started.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// `cachy run`: a check found work.
    Manual,
    /// `cachy run --drain`.
    Drain,
    /// `cachy daemon`: a scheduled check found work.
    Schedule,
    /// `cachy daemon`: a wake-triggered check found work.
    WakeTrigger,
}

/// How a run ended, which the run hook's `on` filter selects by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// Nothing failed, and at least one move unit moved.
    Moved,
    /// Nothing failed, and nothing moved.
    NothingMoved,
    /// A move failed, or the run stopped early or left something unresolved:
    /// cachy exits with code 2.
    Failed,
}

#[derive(Serialize)]
pub struct MovedUnit {
    /// The move unit's relpaths: one, or each name of a hardlink group.
    paths: Vec<String>,
    /// The backing branch that it moved to.
    branch: String,
    /// Its size (`st_size`).
    bytes: u64,
}

#[derive(Serialize)]
pub struct SkippedUnit {
    paths: Vec<String>,
    reason: Skip,
}

#[derive(Serialize)]
pub struct FailedUnit {
    paths: Vec<String>,
    error: String,
}

#[derive(Serialize)]
pub struct StaleCopy {
    path: String,
    fate: Fate,
}

impl Summary {
    pub fn new(reason: Reason, cache_branch: &Path) -> Summary {
        Summary {
            schema_version: SCHEMA_VERSION,
            cachy_version: env!("CARGO_PKG_VERSION"),
            reason,
            outcome: RunOutcome::NothingMoved,
            cache_branch: text(cache_branch),
            free_before: None,
            free_after: None,
            byte_budget: None,
            moved: Vec::new(),
            skipped: Vec::new(),
            failed: Vec::new(),
            stale_copies: Vec::new(),
            moved_bytes: 0,
            duration_seconds: 0.0,
            throughput_bytes_per_second: 0.0,
            stopped: false,
            unresolved_leftovers: 0,
            undated_dirs: 0,
        }
    }

    /// Completes the summary of a run that took `duration`, of which it
    /// spent `moving` on moves, and that `succeeded` or not.
    pub fn finish(&mut self, succeeded: bool, duration: Duration, moving: Duration) {
        self.moved_bytes = self.moved.iter().map(|unit| unit.bytes).sum();
        self.duration_seconds = duration.as_secs_f64();
        self.throughput_bytes_per_second = match moving.as_secs_f64() {
            0.0 => 0.0,
            seconds => self.moved_bytes as f64 / seconds,
        };
        self.outcome = if !succeeded {
            RunOutcome::Failed
        } else if self.moved.is_empty() {
            RunOutcome::NothingMoved
        } else {
            RunOutcome::Moved
        };
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a summary serializes")
    }
}

impl MovedUnit {
    pub fn new(rels: &[impl AsRef<Path>], branch: &Path, bytes: u64) -> MovedUnit {
        MovedUnit {
            paths: texts(rels),
            branch: text(branch),
            bytes,
        }
    }
}

impl SkippedUnit {
    pub fn new(rels: &[impl AsRef<Path>], reason: Skip) -> SkippedUnit {
        SkippedUnit {
            paths: texts(rels),
            reason,
        }
    }
}

impl FailedUnit {
    pub fn new(rels: &[impl AsRef<Path>], error: String) -> FailedUnit {
        FailedUnit {
            paths: texts(rels),
            error,
        }
    }
}

impl StaleCopy {
    pub fn new(path: &Path, fate: Fate) -> StaleCopy {
        StaleCopy {
            path: text(path),
            fate,
        }
    }
}

/// A path as JSON text, which must be Unicode: bytes that are not UTF-8
/// become U+FFFD.
fn text(path: impl AsRef<Path>) -> String {
    path.as_ref().to_string_lossy().into_owned()
}

fn texts(paths: &[impl AsRef<Path>]) -> Vec<String> {
    paths.iter().map(text).collect()
}
