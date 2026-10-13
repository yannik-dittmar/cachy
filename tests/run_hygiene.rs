//! Run hygiene: one run per cache branch at a time, removal of the
//! directories that a run emptied, and a clean stop on `SIGTERM`.

mod common;

use common::{Fixture, assert_exit};

/// A move unit large enough that a fault-injection build, told to slow every
/// write down, copies it for a few seconds: one write per MiB.
#[cfg(feature = "fault-injection")]
const SLOW_FILE: &str = "slow.bin";

/// Starts a run that moves [`SLOW_FILE`] slowly under `config`, and returns
/// once it is copying it.
#[cfg(feature = "fault-injection")]
fn spawn_slow_run(fx: &Fixture, config: &str, delay_ms: &str) -> std::process::Child {
    fx.cache_branch_file(SLOW_FILE, &vec![0xa5; 4 << 20], 1_000);
    let mut command = fx.command(config);
    command.env("CACHY_WRITE_DELAY_MS", delay_ms);
    let child = common::spawn(&mut command).unwrap();
    common::wait_for(&fx.staged_on_backing_branch(SLOW_FILE));
    child
}

#[cfg(feature = "fault-injection")]
#[test]
fn second_run_on_the_same_cache_branch_is_refused() {
    let fx = Fixture::new();
    let config = fx.move_everything_config();
    let first = spawn_slow_run(&fx, &config, "500");

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("another cachy run"), "stderr:\n{err}");
    // The refused run left the first one's move alone.
    assert_exit(&first.wait_with_output().unwrap(), 0);
    assert!(fx.on_backing_branch(SLOW_FILE).exists());
    assert!(!fx.on_cache_branch(SLOW_FILE).exists());
}

#[cfg(feature = "fault-injection")]
#[test]
fn runs_on_different_cache_branches_run_at_once() {
    let state_dir = tempfile::tempdir().unwrap();
    let state = format!("state_dir: {}\n", state_dir.path().display());
    let (fx, other) = (Fixture::new(), Fixture::new());
    other.cache_branch_file("a.txt", b"a", 1_000);
    let first = spawn_slow_run(&fx, &(fx.move_everything_config() + &state), "500");

    assert_exit(&other.run(&(other.move_everything_config() + &state)), 0);

    assert!(other.on_backing_branch("a.txt").exists());
    assert_exit(&first.wait_with_output().unwrap(), 0);
    assert!(fx.on_backing_branch(SLOW_FILE).exists());
}

#[test]
fn directories_that_the_run_emptied_are_removed() {
    let fx = Fixture::new();
    fx.cache_branch_file("a/b/c/f.txt", b"f", 1_000);
    fx.cache_branch_file("d/g.txt", b"g", 1_000);
    fx.cache_branch_file("d/h.txt", b"h", 1_000);

    assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert_eq!(Fixture::tree(fx.cache_branch.path()), Vec::<String>::new());
    assert!(fx.on_backing_branch("a/b/c/f.txt").exists());
    assert!(fx.on_backing_branch("d/h.txt").exists());
}

#[test]
fn removal_stops_at_directories_that_were_empty_or_are_excluded() {
    let fx = Fixture::new();
    fx.cache_branch_file("media/a.mkv", b"a", 1_000);
    fx.cache_branch_file("media/b.part", b"b", 1_000);
    fx.cache_branch_file("shows/s1/e1.mkv", b"e", 1_000);
    std::fs::create_dir_all(fx.on_cache_branch("shows/s2")).unwrap();
    std::fs::create_dir_all(fx.on_cache_branch("empty/inner")).unwrap();
    std::fs::create_dir_all(fx.on_cache_branch("incoming")).unwrap();
    let config = fx.move_everything_config() + "exclude:\n  - '*.part'\n  - /incoming/\n";

    assert_exit(&fx.run(&config), 0);

    assert_eq!(
        Fixture::tree(fx.cache_branch.path()),
        [
            "empty",
            "empty/inner",
            "incoming",
            "media",
            "media/b.part",
            "shows",
            "shows/s2"
        ]
    );
}

#[test]
fn keep_empty_dirs_keeps_every_directory() {
    let fx = Fixture::new();
    fx.cache_branch_file("a/b/f.txt", b"f", 1_000);
    let config = fx.move_everything_config() + "keep_empty_dirs: true\n";

    assert_exit(&fx.run(&config), 0);

    assert_eq!(Fixture::tree(fx.cache_branch.path()), ["a", "a/b"]);
    assert!(fx.on_backing_branch("a/b/f.txt").exists());
}

#[cfg(feature = "fault-injection")]
#[test]
fn sigterm_during_a_copy_stops_the_run_promptly_and_cleanly() {
    use std::time::{Duration, Instant};

    use rustix::process::{Pid, Signal, kill_process};

    let fx = Fixture::new();
    fx.cache_branch_file("later.txt", b"later", 2_000);
    // 4 writes of a second each: the copy alone would take 4 s.
    let run = spawn_slow_run(&fx, &fx.move_everything_config(), "1000");
    let signalled = Instant::now();

    kill_process(Pid::from_child(&run), Signal::TERM).unwrap();
    let out = run.wait_with_output().unwrap();

    assert!(
        signalled.elapsed() < Duration::from_secs(3),
        "took {:?} to stop",
        signalled.elapsed()
    );
    let err = assert_exit(&out, 2);
    assert!(err.contains("stopping"), "stderr:\n{err}");
    assert_eq!(
        std::fs::read(fx.on_cache_branch(SLOW_FILE)).unwrap(),
        vec![0xa5; 4 << 20]
    );
    assert!(fx.on_cache_branch("later.txt").exists());
    assert_eq!(
        Fixture::tree(fx.backing_branch.path()),
        Vec::<String>::new()
    );
    fx.assert_no_staging_directories();
    // The next run finds nothing to recover.
    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);
    assert!(err.contains("2 moved"), "stderr:\n{err}");
}
