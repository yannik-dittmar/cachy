//! Stale copies: files on backing branches at the relpath of a move unit,
//! hidden from applications by the cache branch's copy. A move replaces the
//! stale copy on the branch it moves to, or keeps it if it is identical, and
//! removes the stale copies on other branches, so that afterwards exactly one
//! copy is left. It publishes first, then removes stale copies, then retires,
//! so the relpath never disappears from the cache pool.

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use common::{Fixture, assert_exit, pause, resume, wait_for};

const MIB: usize = 1 << 20;

fn layout(branches: &[String]) -> String {
    let mut layout = "backing_branches:\n".to_owned();
    for branch in branches {
        layout += &format!("  - {branch}\n");
    }
    layout
}

fn spec(path: &Path, suffix: &str) -> String {
    format!("{}{suffix}", path.display())
}

fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|b| (b * 7) as u8 ^ seed).collect()
}

/// The paths, among `paths`, at which something exists.
fn existing(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|p| fs::symlink_metadata(p).is_ok())
        .collect()
}

#[test]
fn differing_stale_copy_is_replaced() {
    let fx = Fixture::new();
    fx.cache_branch_file("media/a.bin", b"on the cache branch", 2_000);
    fx.backing_branch_file("media/a.bin", b"stale", 1_000);

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(
        err.contains("1 moved, 0 skipped, 0 failed"),
        "stderr:\n{err}"
    );
    assert!(
        err.contains(&format!(
            "replaced stale copy {}",
            fx.on_backing_branch("media/a.bin").display()
        )),
        "stderr:\n{err}"
    );
    assert!(!fx.on_cache_branch("media/a.bin").exists());
    let moved = fx.on_backing_branch("media/a.bin");
    assert_eq!(fs::read(&moved).unwrap(), b"on the cache branch");
    assert_eq!(fs::metadata(&moved).unwrap().mtime(), 2_000);
    fx.assert_no_staging_directories();
}

#[test]
fn identical_stale_copy_is_kept_and_the_cache_copy_retired() {
    let fx = Fixture::new();
    fx.cache_branch_file("media/a.bin", &content(MIB, 1), 1_000);
    fx.backing_branch_file("media/a.bin", &content(MIB, 1), 1_000);
    let before = fs::metadata(fx.on_backing_branch("media/a.bin")).unwrap();

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(
        err.contains("1 moved, 0 skipped, 0 failed"),
        "stderr:\n{err}"
    );
    assert!(
        err.contains(&format!(
            "kept identical stale copy {}",
            fx.on_backing_branch("media/a.bin").display()
        )),
        "stderr:\n{err}"
    );
    assert!(!fx.on_cache_branch("media/a.bin").exists());
    let after = fs::metadata(fx.on_backing_branch("media/a.bin")).unwrap();
    assert_eq!(
        (after.ino(), after.ctime(), after.ctime_nsec()),
        (before.ino(), before.ctime(), before.ctime_nsec()),
        "the stale copy was rewritten"
    );
    assert!(fs::read(fx.on_backing_branch("media/a.bin")).unwrap() == content(MIB, 1));
    fx.assert_no_staging_directories();
}

/// An identical stale copy is found by its size and content before the
/// copy: the move writes no copy of its own.
#[cfg(feature = "fault-injection")]
#[test]
fn identical_stale_copy_is_found_before_copying() {
    let fx = Fixture::new();
    fx.cache_branch_file("media/a.bin", &content(MIB, 2), 1_000);
    fx.backing_branch_file("media/a.bin", &content(MIB, 2), 1_000);
    let mut command = fx.command(&fx.move_everything_config());
    command.env("CACHY_FAULT_TRACE", "1");

    let out = common::output(&mut command).unwrap();

    let err = assert_exit(&out, 0);
    let written: Vec<_> = common::traced_operations(&err)
        .into_iter()
        .filter(|(_, op)| matches!(*op, "(create_new)" | "(write_at)"))
        .collect();
    assert!(written.is_empty(), "{written:?}; stderr:\n{err}");
    assert!(!fx.on_cache_branch("media/a.bin").exists());
}

#[test]
fn stale_copies_on_several_branches_all_disappear() {
    let fx = Fixture::new();
    let second = fx.extra_backing_branch();
    let third = fx.extra_backing_branch();
    let branches = [fx.backing_branch.path(), second.path(), third.path()];
    for (i, branch) in branches.iter().enumerate() {
        common::write_file(&branch.join("media/a.bin"), b"stale", 1_000 + i as i64);
    }
    fx.cache_branch_file("media/a.bin", b"on the cache branch", 2_000);
    let config = fx.layout_config(&layout(&branches.map(|branch| spec(branch, ""))));

    let err = assert_exit(&fx.run(&config), 0);

    assert!(
        err.contains("1 moved, 0 skipped, 0 failed"),
        "stderr:\n{err}"
    );
    assert_eq!(
        err.matches("replaced stale copy").count(),
        1,
        "stderr:\n{err}"
    );
    assert_eq!(
        err.matches("removed stale copy").count(),
        2,
        "stderr:\n{err}"
    );
    assert!(!fx.on_cache_branch("media/a.bin").exists());
    let copies = existing(branches.map(|branch| branch.join("media/a.bin")));
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(fs::read(&copies[0]).unwrap(), b"on the cache branch");
    for branch in branches {
        assert!(!branch.join(common::STAGING).exists());
    }
}

#[test]
fn stale_copy_on_a_branch_without_room_is_removed_once_the_move_unit_is_published_elsewhere() {
    let fx = Fixture::new();
    let full = fx.extra_backing_branch();
    common::write_file(&full.path().join("media/a.bin"), b"stale", 1_000);
    fx.cache_branch_file("media/a.bin", b"on the cache branch", 2_000);
    let config = fx.layout_config(&layout(&[
        spec(full.path(), "=RW,1000T"),
        spec(fx.backing_branch.path(), ""),
    ]));

    let err = assert_exit(&fx.run(&config), 0);

    assert!(
        err.contains(&format!(
            "removed stale copy {}",
            full.path().join("media/a.bin").display()
        )),
        "stderr:\n{err}"
    );
    assert!(!fx.on_cache_branch("media/a.bin").exists());
    assert!(!full.path().join("media/a.bin").exists());
    assert_eq!(
        fs::read(fx.on_backing_branch("media/a.bin")).unwrap(),
        b"on the cache branch"
    );
    assert!(!full.path().join(common::STAGING).exists());
}

/// cachy never writes to a branch that takes no new files, so it cannot
/// remove a stale copy there, and the move unit stays where it is.
#[test]
fn stale_copy_on_a_branch_that_takes_no_new_files_fails_the_move() {
    for mode in ["RO", "NC"] {
        let fx = Fixture::new();
        let frozen = fx.extra_backing_branch();
        common::write_file(&frozen.path().join("media/a.bin"), b"stale", 1_000);
        fx.cache_branch_file("media/a.bin", b"on the cache branch", 2_000);
        let config = fx.layout_config(&layout(&[
            spec(fx.backing_branch.path(), ""),
            spec(frozen.path(), &format!("={mode}")),
        ]));

        let err = assert_exit(&fx.run(&config), 2);

        assert!(
            err.contains(&format!(
                "move of media/a.bin failed: a stale copy of media/a.bin on backing branch {}, which takes no new files, cannot be removed",
                frozen.path().display()
            )),
            "{mode}; stderr:\n{err}"
        );
        assert_eq!(
            fs::read(fx.on_cache_branch("media/a.bin")).unwrap(),
            b"on the cache branch"
        );
        assert_eq!(
            fs::read(frozen.path().join("media/a.bin")).unwrap(),
            b"stale"
        );
        assert!(Fixture::tree(fx.backing_branch.path()).is_empty(), "{mode}");
    }
}

/// A stale copy that changed after the cache branch's copy, for example an
/// edit through the cache pool to a published copy whose move was then
/// undone, is never overwritten with the older content.
#[test]
fn stale_copy_newer_than_the_cache_copy_fails_the_move() {
    let fx = Fixture::new();
    fx.cache_branch_file("media/a.bin", b"older", 1_000);
    fx.backing_branch_file("media/a.bin", b"newer", 2_000);

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 2);

    assert!(
        err.contains(&format!(
            "move of media/a.bin failed: the stale copy {} is newer than the cache branch's copy",
            fx.on_backing_branch("media/a.bin").display()
        )),
        "stderr:\n{err}"
    );
    assert_eq!(
        fs::read(fx.on_cache_branch("media/a.bin")).unwrap(),
        b"older"
    );
    assert_eq!(
        fs::read(fx.on_backing_branch("media/a.bin")).unwrap(),
        b"newer"
    );
    fx.assert_no_staging_directories();
}

/// A directory at the relpath on a backing branch is not a stale copy: a
/// move never replaces or removes one.
#[test]
fn directory_at_the_relpath_on_a_backing_branch_fails_the_move() {
    let fx = Fixture::new();
    fx.cache_branch_file("media/a", b"a file", 1_000);
    fx.backing_branch_file("media/a/inside.bin", b"inside", 1_000);

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 2);

    assert!(err.contains("move of media/a failed"), "stderr:\n{err}");
    assert_eq!(fs::read(fx.on_cache_branch("media/a")).unwrap(), b"a file");
    assert_eq!(
        fs::read(fx.on_backing_branch("media/a/inside.bin")).unwrap(),
        b"inside"
    );
    fx.assert_no_staging_directories();
}

/// Runs cachy on `fx` and calls `during_copy` while the run is paused between
/// creating the temp file of `rel` and finishing its copy.
fn run_interrupted_mid_copy(fx: &Fixture, rel: &str, during_copy: impl FnOnce()) -> String {
    let child = fx.spawn(&fx.move_everything_config());
    wait_for(&fx.staged_on_backing_branch(rel));
    pause(&child);
    assert!(
        !fx.staged_on_cache_branch(rel).exists(),
        "the run was not caught mid-copy"
    );
    during_copy();
    resume(&child);
    assert_exit(&child.wait_with_output().unwrap(), 2)
}

/// The copy takes a while, and the stale copy that the move inspected at its
/// start may be replaced or changed meanwhile: the move then fails, and
/// leaves the new stale copy as it is.
#[test]
fn stale_copy_that_changes_during_the_move_is_not_overwritten() {
    const LARGE: usize = 8 * MIB;
    for replace in [true, false] {
        let fx = Fixture::new();
        let rel = "media/clip.bin";
        fx.cache_branch_file(rel, &content(LARGE, 3), 2_000);
        fx.backing_branch_file(rel, b"stale", 1_000);
        let stale = fx.on_backing_branch(rel);

        let err = run_interrupted_mid_copy(&fx, rel, || {
            if replace {
                let tmp = stale.with_extension("tmp");
                common::write_file(&tmp, b"changed", 1_000);
                fs::rename(&tmp, &stale).unwrap();
            } else {
                fs::OpenOptions::new()
                    .append(true)
                    .open(&stale)
                    .and_then(|mut f| std::io::Write::write_all(&mut f, b", changed"))
                    .unwrap();
            }
        });

        let context = format!("replace: {replace}; stderr:\n{err}");
        assert!(
            err.contains(&format!(
                "move of {rel} failed: the stale copy {} changed during the move",
                stale.display()
            )),
            "{context}"
        );
        let expected: &[u8] = if replace {
            b"changed"
        } else {
            b"stale, changed"
        };
        assert_eq!(fs::read(&stale).unwrap(), expected, "{context}");
        assert!(
            fs::read(fx.on_cache_branch(rel)).unwrap() == content(LARGE, 3),
            "{context}"
        );
        fx.assert_no_staging_directories();
    }
}

#[test]
fn hardlink_group_replaces_stale_copies_at_any_of_its_names() {
    let fx = Fixture::new();
    let names = ["a/file.bin", "b/link.bin", "c/link.bin"];
    fx.cache_branch_hardlinks(&names, b"content", 1_000);
    fx.backing_branch_file(names[0], b"stale first", 500);
    fx.backing_branch_file(names[1], b"stale second", 500);

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(
        err.contains("1 moved, 0 skipped, 0 failed"),
        "stderr:\n{err}"
    );
    assert_eq!(
        err.matches("replaced stale copy").count(),
        2,
        "stderr:\n{err}"
    );
    let first = fs::metadata(fx.on_backing_branch(names[0])).unwrap();
    assert_eq!(first.nlink(), 3);
    for name in names {
        assert!(!fx.on_cache_branch(name).exists());
        let moved = fs::metadata(fx.on_backing_branch(name)).unwrap();
        assert_eq!((moved.dev(), moved.ino()), (first.dev(), first.ino()));
        assert_eq!(fs::read(fx.on_backing_branch(name)).unwrap(), b"content");
    }
    fx.assert_no_staging_directories();
}

#[test]
fn symlink_replaces_a_differing_stale_copy() {
    let fx = Fixture::new();
    fx.backing_branch_file("dir/file-there", b"stale", 500);
    fs::create_dir_all(fx.on_cache_branch("dir")).unwrap();
    symlink("elsewhere", fx.on_backing_branch("dir/link-there")).unwrap();
    common::set_times(&fx.on_backing_branch("dir/link-there"), 500, 500);
    for rel in ["dir/file-there", "dir/link-there"] {
        symlink("target", fx.on_cache_branch(rel)).unwrap();
        common::set_times(&fx.on_cache_branch(rel), 1_000, 1_000);
    }

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(
        err.contains("2 moved, 0 skipped, 0 failed"),
        "stderr:\n{err}"
    );
    assert_eq!(
        err.matches("replaced stale copy").count(),
        2,
        "stderr:\n{err}"
    );
    for rel in ["dir/file-there", "dir/link-there"] {
        assert!(fs::symlink_metadata(fx.on_cache_branch(rel)).is_err());
        let moved = fx.on_backing_branch(rel);
        assert_eq!(fs::read_link(&moved).unwrap(), Path::new("target"));
        assert_eq!(fs::symlink_metadata(&moved).unwrap().mtime(), 1_000);
    }
    fx.assert_no_staging_directories();
}

/// Read through the cache pool, the relpath shows the cache branch's content
/// throughout the move: the move publishes, then removes stale copies, then
/// retires, so neither a missing file nor a stale copy ever shows. One
/// branch, listed before the one moved to, holds stale copies but has no
/// room; the other holds stale copies that are replaced in place.
#[test]
fn reads_through_the_cache_pool_never_miss_the_relpath_during_the_move() {
    let _lock = common::free_space_lock();
    let fx = Fixture::new();
    let full = fx.extra_backing_branch();
    let branches = format!(
        "{}:{}:{}",
        fx.cache_branch.path().display(),
        full.path().display(),
        fx.backing_branch.path().display()
    );
    let Some(cache_pool) = fx.cache_pool_with(&branches, "") else {
        return;
    };
    let mut rels = Vec::new();
    for i in 0..20 {
        let in_place = format!("in-place/{i}.bin");
        fx.backing_branch_file(&in_place, b"stale", 1_000);
        let elsewhere = format!("elsewhere/{i}.bin");
        common::write_file(&full.path().join(&elsewhere), b"stale", 1_000);
        for rel in [in_place, elsewhere] {
            fx.cache_branch_file(&rel, b"on the cache branch", 2_000);
            rels.push(rel);
        }
    }
    let config = fx.layout_config(&layout(&[
        spec(full.path(), "=RW,1000T"),
        spec(fx.backing_branch.path(), ""),
    ]));
    let done = AtomicBool::new(false);

    let (out, misreads) = thread::scope(|s| {
        let reader = s.spawn(|| {
            let mut misreads = Vec::new();
            while !done.load(Ordering::Relaxed) {
                for rel in &rels {
                    match fs::read(cache_pool.path(rel)) {
                        Ok(read) if read == b"on the cache branch" => {}
                        Ok(read) => {
                            misreads.push(format!("{rel}: {}", String::from_utf8_lossy(&read)))
                        }
                        Err(e) => misreads.push(format!("{rel}: {e}")),
                    }
                }
            }
            misreads
        });
        let out = fx.run(&config);
        done.store(true, Ordering::Relaxed);
        (out, reader.join().unwrap())
    });

    let err = assert_exit(&out, 0);
    assert!(err.contains("40 moved"), "stderr:\n{err}");
    assert!(misreads.is_empty(), "{misreads:?}");
    for rel in &rels {
        assert!(!fx.on_cache_branch(rel).exists());
        assert!(!full.path().join(rel).exists());
        assert_eq!(
            fs::read(fx.on_backing_branch(rel)).unwrap(),
            b"on the cache branch"
        );
    }
}
