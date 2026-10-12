//! Recovery: cachy starts every invocation by resolving the leftovers of
//! unfinished moves in the staging directories.

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use common::{Fixture, assert_exit, skip};

#[test]
fn retired_source_identical_to_its_published_copy_is_deleted() {
    let fx = Fixture::new();
    fx.staged_cache_branch_file("dir/file.bin", b"content", 1_000);
    fx.backing_branch_file("dir/file.bin", b"content", 1_000);

    assert_exit(&fx.run(&fx.no_work_config()), 0);

    assert_eq!(
        fs::read(fx.on_backing_branch("dir/file.bin")).unwrap(),
        b"content"
    );
    assert!(!fx.on_cache_branch("dir/file.bin").exists());
    fx.assert_no_staging_directories();
}

#[test]
fn retired_source_differing_from_its_published_copy_is_renamed_back() {
    let fx = Fixture::new();
    fx.staged_cache_branch_file("dir/file.bin", b"written while retiring", 1_000);
    fx.backing_branch_file("dir/file.bin", b"content", 1_000);

    assert_exit(&fx.run(&fx.no_work_config()), 0);

    assert_eq!(
        fs::read(fx.on_cache_branch("dir/file.bin")).unwrap(),
        b"written while retiring"
    );
    // Now a stale copy, hidden behind the cache branch's file.
    assert_eq!(
        fs::read(fx.on_backing_branch("dir/file.bin")).unwrap(),
        b"content"
    );
    fx.assert_no_staging_directories();
}

#[test]
fn retired_source_without_a_published_copy_is_renamed_back() {
    let fx = Fixture::new();
    fx.staged_cache_branch_file("gone/deeper/file.bin", b"content", 1_000);

    assert_exit(&fx.run(&fx.no_work_config()), 0);

    assert_eq!(
        fs::read(fx.on_cache_branch("gone/deeper/file.bin")).unwrap(),
        b"content"
    );
    assert!(!fx.on_backing_branch("gone").exists());
    fx.assert_no_staging_directories();
}

#[test]
fn temp_file_is_deleted() {
    let fx = Fixture::new();
    fx.cache_branch_file("dir/file.bin", b"content", 1_000);
    fx.staged_backing_branch_file("dir/file.bin", b"cont", 1_000);

    assert_exit(&fx.run(&fx.no_work_config()), 0);

    assert_eq!(
        fs::read(fx.on_cache_branch("dir/file.bin")).unwrap(),
        b"content"
    );
    assert!(!fx.on_backing_branch("dir/file.bin").exists());
    fx.assert_no_staging_directories();
}

/// A new file at the relpath blocks renaming the retired source back; both
/// are kept, and the run reports the leftover as unresolved.
#[test]
fn retired_source_that_cannot_be_renamed_back_is_kept() {
    let fx = Fixture::new();
    fx.staged_cache_branch_file("dir/file.bin", b"retired", 1_000);
    fx.cache_branch_file("dir/file.bin", b"created since", 1_000);

    let err = assert_exit(&fx.run(&fx.no_work_config()), 2);

    assert!(
        err.contains("cannot resolve staging leftover") && err.contains("dir/file.bin"),
        "stderr:\n{err}"
    );
    assert_eq!(
        fs::read(fx.staged_on_cache_branch("dir/file.bin")).unwrap(),
        b"retired"
    );
    assert_eq!(
        fs::read(fx.on_cache_branch("dir/file.bin")).unwrap(),
        b"created since"
    );
}

/// Only the staging directory at a branch's root is cachy's; a directory of
/// the same name deeper down is user data.
#[test]
fn recovery_touches_nothing_outside_the_staging_directories() {
    let fx = Fixture::new();
    fx.staged_cache_branch_file("a.bin", b"content", 1_000);
    fx.backing_branch_file("a.bin", b"content", 1_000);
    fx.staged_backing_branch_file("b.bin", b"partial", 1_000);
    fx.cache_branch_file("b.bin", b"complete", 1_000);
    fx.cache_branch_file("user/.cachy-staging/a.bin", b"user data", 1_000);
    fx.backing_branch_file("user/.cachy-staging/b.bin", b"user data", 1_000);
    fx.backing_branch_file("user/.cachy-staging/a.bin", b"user data", 1_000);
    let cache_before = Fixture::tree(fx.cache_branch.path());
    let backing_before = Fixture::tree(fx.backing_branch.path());

    assert_exit(&fx.run(&fx.no_work_config()), 0);

    let without_staging = |tree: Vec<String>| -> Vec<String> {
        tree.into_iter()
            .filter(|p| !p.starts_with(common::STAGING))
            .collect()
    };
    assert_eq!(
        Fixture::tree(fx.cache_branch.path()),
        without_staging(cache_before)
    );
    assert_eq!(
        Fixture::tree(fx.backing_branch.path()),
        without_staging(backing_before)
    );
    for (path, content) in [
        (fx.on_cache_branch("user/.cachy-staging/a.bin"), "user data"),
        (
            fx.on_backing_branch("user/.cachy-staging/a.bin"),
            "user data",
        ),
        (
            fx.on_backing_branch("user/.cachy-staging/b.bin"),
            "user data",
        ),
        (fx.on_backing_branch("a.bin"), "content"),
        (fx.on_cache_branch("b.bin"), "complete"),
    ] {
        assert_eq!(
            fs::read(&path).unwrap(),
            content.as_bytes(),
            "{}",
            path.display()
        );
    }
}

/// Listing a directory updates its atime (once its atime is older than its
/// mtime), so an unchanged atime shows that recovery never listed it.
#[test]
fn recovery_never_lists_backing_branch_directories_outside_staging() {
    let fx = Fixture::new();
    fx.staged_cache_branch_file("dir/same.bin", b"content", 1_000);
    fx.backing_branch_file("dir/same.bin", b"content", 1_000);
    fx.staged_cache_branch_file("dir/changed.bin", b"changed", 1_000);
    fx.backing_branch_file("dir/changed.bin", b"content", 1_000);
    fx.staged_backing_branch_file("dir/copying.bin", b"partial", 1_000);
    fx.backing_branch_file("other/file.bin", b"content", 1_000);
    let dirs = ["", "dir", "other"].map(|rel| fx.on_backing_branch(rel));
    for dir in &dirs {
        set_old_atime(dir);
    }
    let control = fx.on_backing_branch("control");
    fs::create_dir(&control).unwrap();
    set_old_atime(&control);
    fs::read_dir(&control).unwrap().for_each(drop);
    if atime(&control) == OLD_ATIME {
        skip("listing a directory does not update its atime on this filesystem");
        return;
    }

    assert_exit(&fx.run(&fx.no_work_config()), 0);

    for dir in &dirs {
        assert_eq!(atime(dir), OLD_ATIME, "{} was listed", dir.display());
    }
    fx.assert_no_staging_directories();
}

const OLD_ATIME: i64 = 1_000;

fn set_old_atime(path: &Path) {
    let mtime = fs::metadata(path).unwrap().mtime();
    rustix::fs::utimensat(
        rustix::fs::CWD,
        path,
        &rustix::fs::Timestamps {
            last_access: rustix::fs::Timespec {
                tv_sec: OLD_ATIME,
                tv_nsec: 0,
            },
            last_modification: rustix::fs::Timespec {
                tv_sec: mtime,
                tv_nsec: 0,
            },
        },
        rustix::fs::AtFlags::empty(),
    )
    .unwrap();
}

fn atime(path: &Path) -> i64 {
    fs::metadata(path).unwrap().atime()
}

/// A crash releases the lease that kept openers away from a retired source,
/// so one may be writing to it by the time recovery runs. Deleting it would
/// lose those writes, even if its content still matches the published copy.
#[test]
fn retired_source_open_for_writing_is_renamed_back_not_deleted() {
    let fx = Fixture::new();
    fx.staged_cache_branch_file("dir/file.bin", b"content", 1_000);
    fx.backing_branch_file("dir/file.bin", b"content", 1_000);
    let mut writer = fs::OpenOptions::new()
        .append(true)
        .open(fx.staged_on_cache_branch("dir/file.bin"))
        .unwrap();

    assert_exit(&fx.run(&fx.no_work_config()), 0);
    writer.write_all(b" appended").unwrap();
    drop(writer);

    assert_eq!(
        fs::read(fx.on_cache_branch("dir/file.bin")).unwrap(),
        b"content appended"
    );
    assert_eq!(
        fs::read(fx.on_backing_branch("dir/file.bin")).unwrap(),
        b"content"
    );
    fx.assert_no_staging_directories();
}

/// Deleting a retired source whose metadata differs from its published copy
/// would lose that metadata, even if the content is the same.
#[test]
fn retired_source_with_other_metadata_than_its_published_copy_is_renamed_back() {
    let fx = Fixture::new();
    fx.staged_cache_branch_file("dir/mtime.bin", b"content", 1_000);
    fx.backing_branch_file("dir/mtime.bin", b"content", 2_000);
    fx.staged_cache_branch_file("dir/mode.bin", b"content", 1_000);
    fx.backing_branch_file("dir/mode.bin", b"content", 1_000);
    fs::set_permissions(
        fx.on_backing_branch("dir/mode.bin"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();

    assert_exit(&fx.run(&fx.no_work_config()), 0);

    for rel in ["dir/mtime.bin", "dir/mode.bin"] {
        assert_eq!(fs::read(fx.on_cache_branch(rel)).unwrap(), b"content");
        assert_eq!(fs::read(fx.on_backing_branch(rel)).unwrap(), b"content");
    }
    assert_eq!(
        fs::metadata(fx.on_cache_branch("dir/mtime.bin"))
            .unwrap()
            .mtime(),
        1_000
    );
    fx.assert_no_staging_directories();
}

/// A retired source whose directory is gone from the cache branch goes back
/// into a recreated directory, which takes the metadata of its counterpart on
/// the backing branch.
#[test]
fn recreated_cache_directory_takes_the_backing_directory_metadata() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new();
    fx.staged_cache_branch_file("gone/file.bin", b"written while retiring", 1_000);
    fx.backing_branch_file("gone/file.bin", b"published", 1_000);
    let backing_dir = fx.on_backing_branch("gone");
    common::set_xattr(&backing_dir, "user.dir", b"gone").unwrap();
    fs::set_permissions(&backing_dir, fs::Permissions::from_mode(0o750)).unwrap();

    assert_exit(&fx.run(&fx.no_work_config()), 0);

    let recreated = fx.on_cache_branch("gone");
    assert_eq!(
        fs::read(recreated.join("file.bin")).unwrap(),
        b"written while retiring"
    );
    assert_eq!(fs::metadata(&recreated).unwrap().mode(), 0o040_750);
    assert_eq!(common::xattrs(&recreated), common::xattrs(&backing_dir));
    fx.assert_no_staging_directories();
}
