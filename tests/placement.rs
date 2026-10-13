//! Placement across several backing branches: a move unit goes to a branch
//! that takes new files (not `RO` or `NC`), keeps its `minfreespace` free and
//! has room for the whole unit. Among those, a branch that already has the
//! unit's parent directory wins, then the one with the most free space.
//! Moves run one at a time per branch, and on different branches in
//! parallel.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use common::{Fixture, TmpfsBranch, assert_exit};

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

#[test]
fn read_only_and_no_create_branches_are_never_written() {
    let fx = Fixture::new();
    let read_only = fx.extra_backing_branch();
    let no_create = fx.extra_backing_branch();
    // Their parent directories would make them preferred, if they took new
    // files.
    for branch in [&read_only, &no_create] {
        fs::create_dir_all(branch.path().join("media/show")).unwrap();
    }
    fx.cache_branch_file("media/show/e1.mkv", b"e1", 1_000);
    fx.cache_branch_file("media/show/e2.mkv", b"e2", 1_001);
    fx.cache_branch_file("top.bin", b"top", 1_002);
    let config = fx.layout_config(&layout(&[
        spec(read_only.path(), "=RO"),
        spec(no_create.path(), "=NC"),
        spec(fx.backing_branch.path(), "=RW"),
    ]));

    assert_exit(&fx.run(&config), 0);

    for rel in ["media/show/e1.mkv", "media/show/e2.mkv", "top.bin"] {
        assert!(fx.on_backing_branch(rel).exists(), "{rel}");
    }
    for branch in [&read_only, &no_create] {
        assert_eq!(Fixture::tree(branch.path()), ["media", "media/show"]);
    }
}

#[test]
fn branch_that_would_fall_below_its_minfreespace_is_never_written() {
    let fx = Fixture::new();
    let full = fx.extra_backing_branch();
    fs::create_dir_all(full.path().join("media")).unwrap();
    fx.cache_branch_file("media/a.bin", b"a", 1_000);
    let config = fx.layout_config(&layout(&[
        spec(full.path(), "=RW,1000T"),
        spec(fx.backing_branch.path(), ""),
    ]));

    assert_exit(&fx.run(&config), 0);

    assert!(fx.on_backing_branch("media/a.bin").exists());
    assert_eq!(Fixture::tree(full.path()), ["media"]);
}

#[test]
fn move_unit_is_skipped_when_no_branch_has_room() {
    let fx = Fixture::new();
    fx.cache_branch_file("a.bin", b"a", 1_000);
    let config = fx.layout_config(&layout(&[spec(fx.backing_branch.path(), "=RW,1000T")]));

    let err = assert_exit(&fx.run(&config), 0);

    assert!(
        err.contains("skipped a.bin: no backing branch has room"),
        "stderr:\n{err}"
    );
    assert!(fx.on_cache_branch("a.bin").exists());
    assert!(Fixture::tree(fx.backing_branch.path()).is_empty());
}

#[test]
fn dry_run_reports_a_move_unit_without_room_as_skipped() {
    let fx = Fixture::new();
    fx.cache_branch_file("a.bin", b"a", 1_000);
    let config = fx.layout_config(&layout(&[spec(fx.backing_branch.path(), "=RW,1000T")]));
    let mut command = fx.command(&config);
    command.arg("--dry-run");

    let out = common::output(&mut command).unwrap();

    assert_exit(&out, 0);
    let planned = String::from_utf8(out.stdout).unwrap();
    assert!(
        planned.contains("would skip a.bin: no backing branch has room"),
        "stdout:\n{planned}"
    );
}

#[test]
fn move_unit_lands_on_the_branch_that_has_its_parent_directory() {
    let fx = Fixture::new();
    let other = fx.extra_backing_branch();
    fs::create_dir_all(other.path().join("media/show")).unwrap();
    fs::create_dir_all(fx.on_backing_branch("docs")).unwrap();
    fx.cache_branch_file("media/show/e1.mkv", b"e1", 1_000);
    fx.cache_branch_file("docs/a.txt", b"a", 1_001);
    fx.cache_branch_file("media/show/e2.mkv", b"e2", 1_002);
    let config = fx.layout_config(&layout(&[
        spec(fx.backing_branch.path(), ""),
        spec(other.path(), ""),
    ]));

    assert_exit(&fx.run(&config), 0);

    assert!(other.path().join("media/show/e1.mkv").exists());
    assert!(other.path().join("media/show/e2.mkv").exists());
    assert!(fx.on_backing_branch("docs/a.txt").exists());
    assert!(!fx.on_backing_branch("media").exists());
}

#[test]
fn move_units_in_a_directory_that_a_run_creates_stay_together() {
    let fx = Fixture::new();
    let other = fx.extra_backing_branch();
    let content = vec![7; MIB];
    for (i, name) in ["new/a.bin", "new/b.bin", "new/c.bin", "new/d.bin"]
        .iter()
        .enumerate()
    {
        fx.cache_branch_file(name, &content, 1_000 + i as i64);
    }
    let config = fx.layout_config(&layout(&[
        spec(fx.backing_branch.path(), ""),
        spec(other.path(), ""),
    ]));

    assert_exit(&fx.run(&config), 0);

    let on = |branch: &Path| {
        ["a", "b", "c", "d"]
            .iter()
            .filter(|n| branch.join(format!("new/{n}.bin")).exists())
            .count()
    };
    let counts = (on(fx.backing_branch.path()), on(other.path()));
    assert!(counts == (4, 0) || counts == (0, 4), "{counts:?}");
}

#[test]
fn hardlink_group_lands_on_one_branch_chosen_by_its_first_name() {
    let fx = Fixture::new();
    let other = fx.extra_backing_branch();
    fs::create_dir_all(other.path().join("a")).unwrap();
    fs::create_dir_all(fx.on_backing_branch("b")).unwrap();
    fx.cache_branch_hardlinks(&["a/x.bin", "b/y.bin"], b"linked", 1_000);
    let config = fx.layout_config(&layout(&[
        spec(fx.backing_branch.path(), ""),
        spec(other.path(), ""),
    ]));

    assert_exit(&fx.run(&config), 0);

    let x = fs::metadata(other.path().join("a/x.bin")).unwrap();
    let y = fs::metadata(other.path().join("b/y.bin")).unwrap();
    assert_eq!((x.dev(), x.ino()), (y.dev(), y.ino()));
    assert!(!fx.on_backing_branch("b/y.bin").exists());
}

#[test]
fn move_unit_lands_on_its_parents_branch_if_it_has_room_and_on_the_most_free_otherwise() {
    let fx = Fixture::new();
    let (Some(small), Some(large)) = (TmpfsBranch::mount("4M"), TmpfsBranch::mount("32M")) else {
        return;
    };
    for dir in ["fits", "too-big"] {
        fs::create_dir(small.path().join(dir)).unwrap();
    }
    fx.cache_branch_file("fits/a.bin", &vec![1; MIB], 1_000);
    fx.cache_branch_file("too-big/b.bin", &vec![2; 6 * MIB], 1_001);
    fx.cache_branch_file("nowhere/c.bin", &vec![3; MIB], 1_002);
    let config = fx.layout_config(&layout(&[spec(small.path(), ""), spec(large.path(), "")]));

    assert_exit(&fx.run(&config), 0);

    assert!(small.path().join("fits/a.bin").exists());
    assert!(large.path().join("too-big/b.bin").exists());
    assert!(large.path().join("nowhere/c.bin").exists());
    assert!(!small.path().join("nowhere").exists());
}

#[test]
fn move_unit_with_a_stale_copy_lands_on_the_branch_that_holds_it() {
    let fx = Fixture::new();
    let holder = fx.extra_backing_branch();
    fs::create_dir_all(fx.on_backing_branch("media")).unwrap();
    common::write_file(&holder.path().join("media/a.bin"), b"a", 1_000);
    fx.cache_branch_file("media/a.bin", b"a", 1_000);
    let config = fx.layout_config(&layout(&[
        spec(fx.backing_branch.path(), ""),
        spec(holder.path(), ""),
    ]));

    assert_exit(&fx.run(&config), 0);

    assert!(!fx.on_cache_branch("media/a.bin").exists());
    assert_eq!(fs::read(holder.path().join("media/a.bin")).unwrap(), b"a");
    assert!(!fx.on_backing_branch("media/a.bin").exists());
}

#[test]
fn space_of_a_move_unit_that_does_not_move_is_given_back() {
    let fx = Fixture::new();
    let Some(small) = TmpfsBranch::mount("4M") else {
        return;
    };
    fx.cache_branch_file("in-use.bin", &vec![1; 3 * MIB], 1_000);
    fx.cache_branch_file("free.bin", &vec![2; 3 * MIB], 1_001);
    let _writer = fs::OpenOptions::new()
        .write(true)
        .open(fx.on_cache_branch("in-use.bin"))
        .unwrap();
    let config = fx.layout_config(&layout(&[spec(small.path(), "")]));

    let err = assert_exit(&fx.run(&config), 0);

    assert!(err.contains("skipped in-use.bin: in use"), "stderr:\n{err}");
    assert!(small.path().join("free.bin").exists(), "stderr:\n{err}");
}

/// Moves on one branch never overlap, and moves on different branches do: a
/// fault-injection build slows every write down, and the staging directories
/// show, while it runs, how many temp files each branch holds at once.
#[cfg(feature = "fault-injection")]
#[test]
fn branches_take_one_move_at_a_time_and_move_in_parallel() {
    use std::time::Duration;

    let fx = Fixture::new();
    let other = fx.extra_backing_branch();
    fs::create_dir(fx.on_backing_branch("a")).unwrap();
    fs::create_dir(other.path().join("b")).unwrap();
    // Every move unit for one branch comes before every one for the other,
    // so the second branch starts only if the first one's queue does not
    // hold it up.
    let mut mtime = 1_000;
    for dir in ["a", "b"] {
        for i in 0..4 {
            fx.cache_branch_file(&format!("{dir}/{i}.bin"), b"content", mtime);
            mtime += 1;
        }
    }
    let config = fx.layout_config(&layout(&[
        spec(fx.backing_branch.path(), ""),
        spec(other.path(), ""),
    ]));
    let mut command = fx.command(&config);
    command.env("CACHY_WRITE_DELAY_MS", "100");

    let mut child = common::spawn(&mut command).unwrap();
    let branches = [
        fx.on_backing_branch(common::STAGING),
        other.path().join(common::STAGING),
    ];
    let (mut most, mut together) = ([0; 2], false);
    while child.try_wait().unwrap().is_none() {
        let temp_files = branches.clone().map(|staging| temp_files(&staging));
        for (most, now) in most.iter_mut().zip(temp_files) {
            *most = (*most).max(now);
        }
        together |= temp_files.iter().all(|&n| n > 0);
        std::thread::sleep(Duration::from_millis(1));
    }
    let out = child.wait_with_output().unwrap();

    assert_exit(&out, 0);
    assert_eq!(most, [1, 1], "temp files at once, per branch");
    assert!(together, "the branches never copied at the same time");
    for i in 0..4 {
        assert!(fx.on_backing_branch(&format!("a/{i}.bin")).exists());
        assert!(other.path().join(format!("b/{i}.bin")).exists());
    }
}

/// The regular files below the staging directory `dir`, which may vanish
/// while it is listed. Its own `.cachy-staging`, with the capability probe's
/// scratch files and the directories being created, holds no temp file.
#[cfg(feature = "fault-injection")]
fn temp_files(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name() != common::STAGING)
        .map(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => temp_files(&entry.path()),
            Ok(t) if t.is_file() => 1,
            _ => 0,
        })
        .sum()
}
