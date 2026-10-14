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
        assert!(
            err.contains("set daemon.schedules or daemon.wake_trigger"),
            "stderr:\n{err}"
        );
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

/// Three backing disks, by the stable IDs that the tests' scripted disk
/// state reports on.
const DISKS: [&str; 3] = [
    "/dev/disk/by-id/ata-DISK1",
    "/dev/disk/by-id/ata-DISK2",
    "/dev/disk/by-id/ata-DISK3",
];

/// `config` with the daemon's backing disks set to [`DISKS`] and its
/// scheduled checks gated, with a time limit of an hour.
#[cfg(feature = "fault-injection")]
fn gated(config: String) -> String {
    let mut config = config + "  backing_disks:\n";
    for disk in DISKS {
        config += &format!("    - id: {disk}\n");
    }
    config + "  wake_gate:\n    time_limit: 1h\n"
}

#[test]
fn a_backing_disk_by_an_unstable_path_is_a_config_error() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let config = scheduled_config(&fx, &["0 3 * * *"], &dir.path().join("summaries"))
        + "  backing_disks:\n    - id: /dev/sdb\n";

    let err = assert_exit(&common::output(&mut fx.daemon_command(&config)).unwrap(), 1);

    assert!(err.contains("invalid config"), "stderr:\n{err}");
    assert!(
        err.contains(
            "backing disk `/dev/sdb` is not a stable device ID: expected a path in /dev/disk/by-id/"
        ),
        "stderr:\n{err}"
    );
}

#[test]
fn a_backing_disk_given_twice_is_a_config_error() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let config = scheduled_config(&fx, &["0 3 * * *"], &dir.path().join("summaries"))
        + &format!("  backing_disks:\n    - id: {0}\n    - id: {0}\n", DISKS[0]);

    let err = assert_exit(&common::output(&mut fx.daemon_command(&config)).unwrap(), 1);

    assert!(
        err.contains(&format!("backing disk `{}` is listed twice", DISKS[0])),
        "stderr:\n{err}"
    );
}

#[test]
fn a_wake_gate_without_backing_disks_is_a_config_error() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let config = scheduled_config(&fx, &["0 3 * * *"], &dir.path().join("summaries"))
        + "  wake_gate:\n    time_limit: 1h\n";

    let err = assert_exit(&common::output(&mut fx.daemon_command(&config)).unwrap(), 1);

    assert!(
        err.contains("daemon.wake_gate needs daemon.backing_disks"),
        "stderr:\n{err}"
    );
}

/// On the real disk state, in a build without fault injection as well.
#[test]
fn a_backing_disk_that_does_not_exist_keeps_the_daemon_from_starting() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let missing = "/dev/disk/by-id/cachy-test-no-such-disk";
    let config = scheduled_config(&fx, &["0 3 * * *"], &dir.path().join("summaries"))
        + &format!("  backing_disks:\n    - id: {missing}\n");

    let err = assert_exit(&common::output(&mut fx.daemon_command(&config)).unwrap(), 1);

    assert!(
        err.contains(&format!("backing disk {missing}: no such disk")),
        "stderr:\n{err}"
    );
    assert!(!err.contains(WAITING), "stderr:\n{err}");
}

/// Sets what the scripted disk state in `path` reports: each disk of
/// [`DISKS`] is awake if it is in `awake`, else asleep.
#[cfg(feature = "fault-injection")]
fn set_disks(path: &std::path::Path, awake: &[&str]) {
    let mut text = String::new();
    for disk in DISKS {
        let state = if awake.contains(&disk) {
            "awake"
        } else {
            "asleep"
        };
        text += &format!("{disk} {state}\n");
    }
    let staged = path.with_extension("new");
    std::fs::write(&staged, text).unwrap();
    std::fs::rename(&staged, path).unwrap();
}

/// The line that the wake gate logs when it starts to hold a scheduled check.
#[cfg(feature = "fault-injection")]
const GATE_HOLDS: &str = "waiting for the backing disks to wake";

/// Starts a daemon of `fx` under `config`, at 02:59, whose disk state is
/// scripted in `disks`, and lets its 03:00 check reach the wake gate.
#[cfg(feature = "fault-injection")]
fn daemon_at_the_gate(fx: &Fixture, config: &str, disks: &std::path::Path) -> common::Daemon {
    let env = [("CACHY_DISK_STATE", disks.to_str().unwrap())];
    let daemon = fx.start_daemon_with(config, "2026-01-01T02:59:00Z", &env);
    daemon.wait_for_lines(WAITING, 1);
    daemon.set_clock("2026-01-01T03:00:00Z");
    daemon.wait_for_lines(GATE_HOLDS, 1);
    daemon
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_gated_check_waits_until_every_backing_disk_is_awake() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let summaries_path = dir.path().join("summaries");
    let disks = dir.path().join("disks");
    set_disks(&disks, &[]);
    fx.cache_branch_file("a.txt", b"a", 1_000);
    let config = gated(scheduled_config(&fx, &["0 3 * * *"], &summaries_path));

    let daemon = daemon_at_the_gate(&fx, &config, &disks);
    let err = daemon.stderr();
    assert!(
        err.contains(&format!(
            "{GATE_HOLDS} until 2026-01-01 04:00 UTC (asleep: {})",
            DISKS.join(", ")
        )),
        "stderr:\n{err}"
    );
    // Two disks out of three wake: the backing disks are not awake yet.
    set_disks(&disks, &DISKS[..2]);
    daemon.set_clock("2026-01-01T03:10:00Z");
    daemon.wait_for_lines(&format!("still asleep: {}", DISKS[2]), 1);
    assert!(fx.on_cache_branch("a.txt").exists());

    set_disks(&disks, &DISKS);
    daemon.set_clock("2026-01-01T03:20:00Z");
    daemon.wait_for_lines(&format!("{WAITING} 2026-01-02 03:00"), 1);

    let err = daemon.stderr();
    assert!(
        err.contains("the backing disks are awake"),
        "stderr:\n{err}"
    );
    assert!(fx.on_backing_branch("a.txt").exists());
    let summaries = summaries(&summaries_path);
    assert_eq!(summaries.len(), 1, "{summaries:?}");
    assert_eq!(summaries[0]["reason"], "schedule");
    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_gated_check_goes_on_at_the_time_limit_when_free_space_is_below_the_threshold() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let summaries_path = dir.path().join("summaries");
    let disks = dir.path().join("disks");
    // One disk out of three asleep.
    set_disks(&disks, &DISKS[1..]);
    fx.cache_branch_file("a.txt", b"a", 1_000);
    // A threshold of 1T: free space is below it.
    let config = gated(scheduled_config(&fx, &["0 3 * * *"], &summaries_path));
    let daemon = daemon_at_the_gate(&fx, &config, &disks);

    daemon.set_clock("2026-01-01T04:00:00Z");
    daemon.wait_for_lines(&format!("{WAITING} 2026-01-02 03:00"), 1);

    let err = daemon.stderr();
    assert!(
        err.contains(
            "the backing disks did not wake within the wake gate's time limit, but free space is below the threshold ("
        ),
        "stderr:\n{err}"
    );
    assert!(
        err.contains("so the scheduled check goes on"),
        "stderr:\n{err}"
    );
    assert!(fx.on_backing_branch("a.txt").exists());
    assert_eq!(summaries(&summaries_path).len(), 1);
    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_gated_check_is_skipped_at_the_time_limit_unless_free_space_is_below_the_threshold() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let summaries_path = dir.path().join("summaries");
    let disks = dir.path().join("disks");
    set_disks(&disks, &[]);
    fx.cache_branch_file("a.txt", b"a", 1_000);
    // A threshold of 0 bytes, and a max age that a.txt is past: at the time
    // limit, only the threshold counts.
    let config = gated(scheduled_config(&fx, &["0 3 * * *"], &summaries_path)).replace(
        "threshold: 1T\ntarget: 2T",
        "threshold: 0\ntarget: 1\nmax_age: 1d",
    );
    let daemon = daemon_at_the_gate(&fx, &config, &disks);

    daemon.set_clock("2026-01-01T04:00:00Z");
    daemon.wait_for_lines(&format!("{WAITING} 2026-01-02 03:00"), 1);

    let err = daemon.stderr();
    assert!(
        err.contains(
            "skipped the scheduled check: the backing disks did not wake within the wake gate's time limit, and free space"
        ),
        "stderr:\n{err}"
    );
    assert!(fx.on_cache_branch("a.txt").exists());
    assert!(summaries(&summaries_path).is_empty());
    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
}

#[cfg(feature = "fault-injection")]
#[test]
fn sigterm_at_the_wake_gate_stops_the_daemon() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let disks = dir.path().join("disks");
    set_disks(&disks, &[]);
    let config = gated(scheduled_config(
        &fx,
        &["0 3 * * *"],
        &dir.path().join("summaries"),
    ));
    let daemon = daemon_at_the_gate(&fx, &config, &disks);

    let (status, err) = daemon.stop();

    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
    assert!(err.contains("daemon stopped"), "stderr:\n{err}");
    assert!(
        !err.contains("the backing disks are awake"),
        "stderr:\n{err}"
    );
}

/// `config` with the daemon's backing disks set to [`DISKS`] and a wake
/// trigger that looks at them every ten minutes.
#[cfg(feature = "fault-injection")]
fn triggered(config: String) -> String {
    let mut config = config + "daemon:\n  backing_disks:\n";
    for disk in DISKS {
        config += &format!("    - id: {disk}\n");
    }
    config + "  wake_trigger:\n    check_interval: 10m\n"
}

/// The line that the daemon logs as a wake-triggered check starts.
#[cfg(feature = "fault-injection")]
const TRIGGERED: &str = "info: wake-triggered check:";

/// Starts a daemon of `fx` under `config`, at 01:00, whose disk state is
/// scripted in `disks`.
#[cfg(feature = "fault-injection")]
fn triggered_daemon(fx: &Fixture, config: &str, disks: &std::path::Path) -> common::Daemon {
    let env = [("CACHY_DISK_STATE", disks.to_str().unwrap())];
    fx.start_daemon_with(config, "2026-01-01T01:00:00Z", &env)
}

/// The times at which the daemon looked at the scripted disk state in
/// `disks`, as the fake records them.
#[cfg(feature = "fault-injection")]
fn looks(disks: &std::path::Path) -> Vec<String> {
    let mut path = disks.as_os_str().to_owned();
    path.push(".looks");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Waits until the daemon has looked at the scripted disk state in `disks`
/// `n` times. It does one thing at a time: the looks before the last one,
/// and their checks, are over.
#[cfg(feature = "fault-injection")]
fn wait_for_looks(daemon: &common::Daemon, disks: &std::path::Path, n: usize) {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(30);
    while looks(disks).len() < n {
        assert!(
            !daemon.exited() && Instant::now() < deadline,
            "fewer than {n} looks at the disks: {:?}; stderr:\n{}",
            looks(disks),
            daemon.stderr()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn while_the_backing_disks_are_awake_the_wake_trigger_checks_every_check_interval() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let disks = dir.path().join("disks");
    set_disks(&disks, &DISKS);
    let config = triggered(fx.no_work_config());

    let daemon = triggered_daemon(&fx, &config, &disks);
    wait_for_looks(&daemon, &disks, 1);
    // Not yet a check interval since.
    daemon.set_clock("2026-01-01T01:05:00Z");
    daemon.set_clock("2026-01-01T01:10:00Z");
    wait_for_looks(&daemon, &disks, 2);
    // One disk falls asleep.
    set_disks(&disks, &DISKS[1..]);
    daemon.set_clock("2026-01-01T01:20:00Z");
    wait_for_looks(&daemon, &disks, 3);
    daemon.set_clock("2026-01-01T01:30:00Z");
    wait_for_looks(&daemon, &disks, 4);
    set_disks(&disks, &DISKS);
    daemon.set_clock("2026-01-01T01:40:00Z");
    wait_for_looks(&daemon, &disks, 5);
    daemon.set_clock("2026-01-01T01:45:00Z");
    daemon.wait_for_lines("nothing to do", 3);

    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
    assert_eq!(
        looks(&disks),
        [
            "2026-01-01T01:00:00Z",
            "2026-01-01T01:10:00Z",
            "2026-01-01T01:20:00Z",
            "2026-01-01T01:30:00Z",
            "2026-01-01T01:40:00Z",
        ]
    );
    assert_eq!(err.matches(TRIGGERED).count(), 3, "stderr:\n{err}");
    assert_eq!(err.matches("nothing to do").count(), 3, "stderr:\n{err}");
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_wake_triggered_check_that_finds_work_starts_a_run_with_reason_wake_trigger() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let summaries_path = dir.path().join("summaries");
    let disks = dir.path().join("disks");
    set_disks(&disks, &DISKS);
    fx.cache_branch_file("a.txt", b"a", 1_000);
    let config = triggered(hooked_config(&fx, &summaries_path, ""));

    let daemon = triggered_daemon(&fx, &config, &disks);
    wait_for_looks(&daemon, &disks, 1);
    // The check of the first look is over at the next one, which finds the
    // disks asleep.
    set_disks(&disks, &[]);
    daemon.set_clock("2026-01-01T01:10:00Z");
    wait_for_looks(&daemon, &disks, 2);

    assert!(fx.on_backing_branch("a.txt").exists());
    assert!(!fx.on_cache_branch("a.txt").exists());
    let summaries = summaries(&summaries_path);
    assert_eq!(summaries.len(), 1, "{summaries:?}");
    assert_eq!(summaries[0]["reason"], "wake_trigger");
    assert_eq!(summaries[0]["outcome"], "moved");
    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_wake_triggered_check_above_the_threshold_with_nothing_past_the_max_age_starts_no_run() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let summaries_path = dir.path().join("summaries");
    let disks = dir.path().join("disks");
    set_disks(&disks, &DISKS);
    fx.cache_branch_file("a.txt", b"a", 1_000);
    // a.txt is not past the max age.
    let config = triggered(hooked_config(&fx, &summaries_path, "")).replace(
        "threshold: 1T\ntarget: 2T",
        "threshold: 0\ntarget: 1\nmax_age: 100000d",
    );

    let daemon = triggered_daemon(&fx, &config, &disks);
    wait_for_looks(&daemon, &disks, 1);
    daemon.set_clock("2026-01-01T01:10:00Z");
    wait_for_looks(&daemon, &disks, 2);

    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
    assert_eq!(err.matches(TRIGGERED).count(), 2, "stderr:\n{err}");
    assert!(err.contains("nothing to do"), "stderr:\n{err}");
    assert!(fx.on_cache_branch("a.txt").exists());
    assert!(summaries(&summaries_path).is_empty());
}

/// Sets what the scripted disk state in `path` reports: every disk of
/// [`DISKS`] awake, with `requests` completed.
#[cfg(feature = "fault-injection")]
fn set_disk_io(path: &std::path::Path, requests: u64) {
    let mut text = String::new();
    for disk in DISKS {
        text += &format!("{disk} awake {requests}\n");
    }
    let staged = path.with_extension("new");
    std::fs::write(&staged, &text).unwrap();
    std::fs::rename(&staged, path).unwrap();
}

#[cfg(feature = "fault-injection")]
#[test]
fn cachys_own_io_does_not_keep_the_wake_trigger_firing() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let summaries_path = dir.path().join("summaries");
    let disks = dir.path().join("disks");
    set_disk_io(&disks, 100);
    fx.cache_branch_file("a.txt", b"a", 1_000);
    // A run's moves are I/O on the backing disks: the hook, which the check
    // executes before it is over, makes their counters change.
    let own_io = dir.path().join("own-io");
    set_disk_io(&own_io, 150);
    let config = triggered(hooked_config(
        &fx,
        &summaries_path,
        &format!("; cp {} {}", own_io.display(), disks.display()),
    ));

    let daemon = triggered_daemon(&fx, &config, &disks);
    wait_for_looks(&daemon, &disks, 1);
    daemon.set_clock("2026-01-01T01:10:00Z");
    wait_for_looks(&daemon, &disks, 2);
    daemon.set_clock("2026-01-01T01:20:00Z");
    wait_for_looks(&daemon, &disks, 3);
    // Others use the disks.
    set_disk_io(&disks, 160);
    daemon.set_clock("2026-01-01T01:30:00Z");
    wait_for_looks(&daemon, &disks, 4);
    daemon.set_clock("2026-01-01T01:40:00Z");
    wait_for_looks(&daemon, &disks, 5);
    daemon.set_clock("2026-01-01T01:50:00Z");
    wait_for_looks(&daemon, &disks, 6);

    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
    // At 01:00, and at 01:30, after the I/O of others; not at 01:10 or
    // 01:20, after cachy's own, nor at 01:40 or 01:50, after the check at
    // 01:30.
    // Each of them a run, as free space is below the threshold.
    assert_eq!(summaries(&summaries_path).len(), 2);
    assert_eq!(err.matches(TRIGGERED).count(), 2, "stderr:\n{err}");
    assert!(
        err.contains("awake only by cachy's own I/O"),
        "stderr:\n{err}"
    );
}

#[cfg(feature = "fault-injection")]
#[test]
fn the_check_interval_is_five_minutes_unless_the_config_sets_one() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let disks = dir.path().join("disks");
    set_disks(&disks, &[]);
    let config = triggered(fx.no_work_config()).replace("    check_interval: 10m\n", "");
    let config = config.replace("  wake_trigger:\n", "  wake_trigger: {}\n");

    let daemon = triggered_daemon(&fx, &config, &disks);
    wait_for_looks(&daemon, &disks, 1);
    daemon.set_clock("2026-01-01T01:05:00Z");
    wait_for_looks(&daemon, &disks, 2);

    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
    assert!(
        err.contains("wake trigger: a check every 5m while the backing disks are awake"),
        "stderr:\n{err}"
    );
    assert_eq!(
        looks(&disks),
        ["2026-01-01T01:00:00Z", "2026-01-01T01:05:00Z"]
    );
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_scheduled_check_stands_in_for_the_wake_triggered_check_due_with_it() {
    let fx = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let disks = dir.path().join("disks");
    set_disks(&disks, &DISKS);
    let config = scheduled_config(&fx, &["0 3 * * *"], &dir.path().join("summaries"))
        .replace("threshold: 1T\ntarget: 2T", "threshold: 0\ntarget: 1");
    let config = triggered(config).replace("\ndaemon:\n  backing_disks:", "\n  backing_disks:");
    let env = [("CACHY_DISK_STATE", disks.to_str().unwrap())];

    let daemon = fx.start_daemon_with(&config, "2026-01-01T02:50:00Z", &env);
    wait_for_looks(&daemon, &disks, 1);
    daemon.set_clock("2026-01-01T03:00:00Z");
    daemon.wait_for_lines(&format!("{WAITING} 2026-01-02 03:00"), 1);
    daemon.set_clock("2026-01-01T03:10:00Z");
    wait_for_looks(&daemon, &disks, 2);

    let (status, err) = daemon.stop();
    assert_eq!(status.code(), Some(0), "stderr:\n{err}");
    assert_eq!(
        looks(&disks),
        ["2026-01-01T02:50:00Z", "2026-01-01T03:10:00Z"]
    );
    assert_eq!(err.matches(TRIGGERED).count(), 2, "stderr:\n{err}");
    assert_eq!(
        err.matches("scheduled check (`").count(),
        1,
        "stderr:\n{err}"
    );
    // Logged once each.
    assert_eq!(err.matches(WAITING).count(), 2, "stderr:\n{err}");
}

#[test]
fn an_invalid_check_interval_is_a_config_error() {
    let fx = Fixture::new();
    let disks = format!("  backing_disks:\n    - id: {}\n", DISKS[0]);
    for (trigger, error) in [
        (
            "  wake_trigger:\n    check_interval: 5 minutes\n",
            "invalid duration `5 minutes`",
        ),
        (
            "  wake_trigger:\n    check_interval: 0s\n",
            "daemon.wake_trigger: check_interval must be above 0s",
        ),
        ("  wake_trigger:\n    every: 5m\n", "unknown field `every`"),
    ] {
        let config = fx.move_everything_config() + "daemon:\n" + &disks + trigger;

        let err = assert_exit(&common::output(&mut fx.daemon_command(&config)).unwrap(), 1);

        assert!(err.contains("invalid config"), "stderr:\n{err}");
        assert!(err.contains(error), "{trigger}stderr:\n{err}");
    }
}

#[test]
fn a_wake_trigger_without_backing_disks_is_a_config_error() {
    let fx = Fixture::new();
    let config = fx.move_everything_config() + "daemon:\n  wake_trigger:\n    check_interval: 5m\n";

    let err = assert_exit(&common::output(&mut fx.daemon_command(&config)).unwrap(), 1);

    assert!(
        err.contains("daemon.wake_trigger needs daemon.backing_disks"),
        "stderr:\n{err}"
    );
}

/// A config of `fx` that moves everything, with a run hook that appends each
/// run summary, one per line, to `summaries`, then executes `then`.
#[cfg(feature = "fault-injection")]
fn hooked_config(fx: &Fixture, summaries: &std::path::Path, then: &str) -> String {
    fx.move_everything_config()
        + &format!(
            "run_hook:\n  command: 'cat >> {}; echo >> {}{then}'\n",
            summaries.display(),
            summaries.display()
        )
}
