//! The alternatives to leases as the in-use guard, for platforms without
//! them: `open_file_guard: proc` scans `/proc` for other processes' open
//! descriptors of a file, and `open_file_guard: off` has no guard at all.

mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;

use common::{Fixture, assert_exit, output, pause, resume, wait_for};
use rustix::fs::{Mode, OFlags};

const MIB: usize = 1 << 20;
/// Large enough that a run caught when its temp file appears is still
/// copying.
const LARGE: usize = 8 * MIB;

fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|b| (b * 7) as u8 ^ seed).collect()
}

#[test]
fn proc_skips_a_file_open_for_writing_as_in_use() {
    let fx = Fixture::new();
    fx.cache_branch_file("busy.bin", &content(MIB, 1), 1_000);
    fx.cache_branch_file("idle.bin", &content(MIB, 2), 2_000);
    let _writer = OpenOptions::new()
        .write(true)
        .open(fx.on_cache_branch("busy.bin"))
        .unwrap();
    let config = fx.move_everything_config() + "open_file_guard: proc\n";

    let err = assert_exit(&fx.run(&config), 0);

    assert!(err.contains("busy.bin: in use"), "stderr:\n{err}");
    assert!(fs::read(fx.on_cache_branch("busy.bin")).unwrap() == content(MIB, 1));
    assert!(!fx.on_backing_branch("busy.bin").exists());
    assert!(fs::read(fx.on_backing_branch("idle.bin")).unwrap() == content(MIB, 2));
    assert!(!fx.on_cache_branch("idle.bin").exists());
}

#[test]
fn proc_moves_a_file_open_only_for_reading() {
    let fx = Fixture::new();
    fx.cache_branch_file("stream.bin", &content(MIB, 3), 1_000);
    let _reader = fs::File::open(fx.on_cache_branch("stream.bin")).unwrap();
    let config = fx.move_everything_config() + "open_file_guard: proc\n";

    let err = assert_exit(&fx.run(&config), 0);

    assert!(err.contains("1 moved"), "stderr:\n{err}");
    assert!(!fx.on_cache_branch("stream.bin").exists());
    assert!(fs::read(fx.on_backing_branch("stream.bin")).unwrap() == content(MIB, 3));
}

#[test]
fn proc_with_a_write_lease_type_skips_a_file_open_only_for_reading() {
    let fx = Fixture::new();
    fx.cache_branch_file("stream.bin", &content(MIB, 4), 1_000);
    fx.cache_branch_file("idle.bin", &content(MIB, 5), 2_000);
    let _reader = fs::File::open(fx.on_cache_branch("stream.bin")).unwrap();
    let config = fx.move_everything_config() + "open_file_guard: proc\nlease_type: write\n";

    let err = assert_exit(&fx.run(&config), 0);

    assert!(err.contains("stream.bin: in use"), "stderr:\n{err}");
    assert!(fs::read(fx.on_cache_branch("stream.bin")).unwrap() == content(MIB, 4));
    assert!(fs::read(fx.on_backing_branch("idle.bin")).unwrap() == content(MIB, 5));
}

/// Without a lease, a writer's open mid-copy does not wait for the move; the
/// scan right before publish finds it.
#[test]
fn proc_finds_a_writer_that_opened_mid_copy_before_publishing() {
    let fx = Fixture::new();
    fx.cache_branch_file("media/clip.bin", &content(LARGE, 6), 1_000);
    let config = fx.move_everything_config() + "open_file_guard: proc\n";
    let child = fx.spawn(&config);
    wait_for(&fx.staged_on_backing_branch("media/clip.bin"));
    pause(&child);
    assert!(
        !fx.on_backing_branch("media/clip.bin").exists(),
        "the run was not caught mid-copy"
    );
    let _writer = OpenOptions::new()
        .write(true)
        .open(fx.on_cache_branch("media/clip.bin"))
        .unwrap();
    resume(&child);

    let err = assert_exit(&child.wait_with_output().unwrap(), 0);

    assert!(err.contains("media/clip.bin: in use"), "stderr:\n{err}");
    assert!(fs::read(fx.on_cache_branch("media/clip.bin")).unwrap() == content(LARGE, 6));
    assert!(!fx.on_backing_branch("media/clip.bin").exists());
    fx.assert_no_staging_directories();
}

/// Runs a move of `db/journal.log` with the proc guard and, while its retired
/// source waits, appends to it through a lookup from before the retire.
/// Returns the run's stderr and the appending file, still open unless
/// `close`.
fn append_while_the_retired_source_waits(fx: &Fixture, close: bool) -> (String, Option<fs::File>) {
    fx.cache_branch_file("db/journal.log", &content(MIB, 7), 1_000);
    let looked_up = look_up(&fx.on_cache_branch("db/journal.log"));
    let config = fx.move_everything_config() + "open_file_guard: proc\n";
    let child = fx.spawn(&config);

    wait_for(&fx.staged_on_cache_branch("db/journal.log"));
    pause(&child);
    let mut writer = OpenOptions::new()
        .append(true)
        .open(format!("/proc/self/fd/{}", looked_up.as_raw_fd()))
        .unwrap();
    writer.write_all(b"appended").unwrap();
    let writer = (!close).then_some(writer);
    resume(&child);

    let err = assert_exit(&child.wait_with_output().unwrap(), 0);
    (err, writer)
}

#[test]
fn proc_rolls_back_a_move_whose_retired_source_is_open_for_writing() {
    let fx = Fixture::new();

    let (err, writer) = append_while_the_retired_source_waits(&fx, false);

    assert!(err.contains("db/journal.log: in use"), "stderr:\n{err}");
    let mut writer = writer.unwrap();
    writer.write_all(b" and more").unwrap();
    drop(writer);
    let mut expected = content(MIB, 7);
    expected.extend_from_slice(b"appended and more");
    assert!(
        fs::read(fx.on_cache_branch("db/journal.log")).unwrap() == expected,
        "the write was lost"
    );
    assert!(!fx.on_backing_branch("db/journal.log").exists());
    fx.assert_no_staging_directories();
}

/// A writer that opened, wrote and closed between two scans leaves no open to
/// find, but its write changed the source.
#[test]
fn proc_rolls_back_a_move_whose_retired_source_was_written_since_it_was_copied() {
    let fx = Fixture::new();

    let (err, _) = append_while_the_retired_source_waits(&fx, true);

    assert!(err.contains("db/journal.log: changed"), "stderr:\n{err}");
    let mut expected = content(MIB, 7);
    expected.extend_from_slice(b"appended");
    assert!(
        fs::read(fx.on_cache_branch("db/journal.log")).unwrap() == expected,
        "the write was lost"
    );
    assert!(!fx.on_backing_branch("db/journal.log").exists());
    fx.assert_no_staging_directories();
}

#[test]
fn proc_dry_run_would_skip_a_file_open_for_writing() {
    let fx = Fixture::new();
    fx.cache_branch_file("busy.bin", &content(MIB, 8), 1_000);
    let _writer = OpenOptions::new()
        .write(true)
        .open(fx.on_cache_branch("busy.bin"))
        .unwrap();
    let config = fx.move_everything_config() + "open_file_guard: proc\n";

    let out = output(fx.command(&config).arg("--dry-run")).unwrap();

    assert_exit(&out, 0);
    let planned = String::from_utf8(out.stdout).unwrap();
    assert!(
        planned.contains("would skip busy.bin: in use"),
        "stdout:\n{planned}"
    );
}

/// Without leases, recovery cannot rule out a writer of a retired source, so
/// it keeps it; the next move adopts the published copy.
#[test]
fn proc_recovery_renames_back_a_retired_source_identical_to_its_published_copy() {
    let fx = Fixture::new();
    fx.staged_cache_branch_file("dir/file.bin", b"content", 1_000);
    fx.backing_branch_file("dir/file.bin", b"content", 1_000);
    let config = fx.no_work_config() + "open_file_guard: proc\n";

    let err = assert_exit(&fx.run(&config), 0);

    assert!(err.contains("renamed back"), "stderr:\n{err}");
    assert_eq!(
        fs::read(fx.on_cache_branch("dir/file.bin")).unwrap(),
        b"content"
    );
    assert_eq!(
        fs::read(fx.on_backing_branch("dir/file.bin")).unwrap(),
        b"content"
    );
    fx.assert_no_staging_directories();
}

#[test]
fn off_moves_a_file_open_for_writing() {
    let fx = Fixture::new();
    fx.cache_branch_file("busy.bin", &content(MIB, 9), 1_000);
    let _writer = OpenOptions::new()
        .write(true)
        .open(fx.on_cache_branch("busy.bin"))
        .unwrap();
    let config = fx.move_everything_config() + "open_file_guard: off\n";

    let err = assert_exit(&fx.run(&config), 0);

    assert!(err.contains("1 moved"), "stderr:\n{err}");
    assert!(!fx.on_cache_branch("busy.bin").exists());
    assert!(fs::read(fx.on_backing_branch("busy.bin")).unwrap() == content(MIB, 9));
}

/// The warning comes first, on every run: one that finds no work, a dry run
/// and one that moves.
#[test]
fn off_warns_on_every_run() {
    let fx = Fixture::new();
    fx.cache_branch_file("idle.bin", &content(MIB, 10), 1_000);
    let guard = "open_file_guard: off\n";

    let runs = [
        fx.run(&(fx.no_work_config() + guard)),
        output(
            fx.command(&(fx.move_everything_config() + guard))
                .arg("--dry-run"),
        )
        .unwrap(),
        fx.run(&(fx.move_everything_config() + guard)),
    ];

    for out in &runs {
        let err = assert_exit(out, 0);
        let first = err.lines().next().unwrap_or_default();
        assert!(
            first.starts_with("cachy: warning: open_file_guard is off")
                && first.contains("NOTHING GUARDS FILES IN USE"),
            "stderr:\n{err}"
        );
    }
}

#[test]
fn runs_name_their_in_use_guard() {
    for (setting, guard) in [
        ("", "lease"),
        ("open_file_guard: lease\n", "lease"),
        ("open_file_guard: proc\n", "proc"),
        ("open_file_guard: off\n", "off"),
    ] {
        let fx = Fixture::new();
        fx.cache_branch_file("idle.bin", &content(MIB, 11), 1_000);
        let config = fx.move_everything_config() + setting;

        let dry_run = output(fx.command(&config).arg("--dry-run")).unwrap();
        let run = fx.run(&config);

        for out in [dry_run, run] {
            let err = assert_exit(&out, 0);
            assert!(
                err.lines().any(|line| line.contains(" finished: ")
                    && line.ends_with(&format!("; in-use guard: {guard}"))),
                "stderr:\n{err}"
            );
        }
    }
}

#[test]
fn unknown_guard_is_refused() {
    let fx = Fixture::new();
    let config = fx.move_everything_config() + "open_file_guard: none\n";

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("open_file_guard"), "stderr:\n{err}");
    assert!(err.contains("`lease`, `proc`"), "stderr:\n{err}");
}

/// An `O_PATH` descriptor names the inode without opening it, like a path
/// lookup that has finished before the open.
fn look_up(path: &Path) -> OwnedFd {
    rustix::fs::open(path, OFlags::PATH | OFlags::CLOEXEC, Mode::empty()).unwrap()
}
