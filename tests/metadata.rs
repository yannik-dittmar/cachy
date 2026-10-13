//! Metadata preservation: a moved file keeps its owner, group, mode, xattrs,
//! ACLs and timestamps, and directories that cachy creates on a backing branch
//! take the cache directory's metadata.

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use common::{
    ACCESS_ACL, DEFAULT_ACL, Fixture, acl_granting, assert_exit, chmod_as_root, chown,
    root_available, set_times, set_xattr, skip, xattrs,
};

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode()
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn moved_file_keeps_mode_xattrs_and_timestamps() {
    let fx = Fixture::new();
    fx.cache_branch_file("dir/file.bin", b"content", 1_000);
    let source = fx.on_cache_branch("dir/file.bin");
    set_xattr(&source, "user.comment", b"keep me").unwrap();
    // Setuid and setgid, which a chown after the chmod would clear.
    set_mode(&source, 0o6740);
    set_times(&source, 500, 1_000);
    let before = fs::metadata(&source).unwrap();
    let xattrs_before = xattrs(&source);

    assert_exit(&fx.run(&fx.move_everything_config()), 0);

    let moved = fx.on_backing_branch("dir/file.bin");
    let after = fs::metadata(&moved).unwrap();
    assert_eq!(fs::read(&moved).unwrap(), b"content");
    assert_eq!(after.mode(), 0o106_740);
    assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
    assert_eq!((after.atime(), after.atime_nsec()), (500, 0));
    assert_eq!((after.mtime(), after.mtime_nsec()), (1_000, 0));
    assert_eq!(xattrs(&moved), xattrs_before);
    assert_eq!(
        xattrs(&moved).get("user.comment").map(Vec::as_slice),
        Some(&b"keep me"[..])
    );
}

#[test]
fn moved_file_keeps_its_owner_and_group() {
    let fx = Fixture::new();
    if !root_available() {
        return;
    }
    fx.cache_branch_file("dir/file.bin", b"content", 1_000);
    let source = fx.on_cache_branch("dir/file.bin");
    chown(&source, 1234, 5678);
    // A chown clears setuid; it must survive the move all the same.
    chmod_as_root(&source, 0o4644);

    assert_exit(&fx.run_as_root(&fx.move_everything_config()), 0);

    let moved = fs::metadata(fx.on_backing_branch("dir/file.bin")).unwrap();
    assert_eq!((moved.uid(), moved.gid()), (1234, 5678));
    assert_eq!(moved.mode(), 0o104_644);
}

/// Whether the filesystem of the directory `dir` stores ACLs, tried on a
/// scratch file in it.
fn supports_acls(dir: &Path) -> bool {
    let probe = dir.join(".acl-probe");
    fs::write(&probe, b"").unwrap();
    let supported = set_xattr(&probe, ACCESS_ACL, &acl_granting(4321)).is_ok();
    fs::remove_file(&probe).unwrap();
    supported
}

#[test]
fn moved_file_keeps_its_acl_and_gains_none() {
    let fx = Fixture::new();
    if !supports_acls(fx.cache_branch.path()) || !supports_acls(fx.backing_branch.path()) {
        skip("a branch's filesystem does not store ACLs");
        return;
    }
    fx.cache_branch_file("with-acl.bin", b"content", 1_000);
    fx.cache_branch_file("without-acl.bin", b"content", 1_000);
    let with_acl = fx.on_cache_branch("with-acl.bin");
    set_xattr(&with_acl, ACCESS_ACL, &acl_granting(4321)).unwrap();
    let without_acl_mode = mode(&fx.on_cache_branch("without-acl.bin"));
    // New files on the backing branch would inherit this.
    set_xattr(fx.backing_branch.path(), DEFAULT_ACL, &acl_granting(8765)).unwrap();

    assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert_eq!(
        xattrs(&fx.on_backing_branch("with-acl.bin")).get(ACCESS_ACL),
        Some(&acl_granting(4321))
    );
    let moved = fx.on_backing_branch("without-acl.bin");
    assert_eq!(xattrs(&moved).get(ACCESS_ACL), None);
    assert_eq!(mode(&moved), without_acl_mode);
}

/// An xattr of a kind that the backing branch cannot store but the cache
/// branch can, or `None` (with a note on stderr) if there is none.
fn unstorable(fx: &Fixture, kind: &str) -> Option<(&'static str, Vec<u8>)> {
    let (name, value) = match kind {
        // Too large for one ext4 block.
        "xattrs" => ("user.large", vec![b'x'; 8_000]),
        "acls" => (ACCESS_ACL, acl_granting(4321)),
        _ => unreachable!(),
    };
    let stores = |dir: &Path| {
        let probe = dir.join(".xattr-probe");
        fs::write(&probe, b"").unwrap();
        let stored = set_xattr(&probe, name, &value).is_ok();
        fs::remove_file(&probe).unwrap();
        stored
    };
    if !stores(fx.cache_branch.path()) || stores(fx.backing_branch.path()) {
        skip(&format!("no {kind} that only the cache branch can store"));
        return None;
    }
    Some((name, value))
}

fn unstorable_metadata_fails_the_move(kind: &str) {
    let fx = Fixture::new();
    let Some((name, value)) = unstorable(&fx, kind) else {
        return;
    };
    fx.cache_branch_file("dir/file.bin", b"content", 1_000);
    set_xattr(&fx.on_cache_branch("dir/file.bin"), name, &value).unwrap();

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 2);

    assert!(
        err.contains("move of dir/file.bin failed: metadata"),
        "stderr:\n{err}"
    );
    assert_eq!(
        fs::read(fx.on_cache_branch("dir/file.bin")).unwrap(),
        b"content"
    );
    assert!(!fx.on_backing_branch("dir/file.bin").exists());
    fx.assert_no_staging_directories();
}

fn allowed_metadata_loss_moves_with_a_warning(kind: &str) {
    let fx = Fixture::new();
    let Some((name, value)) = unstorable(&fx, kind) else {
        return;
    };
    fx.cache_branch_file("dir/file.bin", b"content", 1_000);
    set_xattr(&fx.on_cache_branch("dir/file.bin"), name, &value).unwrap();
    let config = fx.move_everything_config() + &format!("allow_metadata_loss: [{kind}]\n");

    let err = assert_exit(&fx.run(&config), 0);

    assert!(
        err.contains("warning") && err.contains(name) && err.contains("not preserved"),
        "stderr:\n{err}"
    );
    assert_eq!(
        fs::read(fx.on_backing_branch("dir/file.bin")).unwrap(),
        b"content"
    );
    assert!(!fx.on_cache_branch("dir/file.bin").exists());
}

#[test]
fn xattr_the_backing_branch_cannot_store_fails_the_move() {
    unstorable_metadata_fails_the_move("xattrs");
}

#[test]
fn acl_the_backing_branch_cannot_store_fails_the_move() {
    unstorable_metadata_fails_the_move("acls");
}

#[test]
fn allowed_xattr_loss_moves_with_a_warning() {
    allowed_metadata_loss_moves_with_a_warning("xattrs");
}

#[test]
fn allowed_acl_loss_moves_with_a_warning() {
    allowed_metadata_loss_moves_with_a_warning("acls");
}

/// Allowing one kind of loss does not allow the other.
#[test]
fn allowing_acl_loss_does_not_allow_xattr_loss() {
    let fx = Fixture::new();
    let Some((name, value)) = unstorable(&fx, "xattrs") else {
        return;
    };
    fx.cache_branch_file("file.bin", b"content", 1_000);
    set_xattr(&fx.on_cache_branch("file.bin"), name, &value).unwrap();
    let config = fx.move_everything_config() + "allow_metadata_loss: [acls]\n";

    let err = assert_exit(&fx.run(&config), 2);

    assert!(err.contains("failed: metadata"), "stderr:\n{err}");
    assert_eq!(
        fs::read(fx.on_cache_branch("file.bin")).unwrap(),
        b"content"
    );
}

#[test]
fn created_directories_take_the_cache_directory_metadata() {
    let fx = Fixture::new();
    fx.cache_branch_file("a/b/file.bin", b"content", 1_000);
    fx.cache_branch_file("a/other.bin", b"other", 1_000);
    let (a, b) = (fx.on_cache_branch("a"), fx.on_cache_branch("a/b"));
    set_xattr(&a, "user.dir", b"a").unwrap();
    if supports_acls(fx.cache_branch.path()) && supports_acls(fx.backing_branch.path()) {
        set_xattr(&a, DEFAULT_ACL, &acl_granting(4321)).unwrap();
    }
    set_mode(&a, 0o750);
    set_mode(&b, 0o2711);
    set_times(&b, 3_000, 2_000);

    assert_exit(&fx.run(&fx.move_everything_config()), 0);

    for rel in ["a", "a/b"] {
        let (cache_dir, created) = (fx.on_cache_branch(rel), fx.on_backing_branch(rel));
        let (before, after) = (
            fs::metadata(&cache_dir).unwrap(),
            fs::metadata(&created).unwrap(),
        );
        assert_eq!(after.mode(), before.mode(), "{rel}");
        assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
        // Moving the files out changed the cache directories' mtimes; the
        // created directories have the mtime they ended up with.
        assert_eq!(
            (after.mtime(), after.mtime_nsec()),
            (before.mtime(), before.mtime_nsec()),
            "{rel}"
        );
        assert_eq!(xattrs(&created), xattrs(&cache_dir), "{rel}");
    }
    assert_eq!(mode(&fx.on_backing_branch("a")), 0o040_750);
}

#[test]
fn existing_backing_directories_keep_their_metadata() {
    let fx = Fixture::new();
    fx.cache_branch_file("dir/file.bin", b"content", 1_000);
    set_xattr(&fx.on_cache_branch("dir"), "user.cache", b"cache").unwrap();
    set_mode(&fx.on_cache_branch("dir"), 0o750);
    let existing = fx.on_backing_branch("dir");
    fs::create_dir(&existing).unwrap();
    set_xattr(&existing, "user.backing", b"backing").unwrap();
    set_mode(&existing, 0o705);
    let xattrs_before = xattrs(&existing);

    assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert_eq!(
        fs::read(fx.on_backing_branch("dir/file.bin")).unwrap(),
        b"content"
    );
    assert_eq!(mode(&existing), 0o040_705);
    assert_eq!(xattrs(&existing), xattrs_before);
}

/// A stale copy with the source's content is kept as the published copy
/// only if its metadata matches too; otherwise keeping it would lose the
/// source's metadata, so it is replaced.
#[test]
fn stale_copy_with_other_metadata_is_replaced_not_kept() {
    let fx = Fixture::new();
    fx.cache_branch_file("same.bin", b"content", 1_000);
    fx.backing_branch_file("same.bin", b"content", 1_000);
    fx.cache_branch_file("other-mtime.bin", b"content", 1_000);
    fx.backing_branch_file("other-mtime.bin", b"content", 500);

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(
        err.contains(&format!(
            "kept identical stale copy {}",
            fx.on_backing_branch("same.bin").display()
        )),
        "stderr:\n{err}"
    );
    assert!(
        err.contains(&format!(
            "replaced stale copy {}",
            fx.on_backing_branch("other-mtime.bin").display()
        )),
        "stderr:\n{err}"
    );
    for rel in ["same.bin", "other-mtime.bin"] {
        assert!(!fx.on_cache_branch(rel).exists());
        let moved = fs::metadata(fx.on_backing_branch(rel)).unwrap();
        assert_eq!(moved.mtime(), 1_000, "{rel}");
    }
}
