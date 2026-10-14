//! `cachy daemon`: checks at the times that the configured schedules give,
//! each of them a run if it finds work, under the same instance lock as
//! `cachy run`, until `SIGTERM` stops the daemon.
//!
//! A fault-injection build reads the daemon's time from a file that the test
//! sets (see `common::Daemon`), so that the tests need not wait for real
//! minutes to pass.

mod common;

use common::{Fixture, assert_exit};

/// The line that the daemon logs once it waits for its next check.
const WAITING: &str = "next scheduled check at";

#[test]
fn an_invalid_schedule_is_a_config_error() {
    let fx = Fixture::new();
    let config = fx.move_everything_config() + "daemon:\n  schedules:\n    - '61 * * * *'\n";

    let err = assert_exit(&common::output(&mut fx.daemon_command(&config)).unwrap(), 1);
    assert!(err.contains("invalid config"), "stderr:\n{err}");
    assert!(
        err.contains("invalid schedule `61 * * * *`"),
        "stderr:\n{err}"
    );

    // The config is one file: `cachy run` refuses it too.
    let err = assert_exit(&fx.run(&config), 1);
    assert!(err.contains("invalid schedule"), "stderr:\n{err}");
}

#[test]
fn a_daemon_without_schedules_refuses_to_start() {
    let fx = Fixture::new();

    for config in [
        fx.move_everything_config(),
        fx.move_everything_config() + "daemon:\n  schedules: []\n",
    ] {
        let out = common::output(&mut fx.daemon_command(&config)).unwrap();
        let err = assert_exit(&out, 1);
        assert!(err.contains("daemon.schedules"), "stderr:\n{err}");
    }
}

/// On the system clock, in a build without fault injection, as well.
#[test]
fn sigterm_while_waiting_stops_the_daemon() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let config = scheduled_config(&fx, &["0 3 * * *"], &dir.path().join("summaries"));
    let daemon = fx.start_daemon(&config, "2026-01-01T02:59:00Z");
    daemon.wait_for_lines(WAITING, 1);

    let (status, err) = daemon.stop();

    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
    assert!(err.contains("daemon stopped"), "stderr:\n{err}");
    assert!(!err.contains("scheduled check ("), "stderr:\n{err}");
}

/// A config of `fx` that moves everything, with the given schedules and a run
/// hook that appends each run summary, one per line, to `summaries`.
fn scheduled_config(fx: &Fixture, schedules: &[&str], summaries: &std::path::Path) -> String {
    let mut config = fx.move_everything_config();
    config += &format!(
        "run_hook:\n  command: 'cat >> {}; echo >> {}'\n",
        summaries.display(),
        summaries.display()
    );
    config += "daemon:\n  schedules:\n";
    for schedule in schedules {
        config += &format!("    - '{schedule}'\n");
    }
    config
}

/// The run summaries that the hook of [`scheduled_config`] saved.
#[cfg(feature = "fault-injection")]
fn summaries(path: &std::path::Path) -> Vec<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_scheduled_check_at_its_time_starts_a_run_with_reason_schedule() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let summaries_path = dir.path().join("summaries");
    fx.cache_branch_file("a.txt", b"a", 1_000);
    let config = scheduled_config(&fx, &["0 3 * * *"], &summaries_path);

    let daemon = fx.start_daemon(&config, "2026-01-01T02:59:00Z");
    daemon.wait_for_lines(&format!("{WAITING} 2026-01-01 03:00"), 1);
    // Not yet its time.
    assert!(fx.on_cache_branch("a.txt").exists());

    daemon.set_clock("2026-01-01T03:00:00Z");
    daemon.wait_for_lines(&format!("{WAITING} 2026-01-02 03:00"), 1);

    assert!(fx.on_backing_branch("a.txt").exists());
    assert!(!fx.on_cache_branch("a.txt").exists());
    let summaries = summaries(&summaries_path);
    assert_eq!(summaries.len(), 1, "{summaries:?}");
    assert_eq!(summaries[0]["reason"], "schedule");
    assert_eq!(summaries[0]["outcome"], "moved");
    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
}

#[cfg(feature = "fault-injection")]
#[test]
fn the_daemon_checks_at_each_time_of_each_schedule_and_runs_only_on_work() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let summaries_path = dir.path().join("summaries");
    fx.cache_branch_file("a.txt", b"a", 1_000);
    // The checks find the cache branch's free space above the threshold.
    let config = scheduled_config(&fx, &["0 3 * * *", "30 * * * *"], &summaries_path)
        .replace("threshold: 1T\ntarget: 2T", "threshold: 0\ntarget: 1");

    let daemon = fx.start_daemon(&config, "2026-01-01T02:59:00Z");
    daemon.wait_for_lines(&format!("{WAITING} 2026-01-01 03:00"), 1);
    daemon.set_clock("2026-01-01T03:00:00Z");
    daemon.wait_for_lines(&format!("{WAITING} 2026-01-01 03:30"), 1);
    daemon.set_clock("2026-01-01T03:30:00Z");
    daemon.wait_for_lines(&format!("{WAITING} 2026-01-01 04:30"), 1);

    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
    assert_eq!(
        err.matches("scheduled check (`").count(),
        2,
        "stderr:\n{err}"
    );
    assert_eq!(err.matches("nothing to do").count(), 2, "stderr:\n{err}");
    // A check that finds no work is not a run.
    assert!(summaries(&summaries_path).is_empty());
    assert!(fx.on_cache_branch("a.txt").exists());
}

/// A move unit large enough that a fault-injection build, told to slow every
/// write down (`CACHY_WRITE_DELAY_MS`), copies it for a few seconds: one write
/// per MiB.
#[cfg(feature = "fault-injection")]
const SLOW_FILE: &str = "slow.bin";

#[cfg(feature = "fault-injection")]
fn slow_file(fx: &Fixture) {
    fx.cache_branch_file(SLOW_FILE, &vec![0xa5; 4 << 20], 1_000);
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_scheduled_check_during_a_manual_run_is_skipped_and_the_daemon_goes_on() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let summaries_path = dir.path().join("summaries");
    let config = scheduled_config(&fx, &["0 * * * *"], &summaries_path);
    let daemon = fx.start_daemon(&config, "2026-01-01T02:59:00Z");
    daemon.wait_for_lines(WAITING, 1);
    slow_file(&fx);
    let mut manual = fx.command(&fx.move_everything_config());
    manual.env("CACHY_WRITE_DELAY_MS", "500");
    let manual = common::spawn(&mut manual).unwrap();
    common::wait_for(&fx.staged_on_backing_branch(SLOW_FILE));

    daemon.set_clock("2026-01-01T03:00:00Z");
    daemon.wait_for_lines(WAITING, 2);

    let err = daemon.stderr();
    assert!(
        err.contains("warning: skipped the scheduled check: another cachy run"),
        "stderr:\n{err}"
    );
    // The manual run went on undisturbed.
    assert_exit(&manual.wait_with_output().unwrap(), 0);
    assert!(fx.on_backing_branch(SLOW_FILE).exists());
    assert!(summaries(&summaries_path).is_empty());
    // The daemon's next check runs.
    fx.cache_branch_file("a.txt", b"a", 1_000);
    daemon.set_clock("2026-01-01T04:00:00Z");
    daemon.wait_for_lines(WAITING, 3);
    assert!(fx.on_backing_branch("a.txt").exists());
    assert_eq!(summaries(&summaries_path).len(), 1);
    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_manual_run_during_a_scheduled_run_is_refused() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let config = scheduled_config(&fx, &["0 3 * * *"], &dir.path().join("summaries"));
    slow_file(&fx);
    let daemon = fx.start_daemon_with(
        &config,
        "2026-01-01T02:59:00Z",
        &[("CACHY_WRITE_DELAY_MS", "500")],
    );
    daemon.wait_for_lines(WAITING, 1);
    daemon.set_clock("2026-01-01T03:00:00Z");
    common::wait_for(&fx.staged_on_backing_branch(SLOW_FILE));

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 1);

    assert!(err.contains("another cachy run"), "stderr:\n{err}");
    // The scheduled run went on undisturbed.
    daemon.wait_for_lines(WAITING, 2);
    assert!(fx.on_backing_branch(SLOW_FILE).exists());
    assert!(!fx.on_cache_branch(SLOW_FILE).exists());
    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
}

#[cfg(feature = "fault-injection")]
#[test]
fn sigterm_during_a_scheduled_run_stops_the_daemon_promptly_and_cleanly() {
    use std::time::{Duration, Instant};

    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let config = scheduled_config(&fx, &["0 3 * * *"], &dir.path().join("summaries"));
    slow_file(&fx);
    // 4 writes of a second each: the copy alone would take 4 s.
    let daemon = fx.start_daemon_with(
        &config,
        "2026-01-01T02:59:00Z",
        &[("CACHY_WRITE_DELAY_MS", "1000")],
    );
    daemon.wait_for_lines(WAITING, 1);
    daemon.set_clock("2026-01-01T03:00:00Z");
    common::wait_for(&fx.staged_on_backing_branch(SLOW_FILE));
    let signalled = Instant::now();

    let (status, err) = daemon.stop();

    assert!(
        signalled.elapsed() < Duration::from_secs(3),
        "took {:?} to stop",
        signalled.elapsed()
    );
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
    assert!(err.contains("run stopped early"), "stderr:\n{err}");
    assert!(err.contains("daemon stopped"), "stderr:\n{err}");
    assert!(fx.on_cache_branch(SLOW_FILE).exists());
    assert!(!fx.on_backing_branch(SLOW_FILE).exists());
    fx.assert_no_staging_directories();
}

/// The warning about an enabled `cachy.timer`.
#[cfg(feature = "fault-injection")]
const TIMER_ENABLED: &str = "warning: cachy.timer is enabled";

#[cfg(feature = "fault-injection")]
#[test]
fn the_daemon_warns_when_the_timer_is_enabled_too() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let config = scheduled_config(&fx, &["0 3 * * *"], &dir.path().join("summaries"));
    let systemd = tempfile::tempdir().unwrap();
    let wants = systemd.path().join("timers.target.wants");
    std::fs::create_dir(&wants).unwrap();
    // As `systemctl enable cachy.timer` links it.
    std::os::unix::fs::symlink("/etc/systemd/system/cachy.timer", wants.join("cachy.timer"))
        .unwrap();
    let env = [("CACHY_SYSTEMD_DIR", systemd.path().to_str().unwrap())];

    let daemon = fx.start_daemon_with(&config, "2026-01-01T02:59:00Z", &env);
    daemon.wait_for_lines(WAITING, 1);

    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
    assert_eq!(err.matches(TIMER_ENABLED).count(), 1, "stderr:\n{err}");
}

#[cfg(feature = "fault-injection")]
#[test]
fn the_daemon_does_not_warn_while_the_timer_is_disabled() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let config = scheduled_config(&fx, &["0 3 * * *"], &dir.path().join("summaries"));
    let systemd = tempfile::tempdir().unwrap();
    std::fs::create_dir(systemd.path().join("timers.target.wants")).unwrap();
    let env = [("CACHY_SYSTEMD_DIR", systemd.path().to_str().unwrap())];

    let daemon = fx.start_daemon_with(&config, "2026-01-01T02:59:00Z", &env);
    daemon.wait_for_lines(WAITING, 1);

    let (_, err) = daemon.stop();
    assert!(!err.contains(TIMER_ENABLED), "stderr:\n{err}");
}
