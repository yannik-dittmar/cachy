//! The in-use guard: move units open for writing are never moved, and a move
//! that a writer (or an atomic save) interrupts is aborted without data loss.

mod common;

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Output;
use std::thread;
use std::time::{Duration, Instant};

use common::{Fixture, assert_exit, pause, resume, wait_for};
use rustix::fs::{Mode, OFlags};

const MIB: usize = 1 << 20;

/// Large enough that a run caught when its temp file appears is still
/// copying.
const LARGE: usize = 8 * MIB;

fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|b| (b * 7) as u8 ^ seed).collect()
}

/// A config whose threshold and target lie far above any test filesystem's
/// free space, so a run moves every eligible move unit.
fn move_everything(fx: &Fixture) -> String {
    fx.config("1T", "2T")
}

#[test]
fn file_open_for_writing_is_skipped_as_in_use() {
    let fx = Fixture::new();
    fx.cache_branch_file("busy.bin", &content(MIB, 1), 1_000);
    fx.cache_branch_file("idle.bin", &content(MIB, 2), 2_000);
    let _writer = OpenOptions::new()
        .write(true)
        .open(fx.on_cache_branch("busy.bin"))
        .unwrap();

    let err = assert_exit(&fx.run(&move_everything(&fx)), 0);

    assert!(err.contains("busy.bin: in use"), "stderr:\n{err}");
    assert_eq!(
        fs::read(fx.on_cache_branch("busy.bin")).unwrap(),
        content(MIB, 1)
    );
    assert!(!fx.on_backing_branch("busy.bin").exists());
    assert_eq!(
        fs::read(fx.on_backing_branch("idle.bin")).unwrap(),
        content(MIB, 2)
    );
    assert!(!fx.on_cache_branch("idle.bin").exists());
}

/// Runs cachy on a fixture holding only `rel` and calls `during_copy` while
/// the run is paused between creating its temp file and finishing the copy.
/// Returns the run's output and when it resumed.
fn run_interrupted_mid_copy(
    fx: &Fixture,
    rel: &str,
    during_copy: impl FnOnce(),
) -> (Output, Instant) {
    let child = fx.spawn(&move_everything(fx));
    wait_for(&temp_file(fx, rel));
    pause(&child);
    assert!(
        !fx.on_backing_branch(rel).exists(),
        "the run was not caught mid-copy"
    );
    during_copy();
    resume(&child);
    let resumed = Instant::now();
    (child.wait_with_output().unwrap(), resumed)
}

/// Where a move of `rel` writes its copy on the backing branch.
fn temp_file(fx: &Fixture, rel: &str) -> PathBuf {
    fx.on_backing_branch(".cachy-staging").join(rel)
}

fn assert_no_staging_directories(fx: &Fixture) {
    assert!(!fx.on_cache_branch(".cachy-staging").exists());
    assert!(!fx.on_backing_branch(".cachy-staging").exists());
}

#[test]
fn writer_opening_mid_copy_aborts_the_move() {
    let fx = Fixture::new();
    fx.cache_branch_file("media/clip.bin", &content(LARGE, 3), 1_000);
    let source = fx.on_cache_branch("media/clip.bin");
    let mut writer = None;

    let (out, resumed) = run_interrupted_mid_copy(&fx, "media/clip.bin", || {
        // The open blocks until cachy gives up its lease, so it runs on its
        // own thread while cachy resumes.
        let source = source.clone();
        writer = Some(thread::spawn(move || {
            let file = OpenOptions::new().write(true).open(&source).unwrap();
            (file, Instant::now())
        }));
        thread::sleep(Duration::from_millis(100));
    });
    let (mut file, opened) = writer.unwrap().join().unwrap();

    let err = assert_exit(&out, 0);
    assert!(
        opened.saturating_duration_since(resumed) < Duration::from_secs(5),
        "the writer waited {:?} after cachy resumed",
        opened.saturating_duration_since(resumed)
    );
    file.write_all(b"new").unwrap();
    drop(file);
    assert!(err.contains("media/clip.bin: in use"), "stderr:\n{err}");
    let mut expected = content(LARGE, 3);
    expected[..3].copy_from_slice(b"new");
    assert!(fs::read(&source).unwrap() == expected, "the source changed");
    assert!(!fx.on_backing_branch("media/clip.bin").exists());
    assert_no_staging_directories(&fx);
}

/// Regression: upstream published the old content and deleted the new one.
#[test]
fn atomic_save_during_copy_aborts_the_move() {
    let fx = Fixture::new();
    fx.cache_branch_file("docs/report.odt", &content(LARGE, 5), 1_000);
    let source = fx.on_cache_branch("docs/report.odt");

    let (out, _) = run_interrupted_mid_copy(&fx, "docs/report.odt", || {
        // An editor's atomic save: write the new version to a temp file, then
        // rename it over the original.
        let saved = fx.on_cache_branch("docs/.~report.odt");
        fs::write(&saved, b"the new version").unwrap();
        fs::rename(&saved, &source).unwrap();
    });

    let err = assert_exit(&out, 0);
    assert!(err.contains("docs/report.odt: changed"), "stderr:\n{err}");
    assert_eq!(fs::read(&source).unwrap(), b"the new version");
    assert!(!fx.on_backing_branch("docs/report.odt").exists());
    assert_no_staging_directories(&fx);
}

/// Regression: a writer that resolved the path before the source was retired
/// must not write into an unlinked inode.
#[test]
fn write_during_retire_grace_period_rolls_the_move_back() {
    let fx = Fixture::new();
    fx.cache_branch_file("db/journal.log", &content(MIB, 6), 1_000);
    // An O_PATH descriptor names the inode without opening it, like a path
    // lookup that has finished before the open reaches the lease check.
    let looked_up = rustix::fs::open(
        fx.on_cache_branch("db/journal.log"),
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    let child = fx.spawn(&move_everything(&fx));

    wait_for(&fx.on_cache_branch(".cachy-staging/db/journal.log"));
    let mut writer = OpenOptions::new()
        .append(true)
        .open(format!("/proc/self/fd/{}", looked_up.as_raw_fd()))
        .unwrap();
    writer.write_all(b"appended").unwrap();
    drop(writer);

    let err = assert_exit(&child.wait_with_output().unwrap(), 0);
    assert!(err.contains("db/journal.log: in use"), "stderr:\n{err}");
    let mut expected = content(MIB, 6);
    expected.extend_from_slice(b"appended");
    assert!(
        fs::read(fx.on_cache_branch("db/journal.log")).unwrap() == expected,
        "the write was lost"
    );
    assert!(!fx.on_backing_branch("db/journal.log").exists());
    assert_no_staging_directories(&fx);
}

#[test]
fn reader_opening_mid_move_does_not_block_it() {
    let fx = Fixture::new();
    fx.cache_branch_file("media/clip.bin", &content(LARGE, 4), 1_000);
    let mut reader = None;

    let (out, _) = run_interrupted_mid_copy(&fx, "media/clip.bin", || {
        // O_NONBLOCK makes an open that would break cachy's lease fail
        // instead of waiting for it.
        reader = Some(
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(fx.on_cache_branch("media/clip.bin"))
                .expect("the reader's open conflicted with the move"),
        );
    });

    let err = assert_exit(&out, 0);
    assert!(err.contains("1 moved"), "stderr:\n{err}");
    assert!(!fx.on_cache_branch("media/clip.bin").exists());
    assert!(fs::read(fx.on_backing_branch("media/clip.bin")).unwrap() == content(LARGE, 4));
    let mut read = Vec::new();
    reader.unwrap().read_to_end(&mut read).unwrap();
    assert!(read == content(LARGE, 4), "the reader read different data");
}
