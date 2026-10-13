//! What cachy tells the operator outside the run summary: its version, and
//! log lines on stderr.

mod common;

use std::io::{Read, Seek, SeekFrom};
use std::process::Command;

use common::{Fixture, assert_exit};

#[test]
fn version_prints_the_crate_version() {
    let out = common::cachy(&["--version"]);

    assert_exit(&out, 0);
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("cachy {}\n", env!("CARGO_PKG_VERSION"))
    );
}

/// A run that moves a file, skips a special file and warns of a missing
/// in-use guard, under `fx`.
fn eventful_run(fx: &Fixture) -> Command {
    fx.cache_branch_file("a.txt", b"a", 1_000);
    rustix::fs::mknodat(
        rustix::fs::CWD,
        fx.on_cache_branch("pipe"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();
    fx.command(&(fx.move_everything_config() + "open_file_guard: off\n"))
}

#[test]
fn every_log_line_names_its_level() {
    let fx = Fixture::new();

    let err = assert_exit(&common::output(&mut eventful_run(&fx)).unwrap(), 0);

    for level in ["info", "warning"] {
        assert!(err.contains(&format!("cachy: {level}: ")), "stderr:\n{err}");
    }
    for line in err.lines() {
        assert!(
            ["error", "warning", "info"]
                .iter()
                .any(|level| line.starts_with(&format!("cachy: {level}: "))),
            "line without a level: {line:?}"
        );
    }
    assert!(!err.contains('\x1b'), "colour codes in stderr:\n{err}");
}

#[test]
fn log_lines_into_the_journal_carry_syslog_priorities() {
    let fx = Fixture::new();
    let log = tempfile::tempfile().unwrap();
    let stat = rustix::fs::fstat(&log).unwrap();
    let mut command = eventful_run(&fx);
    // As systemd connects a service's stderr to the journal.
    command
        .env("JOURNAL_STREAM", format!("{}:{}", stat.st_dev, stat.st_ino))
        .stderr(log.try_clone().unwrap());

    let out = common::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let mut err = String::new();
    let mut log = log;
    log.seek(SeekFrom::Start(0)).unwrap();
    log.read_to_string(&mut err).unwrap();
    assert!(err.contains("<6>cachy: info: "), "stderr:\n{err}");
    assert!(err.contains("<4>cachy: warning: "), "stderr:\n{err}");
    for line in err.lines() {
        let priority = [
            "<3>cachy: error: ",
            "<4>cachy: warning: ",
            "<6>cachy: info: ",
        ];
        assert!(
            priority.iter().any(|prefix| line.starts_with(prefix)),
            "line without its priority: {line:?}"
        );
    }
}
