//! The run summary: the JSON document that the run hook receives on stdin
//! after each run, as documented in docs/run-summary.md.

mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use common::{Fixture, assert_exit};

/// The keys of a run summary, as documented.
const KEYS: &[&str] = &[
    "schema_version",
    "cachy_version",
    "reason",
    "outcome",
    "cache_branch",
    "free_before",
    "free_after",
    "byte_budget",
    "moved",
    "skipped",
    "failed",
    "stale_copies",
    "moved_bytes",
    "duration_seconds",
    "throughput_bytes_per_second",
    "stopped",
    "unresolved_leftovers",
    "undated_dirs",
];

/// A run hook that saves the summary it receives to a file.
struct Recorder {
    dir: tempfile::TempDir,
}

impl Recorder {
    fn new() -> Recorder {
        Recorder {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("summary.json")
    }

    /// The config lines that set the run hook.
    fn config(&self) -> String {
        format!("run_hook:\n  command: cat > '{}'\n", self.path().display())
    }

    /// The summary the hook received, checked against the documented keys.
    fn summary(&self) -> Value {
        let text = fs::read_to_string(self.path()).expect("the run hook ran");
        let summary: Value = serde_json::from_str(&text).unwrap();
        let mut keys: Vec<&str> = summary
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        let mut documented = KEYS.to_vec();
        documented.sort();
        assert_eq!(keys, documented, "summary:\n{text}");
        summary
    }

    fn called(&self) -> bool {
        self.path().exists()
    }
}

fn path(p: &Path) -> Value {
    Value::from(p.to_str().unwrap())
}

#[test]
fn successful_run_summarises_each_moved_move_unit() {
    let fx = Fixture::new();
    let hook = Recorder::new();
    fx.cache_branch_file("a.txt", b"hello", 1_000);
    fx.cache_branch_file("dir/b.txt", b"hi", 2_000);

    assert_exit(&fx.run(&(fx.move_everything_config() + &hook.config())), 0);

    let summary = hook.summary();
    assert_eq!(summary["schema_version"], 1);
    assert_eq!(summary["cachy_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(summary["reason"], "manual");
    assert_eq!(summary["outcome"], "moved");
    assert_eq!(summary["cache_branch"], path(fx.cache_branch.path()));
    let free_before = summary["free_before"].as_u64().unwrap();
    assert!(summary["free_after"].as_u64().unwrap() > 0);
    assert_eq!(
        summary["byte_budget"].as_u64().unwrap(),
        (2u64 << 40) - free_before
    );
    let backing = path(fx.backing_branch.path());
    assert_eq!(
        summary["moved"],
        json!([
            {"paths": ["a.txt"], "branch": backing, "bytes": 5},
            {"paths": ["dir/b.txt"], "branch": backing, "bytes": 2},
        ])
    );
    assert_eq!(summary["skipped"], json!([]));
    assert_eq!(summary["failed"], json!([]));
    assert_eq!(summary["stale_copies"], json!([]));
    assert_eq!(summary["moved_bytes"], 7);
    assert!(summary["duration_seconds"].as_f64().unwrap() > 0.0);
    assert!(summary["throughput_bytes_per_second"].as_f64().unwrap() > 0.0);
    assert_eq!(summary["stopped"], false);
    assert_eq!(summary["unresolved_leftovers"], 0);
    assert_eq!(summary["undated_dirs"], 0);
}

#[test]
fn run_that_moves_nothing_summarises_each_skip_with_its_reason_code() {
    let fx = Fixture::new();
    let hook = Recorder::new();
    fx.cache_branch_file("busy.bin", b"busy", 1_000);
    let _writer = OpenOptions::new()
        .append(true)
        .open(fx.on_cache_branch("busy.bin"))
        .unwrap();

    assert_exit(&fx.run(&(fx.move_everything_config() + &hook.config())), 0);

    let summary = hook.summary();
    assert_eq!(summary["outcome"], "nothing_moved");
    assert_eq!(
        summary["skipped"],
        json!([{"paths": ["busy.bin"], "reason": "in_use"}])
    );
    assert_eq!(summary["moved"], json!([]));
    assert_eq!(summary["failed"], json!([]));
    assert_eq!(summary["moved_bytes"], 0);
    assert_eq!(summary["throughput_bytes_per_second"], 0.0);
}

#[test]
fn failed_run_summarises_each_failure_with_its_error() {
    let fx = Fixture::new();
    let hook = Recorder::new();
    let frozen = fx.extra_backing_branch();
    common::write_file(&frozen.path().join("a.bin"), b"stale", 1_000);
    fx.cache_branch_file("a.bin", b"on the cache branch", 2_000);
    fx.cache_branch_file("b.bin", b"b", 1_000);
    let layout = format!(
        "backing_branches:\n  - {}\n  - {}=NC\n",
        fx.backing_branch.path().display(),
        frozen.path().display()
    );

    assert_exit(&fx.run(&(fx.layout_config(&layout) + &hook.config())), 2);

    let summary = hook.summary();
    assert_eq!(summary["outcome"], "failed");
    assert_eq!(
        summary["failed"],
        json!([{
            "paths": ["a.bin"],
            "error": format!(
                "a stale copy of a.bin on backing branch {}, which takes no new files, cannot be removed",
                frozen.path().display()
            ),
        }])
    );
    assert_eq!(
        summary["moved"],
        json!([{"paths": ["b.bin"], "branch": path(fx.backing_branch.path()), "bytes": 1}])
    );
}

#[test]
fn drain_is_summarised_as_such_without_a_byte_budget() {
    let fx = Fixture::new();
    let hook = Recorder::new();
    fx.cache_branch_file("a.txt", b"a", 1_000);
    let mut command = fx.command(&(fx.no_work_config() + &hook.config()));
    command.arg("--drain");

    assert_exit(&common::output(&mut command).unwrap(), 0);

    let summary = hook.summary();
    assert_eq!(summary["reason"], "drain");
    assert_eq!(summary["outcome"], "moved");
    assert_eq!(summary["byte_budget"], Value::Null);
}

#[test]
fn check_that_finds_no_work_is_no_run_to_summarise() {
    let fx = Fixture::new();
    let hook = Recorder::new();
    fx.cache_branch_file("a.txt", b"a", 1_000);

    assert_exit(&fx.run(&(fx.no_work_config() + &hook.config())), 0);

    assert!(!hook.called());
}

#[test]
fn dry_run_is_not_summarised() {
    let fx = Fixture::new();
    let hook = Recorder::new();
    fx.cache_branch_file("a.txt", b"a", 1_000);
    let mut command = fx.command(&(fx.move_everything_config() + &hook.config()));
    command.arg("--dry-run");

    assert_exit(&common::output(&mut command).unwrap(), 0);

    assert!(!hook.called());
}

#[test]
fn stale_copies_that_moves_dealt_with_are_summarised_with_their_fate() {
    let fx = Fixture::new();
    let hook = Recorder::new();
    let other = fx.extra_backing_branch();
    let copies = [fx.on_backing_branch("a.bin"), other.path().join("a.bin")];
    for copy in &copies {
        common::write_file(copy, b"stale", 1_000);
    }
    fx.cache_branch_file("a.bin", b"on the cache branch", 2_000);
    let layout = format!(
        "backing_branches:\n  - {}\n  - {}\n",
        fx.backing_branch.path().display(),
        other.path().display()
    );

    assert_exit(&fx.run(&(fx.layout_config(&layout) + &hook.config())), 0);

    // Placement chooses either branch: the copy there is replaced, and the
    // other removed.
    let (replaced, removed) = if copies[0].exists() {
        (&copies[0], &copies[1])
    } else {
        (&copies[1], &copies[0])
    };
    assert_eq!(fs::read(replaced).unwrap(), b"on the cache branch");
    let summary = hook.summary();
    let mut stale_copies = summary["stale_copies"].as_array().unwrap().clone();
    stale_copies.sort_by_key(|copy| copy["fate"].as_str().unwrap().to_owned());
    assert_eq!(
        stale_copies,
        [
            json!({"path": path(removed), "fate": "removed"}),
            json!({"path": path(replaced), "fate": "replaced"}),
        ]
    );
}

/// A move that replaced a stale copy and was then rolled back did not bring
/// the stale copy back, so the summary still lists it.
#[test]
fn stale_copy_replaced_by_a_move_that_rolled_back_is_summarised() {
    let fx = Fixture::new();
    let hook = Recorder::new();
    fx.cache_branch_file("db/journal.log", &[7; 1 << 20], 1_000);
    fx.backing_branch_file("db/journal.log", b"stale", 500);
    let looked_up = rustix::fs::open(
        fx.on_cache_branch("db/journal.log"),
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    let child = fx.spawn(&(fx.move_everything_config() + &hook.config()));

    common::wait_for(&fx.staged_on_cache_branch("db/journal.log"));
    // Blocks on the lease until cachy rolls the move back.
    OpenOptions::new()
        .append(true)
        .open(format!("/proc/self/fd/{}", looked_up.as_raw_fd()))
        .unwrap()
        .write_all(b"appended")
        .unwrap();

    assert_exit(&child.wait_with_output().unwrap(), 0);
    let summary = hook.summary();
    assert_eq!(
        summary["skipped"],
        json!([{"paths": ["db/journal.log"], "reason": "in_use"}])
    );
    assert_eq!(
        summary["stale_copies"],
        json!([{"path": path(&fx.on_backing_branch("db/journal.log")), "fate": "replaced"}])
    );
}
