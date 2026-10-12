//! Read-back verification (`verify: readback`): a copy that reaches the disk
//! differing from its source is never published.
//!
//! The tests that corrupt a copy drive a `cachy` built with the test-only
//! `fault-injection` feature, whose `corrupt` fault makes a write succeed
//! with its data altered.

mod common;

use std::fs;

use common::{Fixture, assert_exit};

const CONTENT: &[u8] = b"irreplaceable data";

fn readback_config(fx: &Fixture) -> String {
    fx.move_everything_config() + "verify: readback\n"
}

#[test]
fn readback_moves_files_whose_copy_reads_back_intact() {
    let fx = Fixture::new();
    fx.cache_branch_file("file.bin", CONTENT, 1_000);
    fx.cache_branch_file("empty", b"", 1_000);

    assert_exit(&fx.run(&readback_config(&fx)), 0);

    assert_eq!(fs::read(fx.on_backing_branch("file.bin")).unwrap(), CONTENT);
    assert_eq!(fs::read(fx.on_backing_branch("empty")).unwrap(), b"");
    assert!(!fx.on_cache_branch("file.bin").exists());
    fx.assert_no_staging_directories();
}

#[test]
fn unknown_verification_is_refused() {
    let fx = Fixture::new();
    let config = fx.move_everything_config() + "verify: twice\n";

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("verify"), "stderr:\n{err}");
}

#[cfg(feature = "fault-injection")]
mod fault_injection {
    use super::*;

    /// A fixture with one file to move.
    fn one_file() -> Fixture {
        let fx = Fixture::new();
        fx.cache_branch_file("file.bin", CONTENT, 1_000);
        fx
    }

    /// The number of the first write of a run of `config` on [`one_file`],
    /// found by tracing a run on a fixture of its own.
    fn first_write(config: impl Fn(&Fixture) -> String) -> u64 {
        let fx = one_file();
        let out = common::output(fx.command(&config(&fx)).env("CACHY_FAULT_TRACE", "1")).unwrap();
        let err = assert_exit(&out, 0);
        err.lines()
            .find_map(|line| {
                let op = line.strip_prefix("cachy: trace: operation ")?;
                let (n, name) = op.split_once(' ')?;
                (name == "(write_at)").then(|| n.parse().unwrap())
            })
            .unwrap_or_else(|| panic!("no write traced; stderr:\n{err}"))
    }

    /// Runs `config` on [`one_file`] with its first write corrupted.
    fn run_corrupted(config: impl Fn(&Fixture) -> String) -> (Fixture, String, i32) {
        let n = first_write(&config);
        let fx = one_file();
        let out = common::output(
            fx.command(&config(&fx))
                .env("CACHY_FAULT", format!("corrupt@{n}")),
        )
        .unwrap();
        let err = common::stderr(&out);
        assert!(
            err.contains("cachy: fault injection: corrupt"),
            "nothing corrupted; stderr:\n{err}"
        );
        (fx, err, out.status.code().unwrap())
    }

    #[test]
    fn readback_fails_the_move_of_a_corrupted_copy_and_keeps_the_source() {
        let (fx, err, code) = run_corrupted(readback_config);

        assert_eq!(code, 2, "stderr:\n{err}");
        assert!(err.contains("read back"), "stderr:\n{err}");
        assert_eq!(fs::read(fx.on_cache_branch("file.bin")).unwrap(), CONTENT);
        assert!(!fx.on_backing_branch("file.bin").exists());
        fx.assert_no_staging_directories();
    }

    /// Without read-back, nothing reads the copy, so a corruption on its way
    /// to the disk goes unnoticed.
    #[test]
    fn default_does_not_read_the_copy_back() {
        let (fx, err, code) = run_corrupted(Fixture::move_everything_config);

        assert_eq!(code, 0, "stderr:\n{err}");
        let copy = fs::read(fx.on_backing_branch("file.bin")).unwrap();
        assert_eq!(copy.len(), CONTENT.len());
        assert_ne!(copy, CONTENT);
    }
}
