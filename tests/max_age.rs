//! The max age: move units unchanged for longer than it count as work for any
//! check, whatever the free space, and a run moves them first.
//!
//! A test file's ctime is always fresh and cannot be set back, so these tests
//! use a max age of one second and wait for files to pass it.

mod common;

use std::fs;
use std::thread;
use std::time::Duration;

use common::{Fixture, assert_exit, free_space_lock, output, set_times};

const MAX_AGE: &str = "max_age: 1s\n";
const MIB: usize = 1 << 20;

/// Waits until files written before the call are past [`MAX_AGE`].
fn wait_past_max_age() {
    thread::sleep(Duration::from_millis(1_500));
}

/// `overdue.bin`, past the max age, then `fresh.bin`, which is not.
fn overdue_and_fresh(fx: &Fixture) {
    fx.cache_branch_file("overdue.bin", b"overdue", 1_000);
    wait_past_max_age();
    fx.cache_branch_file("fresh.bin", b"fresh", 1_000);
}

#[test]
fn move_units_past_the_max_age_move_although_free_space_is_above_the_threshold() {
    let fx = Fixture::new();
    overdue_and_fresh(&fx);

    let err = assert_exit(&fx.run(&(fx.no_work_config() + MAX_AGE)), 0);

    assert!(err.contains("1 moved, 0 skipped"), "stderr:\n{err}");
    assert_eq!(
        fs::read(fx.on_backing_branch("overdue.bin")).unwrap(),
        b"overdue"
    );
    assert!(!fx.on_cache_branch("overdue.bin").exists());
    assert_eq!(fs::read(fx.on_cache_branch("fresh.bin")).unwrap(), b"fresh");
    assert!(!fx.on_backing_branch("fresh.bin").exists());
}

#[test]
fn a_check_without_move_units_past_the_max_age_finds_no_work() {
    let fx = Fixture::new();
    fx.cache_branch_file("fresh.bin", b"fresh", 1_000);

    let out = output(
        fx.command(&(fx.no_work_config() + "max_age: 1d\n"))
            .arg("--dry-run"),
    )
    .unwrap();

    assert_exit(&out, 0);
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "byte budget: none (free space is at or above the threshold)\n"
    );
}

#[test]
fn dry_run_lists_only_move_units_past_the_max_age_when_free_space_is_above_the_threshold() {
    let fx = Fixture::new();
    overdue_and_fresh(&fx);

    let out = output(
        fx.command(&(fx.no_work_config() + MAX_AGE))
            .arg("--dry-run"),
    )
    .unwrap();

    assert_exit(&out, 0);
    let planned = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        planned.lines().collect::<Vec<_>>(),
        ["byte budget: 0 bytes", "would move overdue.bin"],
        "stdout:\n{planned}"
    );
}

#[test]
fn move_units_past_the_max_age_are_moved_first_and_all_of_them_beyond_the_byte_budget() {
    let _lock = free_space_lock();
    let fx = Fixture::new();
    // Least recently accessed last, so that without a max age they would
    // move last. Large enough that other tests' writes to the cache branch's
    // filesystem cannot grow the byte budget past them.
    let content = |rel: &str| rel.repeat(MIB / rel.len());
    for rel in ["overdue/a.bin", "overdue/b.bin"] {
        fx.cache_branch_file(rel, content(rel).as_bytes(), 1_000);
        set_times(&fx.on_cache_branch(rel), 9_000, 1_000);
    }
    wait_past_max_age();
    fx.cache_branch_file("fresh.bin", b"fresh", 1_000);
    let free = fx.cache_branch_free();
    // A byte budget that the first move consumes.
    let config = fx.config(&(free + 1).to_string(), &(free + 2).to_string()) + MAX_AGE;

    let out = output(fx.command(&config).arg("--dry-run")).unwrap();
    assert_exit(&out, 0);
    let planned: Vec<_> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("would move ").map(str::to_owned))
        .collect();
    assert_eq!(planned, ["overdue/a.bin", "overdue/b.bin"]);

    let err = assert_exit(&fx.run(&config), 0);

    assert!(err.contains("2 moved, 0 skipped"), "stderr:\n{err}");
    for rel in ["overdue/a.bin", "overdue/b.bin"] {
        assert_eq!(
            fs::read(fx.on_backing_branch(rel)).unwrap(),
            content(rel).as_bytes()
        );
    }
    assert_eq!(fs::read(fx.on_cache_branch("fresh.bin")).unwrap(), b"fresh");
}

#[test]
fn the_command_line_max_age_overrides_the_configured_one() {
    let fx = Fixture::new();
    overdue_and_fresh(&fx);

    let mut command = fx.command(&(fx.no_work_config() + "max_age: 1d\n"));
    let err = assert_exit(&output(command.args(["--max-age", "1s"])).unwrap(), 0);

    assert!(err.contains("1 moved, 0 skipped"), "stderr:\n{err}");
    assert!(fx.on_backing_branch("overdue.bin").exists());
    assert!(fx.on_cache_branch("fresh.bin").exists());
}

#[test]
fn the_command_line_max_age_applies_without_a_configured_one() {
    let fx = Fixture::new();
    overdue_and_fresh(&fx);

    let mut command = fx.command(&fx.no_work_config());
    let err = assert_exit(&output(command.args(["--max-age", "1s"])).unwrap(), 0);

    assert!(err.contains("1 moved, 0 skipped"), "stderr:\n{err}");
    assert!(fx.on_backing_branch("overdue.bin").exists());
}

#[test]
fn max_age_not_above_the_minimum_age_is_refused() {
    let fx = Fixture::new();
    for (minimum_age, max_age) in [("0s", "0s"), ("15m", "15m"), ("1h", "30m"), ("1d", "24h")] {
        let config = fx
            .no_work_config()
            .replace("minimum_age: 0s", &format!("minimum_age: {minimum_age}"))
            + &format!("max_age: {max_age}\n");

        let err = assert_exit(&fx.run(&config), 1);

        assert!(err.contains("max_age"), "stderr:\n{err}");
        assert!(err.contains("above the minimum age"), "stderr:\n{err}");
    }
}

#[test]
fn max_age_not_above_the_default_minimum_age_is_refused() {
    let fx = Fixture::new();
    let config = fx.no_work_config().replace("minimum_age: 0s\n", "") + "max_age: 10m\n";

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("above the minimum age"), "stderr:\n{err}");
}

#[test]
fn command_line_max_age_not_above_the_minimum_age_is_refused() {
    let fx = Fixture::new();
    let config = fx
        .no_work_config()
        .replace("minimum_age: 0s", "minimum_age: 1h");

    let mut command = fx.command(&config);
    let err = assert_exit(&output(command.args(["--max-age", "1h"])).unwrap(), 1);

    assert!(err.contains("--max-age"), "stderr:\n{err}");
    assert!(err.contains("above the minimum age"), "stderr:\n{err}");
}

#[test]
fn invalid_command_line_max_age_is_refused() {
    let fx = Fixture::new();

    let mut command = fx.command(&fx.no_work_config());
    let err = assert_exit(&output(command.args(["--max-age", "soon"])).unwrap(), 1);

    assert!(err.contains("--max-age"), "stderr:\n{err}");
}

/// Without a max age, a check that finds no work reads free space and walks
/// nothing; with one, it walks the cache branch. The fault-injection build
/// traces every filesystem operation.
#[cfg(feature = "fault-injection")]
mod walk {
    use super::*;

    /// The names of the operations that a run with `config` traces.
    fn traced_operations(fx: &Fixture, config: &str) -> Vec<String> {
        let out = output(fx.command(config).env("CACHY_FAULT_TRACE", "1")).unwrap();
        let err = assert_exit(&out, 0);
        common::traced_operations(&err)
            .into_iter()
            .map(|(_, name)| name.to_owned())
            .collect()
    }

    #[test]
    fn a_check_without_a_max_age_walks_no_directory() {
        let fx = Fixture::new();
        fx.cache_branch_file("dir/file.bin", b"content", 1_000);

        let ops = traced_operations(&fx, &fx.no_work_config());

        assert!(
            !ops.iter().any(|op| op == "(list_dir)"),
            "operations: {ops:?}"
        );
        assert!(
            ops.iter().any(|op| op == "(stat_fs)"),
            "operations: {ops:?}"
        );
    }

    #[test]
    fn a_check_with_a_max_age_walks_the_cache_branch() {
        let fx = Fixture::new();
        fx.cache_branch_file("dir/file.bin", b"content", 1_000);

        let ops = traced_operations(&fx, &(fx.no_work_config() + "max_age: 1d\n"));

        assert!(
            ops.iter().any(|op| op == "(list_dir)"),
            "operations: {ops:?}"
        );
    }
}
