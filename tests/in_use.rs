//! The in-use guard: move units open for writing are never moved, and a move
//! that a writer (or an atomic save) interrupts is aborted without data loss.

mod common;

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
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

#[test]
fn file_open_for_writing_is_skipped_as_in_use() {
    let fx = Fixture::new();
    fx.cache_branch_file("busy.bin", &content(MIB, 1), 1_000);
    fx.cache_branch_file("idle.bin", &content(MIB, 2), 2_000);
    let _writer = OpenOptions::new()
        .write(true)
        .open(fx.on_cache_branch("busy.bin"))
        .unwrap();

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

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

#[test]
fn file_open_only_for_reading_is_moved() {
    let fx = Fixture::new();
    fx.cache_branch_file("stream.bin", &content(MIB, 7), 1_000);
    let _reader = fs::File::open(fx.on_cache_branch("stream.bin")).unwrap();

    assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(!fx.on_cache_branch("stream.bin").exists());
    assert!(fs::read(fx.on_backing_branch("stream.bin")).unwrap() == content(MIB, 7));
}

#[test]
fn write_lease_skips_files_open_only_for_reading() {
    let fx = Fixture::new();
    fx.cache_branch_file("stream.bin", &content(MIB, 8), 1_000);
    fx.cache_branch_file("idle.bin", &content(MIB, 9), 2_000);
    let _reader = fs::File::open(fx.on_cache_branch("stream.bin")).unwrap();
    let config = fx.move_everything_config() + "lease_type: write\n";

    let err = assert_exit(&fx.run(&config), 0);

    assert!(err.contains("stream.bin: in use"), "stderr:\n{err}");
    assert!(fs::read(fx.on_cache_branch("stream.bin")).unwrap() == content(MIB, 8));
    assert!(!fx.on_backing_branch("stream.bin").exists());
    assert!(fs::read(fx.on_backing_branch("idle.bin")).unwrap() == content(MIB, 9));
}

/// Runs cachy on a fixture holding only `rel` and calls `during_copy` while
/// the run is paused between creating its temp file and finishing the copy.
/// Returns the run's output and when it resumed.
fn run_interrupted_mid_copy(
    fx: &Fixture,
    rel: &str,
    during_copy: impl FnOnce(),
) -> (Output, Instant) {
    let child = fx.spawn(&fx.move_everything_config());
    wait_for(&fx.staged_on_backing_branch(rel));
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
    fx.assert_no_staging_directories();
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
    fx.assert_no_staging_directories();
}

#[test]
fn renaming_the_parent_directory_during_copy_aborts_the_move() {
    let fx = Fixture::new();
    fx.cache_branch_file("show/episode.mkv", &content(LARGE, 10), 1_000);

    let (out, _) = run_interrupted_mid_copy(&fx, "show/episode.mkv", || {
        fs::rename(fx.on_cache_branch("show"), fx.on_cache_branch("show-s01")).unwrap();
    });

    let err = assert_exit(&out, 0);
    assert!(err.contains("show/episode.mkv: changed"), "stderr:\n{err}");
    assert!(
        fs::read(fx.on_cache_branch("show-s01/episode.mkv")).unwrap() == content(LARGE, 10),
        "the source changed"
    );
    assert!(!fx.on_backing_branch("show/episode.mkv").exists());
    assert!(!fx.on_backing_branch("show-s01/episode.mkv").exists());
}

/// Regression: a writer that resolved the path before the source was retired
/// must not write into an unlinked inode.
#[test]
fn write_while_the_retired_source_waits_rolls_the_move_back() {
    let fx = Fixture::new();
    fx.cache_branch_file("db/journal.log", &content(MIB, 6), 1_000);
    let looked_up = look_up(&fx.on_cache_branch("db/journal.log"));
    let child = fx.spawn(&fx.move_everything_config());

    wait_for(&fx.staged_on_cache_branch("db/journal.log"));
    reopen_for_append(&looked_up)
        .write_all(b"appended")
        .unwrap();

    let err = assert_exit(&child.wait_with_output().unwrap(), 0);
    assert!(err.contains("db/journal.log: in use"), "stderr:\n{err}");
    let mut expected = content(MIB, 6);
    expected.extend_from_slice(b"appended");
    assert!(
        fs::read(fx.on_cache_branch("db/journal.log")).unwrap() == expected,
        "the write was lost"
    );
    assert!(!fx.on_backing_branch("db/journal.log").exists());
    fx.assert_no_staging_directories();
}

#[test]
fn rollback_keeps_a_published_copy_that_changed_meanwhile() {
    let fx = Fixture::new();
    fx.cache_branch_file("db/journal.log", &content(MIB, 11), 1_000);
    let looked_up = look_up(&fx.on_cache_branch("db/journal.log"));
    let child = fx.spawn(&fx.move_everything_config());

    wait_for(&fx.staged_on_cache_branch("db/journal.log"));
    // The cache pool already shows the published copy, and someone edits it.
    OpenOptions::new()
        .append(true)
        .open(fx.on_backing_branch("db/journal.log"))
        .unwrap()
        .write_all(b"edited copy")
        .unwrap();
    reopen_for_append(&looked_up)
        .write_all(b"edited source")
        .unwrap();

    let err = assert_exit(&child.wait_with_output().unwrap(), 0);
    assert!(err.contains("db/journal.log: in use"), "stderr:\n{err}");
    let mut source = content(MIB, 11);
    source.extend_from_slice(b"edited source");
    assert!(fs::read(fx.on_cache_branch("db/journal.log")).unwrap() == source);
    let mut copy = content(MIB, 11);
    copy.extend_from_slice(b"edited copy");
    assert!(
        fs::read(fx.on_backing_branch("db/journal.log")).unwrap() == copy,
        "the edited copy was removed"
    );
}

#[test]
fn parent_directory_replaced_by_a_symlink_during_copy_aborts_the_move() {
    let fx = Fixture::new();
    fx.cache_branch_file("show/episode.mkv", &content(LARGE, 12), 1_000);

    let (out, _) = run_interrupted_mid_copy(&fx, "show/episode.mkv", || {
        fs::rename(fx.on_cache_branch("show"), fx.on_cache_branch("show-s01")).unwrap();
        std::os::unix::fs::symlink("show-s01", fx.on_cache_branch("show")).unwrap();
    });

    let err = assert_exit(&out, 0);
    assert!(err.contains("show/episode.mkv: changed"), "stderr:\n{err}");
    assert!(fs::read(fx.on_cache_branch("show-s01/episode.mkv")).unwrap() == content(LARGE, 12));
    assert!(!fx.on_backing_branch("show/episode.mkv").exists());
}

/// While one move's retired source waits, later moves go ahead; a write to
/// the retired source rolls back only its own move.
#[test]
fn write_to_a_retired_source_during_a_later_move_rolls_back_only_its_move() {
    let fx = Fixture::new();
    fx.cache_branch_file("a/first.log", &content(MIB, 13), 1_000);
    fx.cache_branch_file("b/second.bin", &content(LARGE, 14), 2_000);
    let looked_up = look_up(&fx.on_cache_branch("a/first.log"));
    let child = fx.spawn(&fx.move_everything_config());

    wait_for(&fx.staged_on_backing_branch("b/second.bin"));
    pause(&child);
    assert!(
        fx.staged_on_cache_branch("a/first.log").exists(),
        "the first move is not waiting in its retire"
    );
    let writer = thread::spawn(move || {
        reopen_for_append(&looked_up)
            .write_all(b"appended")
            .unwrap()
    });
    thread::sleep(Duration::from_millis(100));
    resume(&child);
    writer.join().unwrap();

    let err = assert_exit(&child.wait_with_output().unwrap(), 0);
    assert!(err.contains("a/first.log: in use"), "stderr:\n{err}");
    assert!(err.contains("1 moved, 1 skipped"), "stderr:\n{err}");
    let mut first = content(MIB, 13);
    first.extend_from_slice(b"appended");
    assert!(fs::read(fx.on_cache_branch("a/first.log")).unwrap() == first);
    assert!(!fx.on_backing_branch("a/first.log").exists());
    assert!(fs::read(fx.on_backing_branch("b/second.bin")).unwrap() == content(LARGE, 14));
    assert!(!fx.on_cache_branch("b/second.bin").exists());
    fx.assert_no_staging_directories();
}

/// An `O_PATH` descriptor names the inode without opening it, like a path
/// lookup that has finished before the open reaches the lease check.
fn look_up(path: &Path) -> OwnedFd {
    rustix::fs::open(path, OFlags::PATH | OFlags::CLOEXEC, Mode::empty()).unwrap()
}

/// Opens the file that `looked_up` names for appending; blocks while cachy
/// holds a lease on it.
fn reopen_for_append(looked_up: &OwnedFd) -> fs::File {
    OpenOptions::new()
        .append(true)
        .open(format!("/proc/self/fd/{}", looked_up.as_raw_fd()))
        .unwrap()
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

/// A move adopts an identical file already on the backing branch instead of
/// publishing its own copy. Rolling the move back keeps that file: cachy did
/// not write it.
#[test]
fn rollback_keeps_an_adopted_backing_file() {
    let fx = Fixture::new();
    fx.cache_branch_file("db/journal.log", &content(MIB, 15), 1_000);
    fx.backing_branch_file("db/journal.log", &content(MIB, 15), 1_000);
    let backing_inode = fs::metadata(fx.on_backing_branch("db/journal.log"))
        .unwrap()
        .ino();
    let looked_up = look_up(&fx.on_cache_branch("db/journal.log"));
    let child = fx.spawn(&fx.move_everything_config());

    wait_for(&fx.staged_on_cache_branch("db/journal.log"));
    reopen_for_append(&looked_up)
        .write_all(b"appended")
        .unwrap();

    let err = assert_exit(&child.wait_with_output().unwrap(), 0);
    assert!(err.contains("db/journal.log: in use"), "stderr:\n{err}");
    let mut source = content(MIB, 15);
    source.extend_from_slice(b"appended");
    assert!(fs::read(fx.on_cache_branch("db/journal.log")).unwrap() == source);
    let backing = fs::metadata(fx.on_backing_branch("db/journal.log"))
        .expect("the adopted backing file was removed");
    assert_eq!(backing.ino(), backing_inode);
    assert!(fs::read(fx.on_backing_branch("db/journal.log")).unwrap() == content(MIB, 15));
    fx.assert_no_staging_directories();
}

/// Rolling back a move that replaced a stale copy removes the published copy
/// but cannot bring the stale copy back: it was hidden behind the source, and
/// older. The source is then the only copy.
#[test]
fn rollback_after_replacing_a_stale_copy_leaves_only_the_source() {
    let fx = Fixture::new();
    fx.cache_branch_file("db/journal.log", &content(MIB, 16), 1_000);
    fx.backing_branch_file("db/journal.log", b"stale", 500);
    let looked_up = look_up(&fx.on_cache_branch("db/journal.log"));
    let child = fx.spawn(&fx.move_everything_config());

    wait_for(&fx.staged_on_cache_branch("db/journal.log"));
    reopen_for_append(&looked_up)
        .write_all(b"appended")
        .unwrap();

    let err = assert_exit(&child.wait_with_output().unwrap(), 0);
    assert!(err.contains("db/journal.log: in use"), "stderr:\n{err}");
    let mut source = content(MIB, 16);
    source.extend_from_slice(b"appended");
    assert!(fs::read(fx.on_cache_branch("db/journal.log")).unwrap() == source);
    assert!(!fx.on_backing_branch("db/journal.log").exists());
    fx.assert_no_staging_directories();
}
