//! Hardlink groups: the names of one inode move together as one move unit,
//! onto one backing branch, with all their links intact.

mod common;

const MIB: usize = 1 << 20;

use std::fs::{self, OpenOptions};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use common::{Fixture, assert_exit, set_times, set_xattr, wait_for, xattrs};

#[test]
fn group_moves_as_one_inode_with_all_its_links() {
    let _lock = common::free_space_lock();
    let fx = Fixture::new();
    let names = [
        "downloads/show.mkv",
        "media/show/episode.mkv",
        "media/copy.mkv",
    ];
    let content = vec![7; MIB];
    fx.cache_branch_hardlinks(&names, &content, 1_000);
    let free_before = fx.cache_branch_free();

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(
        err.contains("1 moved, 0 skipped, 0 failed"),
        "stderr:\n{err}"
    );
    assert!(
        fx.cache_branch_free() >= free_before + MIB as u64,
        "the group's space on the cache branch was not freed"
    );
    let first = fs::metadata(fx.on_backing_branch(names[0])).unwrap();
    assert_eq!(first.nlink(), 3);
    for name in names {
        assert!(
            fs::symlink_metadata(fx.on_cache_branch(name)).is_err(),
            "{name} is still on the cache branch"
        );
        let moved = fs::metadata(fx.on_backing_branch(name)).unwrap();
        assert_eq!(
            (moved.dev(), moved.ino()),
            (first.dev(), first.ino()),
            "{name} is not a link to the moved inode"
        );
        assert!(fs::read(fx.on_backing_branch(name)).unwrap() == content);
    }
    fx.assert_no_staging_directories();
}

#[test]
fn group_with_a_name_in_an_excluded_directory_is_skipped_and_left_unmodified() {
    let fx = Fixture::new();
    let names = ["appdata/seed.mkv", "media/b.mkv", "media/a.mkv"];
    fx.cache_branch_hardlinks(&names, b"episode", 1_000);
    fx.cache_branch_file("media/other.mkv", b"other", 2_000);
    let before = fs::metadata(fx.on_cache_branch(names[0])).unwrap();
    let config = fx.move_everything_config() + "exclude:\n  - /appdata/\n";

    let err = assert_exit(&fx.run(&config), 0);

    assert!(
        err.contains("skipped media/a.mkv, media/b.mkv: hardlink group partly excluded"),
        "stderr:\n{err}"
    );
    assert!(
        err.contains("1 moved, 1 skipped, 0 failed"),
        "stderr:\n{err}"
    );
    for name in names {
        let kept = fs::metadata(fx.on_cache_branch(name)).unwrap();
        assert_eq!(
            (kept.ino(), kept.nlink(), kept.mtime(), kept.ctime()),
            (before.ino(), 3, before.mtime(), before.ctime()),
            "{name} changed"
        );
        assert!(
            !fx.on_backing_branch(name).exists(),
            "{name} appeared on the backing branch"
        );
    }
    assert!(fx.on_backing_branch("media/other.mkv").exists());
    fx.assert_no_staging_directories();
}

#[test]
fn moved_group_keeps_mode_xattrs_and_timestamps() {
    let fx = Fixture::new();
    let names = ["a/file.bin", "b/link.bin"];
    fx.cache_branch_hardlinks(&names, b"content", 1_000);
    let source = fx.on_cache_branch(names[0]);
    set_xattr(&source, "user.comment", b"keep me").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o6740)).unwrap();
    set_times(&source, 500, 1_000);
    let before = fs::metadata(&source).unwrap();
    let xattrs_before = xattrs(&source);

    assert_exit(&fx.run(&fx.move_everything_config()), 0);

    for name in names {
        let moved = fx.on_backing_branch(name);
        let after = fs::metadata(&moved).unwrap();
        assert_eq!(after.mode(), 0o106_740, "{name}");
        assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
        assert_eq!((after.atime(), after.atime_nsec()), (500, 0), "{name}");
        assert_eq!((after.mtime(), after.mtime_nsec()), (1_000, 0), "{name}");
        assert_eq!(xattrs(&moved), xattrs_before, "{name}");
    }
}

#[test]
fn group_open_for_writing_through_any_name_is_skipped_as_in_use() {
    let fx = Fixture::new();
    let names = ["a/file.bin", "b/link.bin"];
    fx.cache_branch_hardlinks(&names, b"content", 1_000);
    let _writer = OpenOptions::new()
        .write(true)
        .open(fx.on_cache_branch(names[1]))
        .unwrap();

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(
        err.contains("skipped a/file.bin, b/link.bin: in use"),
        "stderr:\n{err}"
    );
    for name in names {
        assert_eq!(fs::read(fx.on_cache_branch(name)).unwrap(), b"content");
        assert!(!fx.on_backing_branch(name).exists(), "{name}");
    }
    assert_eq!(
        fs::metadata(fx.on_cache_branch(names[0])).unwrap().nlink(),
        2
    );
}

/// A stale copy at any name fails the publish, which leaves the group whole
/// on the cache branch and nothing of it on the backing branch.
#[test]
fn group_with_a_stale_copy_at_a_later_name_fails_and_is_rolled_back() {
    let fx = Fixture::new();
    let names = ["a/file.bin", "b/link.bin", "c/link.bin"];
    fx.cache_branch_hardlinks(&names, b"content", 1_000);
    fx.backing_branch_file(names[1], b"stale", 500);

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 2);

    assert!(
        err.contains("move of a/file.bin, b/link.bin, c/link.bin failed"),
        "stderr:\n{err}"
    );
    for name in names {
        assert_eq!(fs::read(fx.on_cache_branch(name)).unwrap(), b"content");
    }
    assert_eq!(
        fs::metadata(fx.on_cache_branch(names[0])).unwrap().nlink(),
        3
    );
    assert!(!fx.on_backing_branch(names[0]).exists());
    assert_eq!(fs::read(fx.on_backing_branch(names[1])).unwrap(), b"stale");
    assert!(!fx.on_backing_branch(names[2]).exists());
    fx.assert_no_staging_directories();
}

/// A link added to the source while its names wait in the staging directory
/// would keep the inode on the cache branch after the move: the move is
/// rolled back instead.
#[test]
fn link_added_while_the_group_waits_in_its_retire_rolls_the_move_back() {
    let fx = Fixture::new();
    let names = ["a/file.bin", "b/link.bin"];
    fx.cache_branch_hardlinks(&names, b"content", 1_000);
    let child = fx.spawn(&fx.move_everything_config());

    wait_for(&fx.staged_on_cache_branch(names[1]));
    fs::hard_link(
        fx.staged_on_cache_branch(names[1]),
        fx.on_cache_branch("late.bin"),
    )
    .unwrap();

    let err = assert_exit(&child.wait_with_output().unwrap(), 0);
    assert!(
        err.contains("skipped a/file.bin, b/link.bin: changed"),
        "stderr:\n{err}"
    );
    for name in [names[0], names[1], "late.bin"] {
        assert_eq!(fs::read(fx.on_cache_branch(name)).unwrap(), b"content");
    }
    for name in names {
        assert!(!fx.on_backing_branch(name).exists(), "{name}");
    }
    fx.assert_no_staging_directories();
}
