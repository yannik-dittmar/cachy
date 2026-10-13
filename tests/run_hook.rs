//! The run hook: a configured command that cachy executes after each run,
//! with the run summary on stdin.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{Fixture, assert_exit};

/// A scratch directory for what hooks leave behind.
struct Marks {
    dir: tempfile::TempDir,
}

impl Marks {
    fn new() -> Marks {
        Marks {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

/// Config lines that set a run hook executing `command`, with the further
/// lines `extra` (indented as keys of `run_hook`).
fn hook(command: &str, extra: &str) -> String {
    format!("run_hook:\n  command: \"{command}\"\n{extra}")
}

#[test]
fn hook_runs_after_runs_whose_outcome_its_filter_selects() {
    let fx = Fixture::new();
    let marks = Marks::new();
    fx.cache_branch_file("a.txt", b"a", 1_000);
    let mark = marks.path("called");
    let config = fx.move_everything_config()
        + &hook(
            &format!("touch '{}'", mark.display()),
            "  on: [moved, failed]\n",
        );

    assert_exit(&fx.run(&config), 0);

    assert!(mark.exists());
}

#[test]
fn hook_does_not_run_after_runs_whose_outcome_its_filter_leaves_out() {
    let fx = Fixture::new();
    let marks = Marks::new();
    fx.cache_branch_file("a.txt", b"a", 1_000);
    let mark = marks.path("called");
    let config = fx.move_everything_config()
        + &hook(
            &format!("touch '{}'", mark.display()),
            "  on: [nothing_moved, failed]\n",
        );

    assert_exit(&fx.run(&config), 0);

    assert!(fx.on_backing_branch("a.txt").exists());
    assert!(!mark.exists());
}

#[test]
fn failing_hook_is_logged_and_leaves_the_exit_code_alone() {
    let fx = Fixture::new();
    fx.cache_branch_file("a.txt", b"a", 1_000);
    let config = fx.move_everything_config() + &hook("cat >/dev/null; exit 3", "");

    let err = assert_exit(&fx.run(&config), 0);

    assert!(
        err.contains("cachy: error: run hook failed: `cat >/dev/null; exit 3` exited with code 3"),
        "stderr:\n{err}"
    );
}

/// Creates move units enough that their run summary overflows a pipe's
/// buffer, so that writing it blocks until the hook reads it.
fn large_summary(fx: &Fixture) {
    let dir = vec!["d".repeat(250); 4].join("/");
    for i in 0..80 {
        fx.cache_branch_file(&format!("{dir}/file-{i:03}.txt"), b"a", 1_000);
    }
}

#[test]
fn hook_may_exit_without_reading_its_stdin() {
    let fx = Fixture::new();
    large_summary(&fx);
    let config = fx.move_everything_config() + &hook("true", "");

    let err = assert_exit(&fx.run(&config), 0);

    assert!(!err.contains("run hook"), "stderr:\n{err}");
}

#[test]
fn hook_exceeding_its_timeout_is_killed_with_what_it_started() {
    let fx = Fixture::new();
    large_summary(&fx);
    // Reads none of its stdin, and leaves a child running.
    let config = fx.move_everything_config() + &hook("sleep 60 & wait", "  timeout: 1s\n");
    let start = Instant::now();

    // The child sleep holds cachy's stdout and stderr open: unless it is
    // killed too, collecting them waits for it.
    let err = assert_exit(&fx.run(&config), 0);

    assert!(
        start.elapsed() < Duration::from_secs(40),
        "{:?}",
        start.elapsed()
    );
    assert!(
        err.contains(
            "cachy: error: run hook failed: `sleep 60 & wait` was killed after its timeout of 1s"
        ),
        "stderr:\n{err}"
    );
}

#[test]
fn hook_timeout_must_be_above_zero() {
    let fx = Fixture::new();
    let config = fx.move_everything_config() + &hook("true", "  timeout: 0s\n");

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("run_hook: timeout"), "stderr:\n{err}");
}
