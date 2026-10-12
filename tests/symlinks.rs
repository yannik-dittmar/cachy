//! Symlinks are move units: they arrive on the backing branch with their link
//! text verbatim, never rewritten, and keep their owner and timestamps.

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::Path;

use common::{Fixture, assert_exit, chown, root_available, set_times};

fn link_text(path: &Path) -> String {
    fs::read_link(path).unwrap().display().to_string()
}

#[test]
fn symlinks_arrive_with_identical_link_text() {
    let fx = Fixture::new();
    fx.cache_branch_file("media/show/episode.mkv", b"episode", 1_000);
    fs::create_dir_all(fx.on_cache_branch("links")).unwrap();
    let links = [
        ("links/relative", "../media/show/episode.mkv"),
        ("links/absolute", "/etc/hostname"),
        ("links/dangling", "../nowhere/at/all"),
        ("links/to-dir", "../media/show"),
        ("links/to-dir-slash", "../media/show/"),
    ];
    for (rel, target) in links {
        symlink(target, fx.on_cache_branch(rel)).unwrap();
        set_times(&fx.on_cache_branch(rel), 1_000, 1_000);
    }

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(
        err.contains("6 moved, 0 skipped, 0 failed"),
        "stderr:\n{err}"
    );
    for (rel, target) in links {
        assert!(
            fs::symlink_metadata(fx.on_cache_branch(rel)).is_err(),
            "{rel} is still on the cache branch"
        );
        let moved = fx.on_backing_branch(rel);
        assert!(
            fs::symlink_metadata(&moved)
                .unwrap()
                .file_type()
                .is_symlink(),
            "{rel} did not arrive as a symlink"
        );
        assert_eq!(link_text(&moved), target, "{rel}");
    }
    // The directory a symlink points at is not walked twice, nor followed.
    assert_eq!(
        fs::read(fx.on_backing_branch("links/to-dir/episode.mkv")).unwrap(),
        b"episode"
    );
    fx.assert_no_staging_directories();
}

#[test]
fn moved_symlink_keeps_its_timestamps() {
    let fx = Fixture::new();
    symlink("target.txt", fx.on_cache_branch("link")).unwrap();
    set_times(&fx.on_cache_branch("link"), 500, 1_000);
    let before = fs::symlink_metadata(fx.on_cache_branch("link")).unwrap();

    assert_exit(&fx.run(&fx.move_everything_config()), 0);

    let after = fs::symlink_metadata(fx.on_backing_branch("link")).unwrap();
    assert_eq!((after.atime(), after.atime_nsec()), (500, 0));
    assert_eq!((after.mtime(), after.mtime_nsec()), (1_000, 0));
    assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
}

#[test]
fn moved_symlink_keeps_its_owner_and_group() {
    let fx = Fixture::new();
    if !root_available() {
        return;
    }
    symlink("/etc/hostname", fx.on_cache_branch("link")).unwrap();
    chown(&fx.on_cache_branch("link"), 1234, 5678);
    set_times(&fx.on_cache_branch("link"), 1_000, 1_000);

    assert_exit(&fx.run_as_root(&fx.move_everything_config()), 0);

    let moved = fx.on_backing_branch("link");
    let after = fs::symlink_metadata(&moved).unwrap();
    assert_eq!((after.uid(), after.gid()), (1234, 5678));
    assert_eq!(after.mtime(), 1_000);
    assert_eq!(link_text(&moved), "/etc/hostname");
    // The symlink itself was chowned, not what it points at.
    assert_ne!(fs::metadata("/etc/hostname").unwrap().uid(), 1234);
}

#[test]
fn symlink_is_never_published_over_an_existing_file() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.on_cache_branch("dir")).unwrap();
    fx.backing_branch_file("dir/file-there", b"stale copy", 1_000);
    fs::create_dir_all(fx.on_backing_branch("dir")).unwrap();
    symlink("elsewhere", fx.on_backing_branch("dir/other-link-there")).unwrap();
    for rel in ["dir/file-there", "dir/other-link-there"] {
        symlink("target", fx.on_cache_branch(rel)).unwrap();
        set_times(&fx.on_cache_branch(rel), 1_000, 1_000);
    }

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 2);

    assert!(
        err.contains("move of dir/file-there failed: publish"),
        "stderr:\n{err}"
    );
    assert!(
        err.contains("move of dir/other-link-there failed: publish"),
        "stderr:\n{err}"
    );
    assert!(
        err.contains("0 moved, 0 skipped, 2 failed"),
        "stderr:\n{err}"
    );
    assert_eq!(link_text(&fx.on_cache_branch("dir/file-there")), "target");
    assert_eq!(
        link_text(&fx.on_cache_branch("dir/other-link-there")),
        "target"
    );
    assert_eq!(
        fs::read(fx.on_backing_branch("dir/file-there")).unwrap(),
        b"stale copy"
    );
    assert_eq!(
        link_text(&fx.on_backing_branch("dir/other-link-there")),
        "elsewhere"
    );
    fx.assert_no_staging_directories();
}

/// A run interrupted between publish and retire leaves the symlink on both
/// branches; the next move of it adopts the published one.
#[test]
fn identical_symlink_on_the_backing_branch_is_adopted() {
    let fx = Fixture::new();
    symlink("target", fx.on_cache_branch("link")).unwrap();
    symlink("target", fx.on_backing_branch("link")).unwrap();
    set_times(&fx.on_cache_branch("link"), 1_000, 1_000);
    set_times(&fx.on_backing_branch("link"), 1_000, 1_000);

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(err.contains("1 moved"), "stderr:\n{err}");
    assert!(fs::symlink_metadata(fx.on_cache_branch("link")).is_err());
    assert_eq!(link_text(&fx.on_backing_branch("link")), "target");
    fx.assert_no_staging_directories();
}

#[test]
fn symlink_younger_than_the_minimum_age_stays() {
    let fx = Fixture::new();
    symlink("target", fx.on_cache_branch("link")).unwrap();
    let config = fx
        .move_everything_config()
        .replace("minimum_age: 0s", "minimum_age: 1h");

    let err = assert_exit(&fx.run(&config), 0);

    assert!(err.contains("skipped link: too young"), "stderr:\n{err}");
    assert_eq!(link_text(&fx.on_cache_branch("link")), "target");
    assert!(fs::symlink_metadata(fx.on_backing_branch("link")).is_err());
}

/// Recovery resolves the symlinks that unfinished moves left in staging
/// directories: a retired symlink is deleted if its published copy is
/// identical, and renamed back otherwise; a temp file is deleted.
#[test]
fn recovery_resolves_symlink_leftovers() {
    let fx = Fixture::new();
    let staged = |path: std::path::PathBuf, target: &str| {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(target, &path).unwrap();
        set_times(&path, 1_000, 1_000);
    };
    staged(fx.staged_on_cache_branch("same"), "target");
    staged(fx.on_backing_branch("same"), "target");
    staged(fx.staged_on_cache_branch("dir/changed"), "new target");
    staged(fx.on_backing_branch("dir/changed"), "old target");
    staged(fx.staged_on_cache_branch("gone/unpublished"), "target");
    staged(fx.on_cache_branch("copying"), "target");
    staged(fx.staged_on_backing_branch("copying"), "target");

    let err = assert_exit(&fx.run(&fx.no_work_config()), 0);

    assert!(
        fs::symlink_metadata(fx.on_cache_branch("same")).is_err(),
        "stderr:\n{err}"
    );
    assert_eq!(link_text(&fx.on_backing_branch("same")), "target");
    assert_eq!(link_text(&fx.on_cache_branch("dir/changed")), "new target");
    assert_eq!(
        link_text(&fx.on_backing_branch("dir/changed")),
        "old target"
    );
    assert_eq!(link_text(&fx.on_cache_branch("gone/unpublished")), "target");
    assert_eq!(link_text(&fx.on_cache_branch("copying")), "target");
    assert!(fs::symlink_metadata(fx.on_backing_branch("copying")).is_err());
    fx.assert_no_staging_directories();
}
