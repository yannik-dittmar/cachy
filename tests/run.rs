mod common;

use std::ffi::OsStr;
use std::fs;
use std::time::{Duration, Instant};

use common::{
    Fixture, FsKind, assert_exit, free_space_lock, root_available, skip, sudo, zfs_dataset,
};

const MIB: u64 = 1 << 20;

/// Five 1 MiB files with distinct content, oldest first.
const FILES: [(&str, i64); 5] = [
    ("old/one.bin", 1_000),
    ("two.bin", 2_000),
    ("deep/er/three.bin", 3_000),
    ("four.bin", 4_000),
    ("old/five.bin", 5_000),
];

fn content(i: usize) -> Vec<u8> {
    (0..MIB as usize).map(|b| (b * 7 + i) as u8).collect()
}

fn populate(fx: &Fixture) {
    // Created newest first, so creation order cannot stand in for mtime order.
    for (i, (rel, mtime)) in FILES.iter().enumerate().rev() {
        fx.cache_branch_file(rel, &content(i), *mtime);
    }
}

fn assert_moved(fx: &Fixture, i: usize) {
    let rel = FILES[i].0;
    assert!(
        !fx.on_cache_branch(rel).exists(),
        "{rel} still on the cache branch"
    );
    assert!(
        fs::read(fx.on_backing_branch(rel)).unwrap() == content(i),
        "{rel} differs on the backing branch"
    );
}

fn assert_kept(fx: &Fixture, i: usize) {
    let rel = FILES[i].0;
    assert!(
        fs::read(fx.on_cache_branch(rel)).unwrap() == content(i),
        "{rel} changed on the cache branch"
    );
    assert!(
        !fx.on_backing_branch(rel).exists(),
        "{rel} appeared on the backing branch"
    );
}

#[test]
fn run_moves_oldest_files_until_free_space_reaches_target() {
    let _lock = free_space_lock();
    let fx = Fixture::new();
    populate(&fx);
    let free = fx.cache_branch_free();
    let config = fx.config(
        &(free + 1).to_string(),
        &(free + 2 * MIB + MIB / 2).to_string(),
    );

    let err = assert_exit(&fx.run(&config), 0);

    for i in 0..3 {
        assert_moved(&fx, i);
    }
    for i in 3..5 {
        assert_kept(&fx, i);
    }
    assert!(!err.contains("warning"), "stderr:\n{err}");
}

/// A file that a reader holds open keeps its space after its move, so free
/// space does not grow; the run still stops once the moves add up to its
/// byte budget, and warns.
#[test]
fn run_stops_once_its_byte_budget_is_consumed_even_if_free_space_does_not_grow() {
    let _lock = free_space_lock();
    let fx = Fixture::new();
    populate(&fx);
    let _readers: Vec<fs::File> = FILES
        .iter()
        .map(|(rel, _)| fs::File::open(fx.on_cache_branch(rel)).unwrap())
        .collect();
    let free = fx.cache_branch_free();
    let config = fx.config(
        &(free + 1).to_string(),
        &(free + 2 * MIB + MIB / 2).to_string(),
    );

    let err = assert_exit(&fx.run(&config), 0);

    for i in 0..3 {
        assert_moved(&fx, i);
    }
    for i in 3..5 {
        assert_kept(&fx, i);
    }
    assert!(err.contains("3 moved"), "stderr:\n{err}");
    assert!(
        err.contains("snapshots of the cache branch may be holding the space"),
        "stderr:\n{err}"
    );
}

#[test]
fn snapshot_holding_moved_files_ends_the_run_with_a_warning() {
    let _lock = free_space_lock();
    let fx = Fixture::new();
    if fx.cache_branch_fs != FsKind::Zfs {
        skip("the cache branch is not on ZFS");
        return;
    }
    if !root_available() {
        return;
    }
    populate(&fx);
    let _snapshot = Snapshot::of(fx.cache_branch.path());
    let free = fx.cache_branch_free();
    let config = fx.config(
        &(free + 1).to_string(),
        &(free + 2 * MIB + MIB / 2).to_string(),
    );

    let err = assert_exit(&fx.run(&config), 0);

    for i in 0..3 {
        assert_moved(&fx, i);
    }
    for i in 3..5 {
        assert_kept(&fx, i);
    }
    assert!(
        err.contains("snapshots of the cache branch may be holding the space"),
        "stderr:\n{err}"
    );
}

/// A ZFS snapshot of the dataset that holds a path, destroyed on drop.
struct Snapshot(String);

impl Snapshot {
    fn of(path: &std::path::Path) -> Snapshot {
        let name = format!("{}@cachy-test-{}", zfs_dataset(path), std::process::id());
        sudo(&[OsStr::new("zfs"), OsStr::new("snapshot"), OsStr::new(&name)]).unwrap();
        Snapshot(name)
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        if let Err(e) = sudo(&[
            OsStr::new("zfs"),
            OsStr::new("destroy"),
            OsStr::new(&self.0),
        ]) {
            eprintln!("cannot destroy snapshot {}: {e}", self.0);
        }
    }
}

/// A retired source waits about a second before it is unlinked; later moves
/// must not wait for it.
#[test]
fn many_small_files_move_without_waiting_for_each_retire() {
    // Writes on ZFS take up several times their size in free space until
    // they sync, so no test in this file writes while another measures.
    let _lock = free_space_lock();
    let fx = Fixture::new();
    for i in 0..20 {
        fx.cache_branch_file(&format!("small/{i}.txt"), b"tiny", 1_000 + i);
    }

    let started = Instant::now();
    let err = assert_exit(&fx.run(&fx.config("1T", "2T")), 0);
    let took = started.elapsed();

    assert!(err.contains("20 moved"), "stderr:\n{err}");
    assert!(took < Duration::from_secs(5), "20 moves took {took:?}");
    assert_eq!(Fixture::tree(fx.cache_branch.path()), ["small"]);
    fx.assert_no_staging_directories();
}

#[test]
fn check_at_threshold_moves_nothing() {
    let _lock = free_space_lock();
    let fx = Fixture::new();
    populate(&fx);
    let cache_branch_before = Fixture::tree(fx.cache_branch.path());
    let free = fx.cache_branch_free();
    let config = fx.config(&free.to_string(), &(free + 10 * MIB).to_string());

    assert_exit(&fx.run(&config), 0);

    for i in 0..FILES.len() {
        assert_kept(&fx, i);
    }
    assert_eq!(Fixture::tree(fx.cache_branch.path()), cache_branch_before);
    assert!(Fixture::tree(fx.backing_branch.path()).is_empty());
}

#[test]
fn successful_run_leaves_no_staging_directory() {
    let _lock = free_space_lock();
    let fx = Fixture::new();
    populate(&fx);
    let free = fx.cache_branch_free();
    let config = fx.config(&(free + 1).to_string(), &(free + 100 * MIB).to_string());

    assert_exit(&fx.run(&config), 0);

    for i in 0..FILES.len() {
        assert_moved(&fx, i);
    }
    assert_eq!(
        Fixture::tree(fx.backing_branch.path()),
        [
            "deep",
            "deep/er",
            "deep/er/three.bin",
            "four.bin",
            "old",
            "old/five.bin",
            "old/one.bin",
            "two.bin"
        ]
    );
    assert_eq!(
        Fixture::tree(fx.cache_branch.path()),
        ["deep", "deep/er", "old"]
    );
}

#[test]
fn publish_never_replaces_an_existing_backing_file() {
    let _lock = free_space_lock();
    let fx = Fixture::new();
    populate(&fx);
    fx.backing_branch_file("two.bin", b"already here", 42);
    let free = fx.cache_branch_free();
    let config = fx.config(&(free + 1).to_string(), &(free + 100 * MIB).to_string());

    let err = assert_exit(&fx.run(&config), 2);

    assert!(err.contains("two.bin"), "stderr:\n{err}");
    assert_eq!(fs::read(fx.on_cache_branch("two.bin")).unwrap(), content(1));
    assert_eq!(
        fs::read(fx.on_backing_branch("two.bin")).unwrap(),
        b"already here"
    );
    for i in [0, 2, 3, 4] {
        assert_moved(&fx, i);
    }
    fx.assert_no_staging_directories();
}

/// `bytes` as a percentage of the cache branch's size, to 12 decimals,
/// rounded down, or `None` if `bytes` exceed the size.
fn percent_of_cache_branch(fx: &Fixture, bytes: u64) -> Option<String> {
    let s = rustix::fs::statvfs(fx.cache_branch.path()).unwrap();
    let size = s.f_blocks as u128 * s.f_frsize as u128;
    if bytes as u128 > size {
        return None;
    }
    let scaled = bytes as u128 * 100 * 10u128.pow(12) / size;
    Some(format!(
        "{}.{:012}%",
        scaled / 10u128.pow(12),
        scaled % 10u128.pow(12)
    ))
}

#[test]
fn percentage_threshold_and_target_move_like_the_equivalent_byte_sizes() {
    let _lock = free_space_lock();
    let fx = Fixture::new();
    populate(&fx);
    let free = fx.cache_branch_free();
    let (threshold, target) = (free + MIB, free + 2 * MIB + MIB / 2);
    let in_bytes = fx.config(&threshold.to_string(), &target.to_string());
    let Some(target_percent) = percent_of_cache_branch(&fx, target) else {
        // A ZFS dataset's size is its own data plus its free space.
        skip("the target exceeds the cache branch's size");
        return;
    };
    let in_percent = fx.config(
        &percent_of_cache_branch(&fx, threshold).unwrap(),
        &target_percent,
    );
    let planned = |config: &str| {
        let out = common::output(fx.command(config).arg("--dry-run")).unwrap();
        assert_exit(&out, 0);
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("would "))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };

    assert_eq!(planned(&in_percent), planned(&in_bytes));
    assert_exit(&fx.run(&in_percent), 0);

    for i in 0..3 {
        assert_moved(&fx, i);
    }
    for i in 3..5 {
        assert_kept(&fx, i);
    }
}

/// A group of two names takes up its space once, so it consumes that much of
/// the byte budget, not twice as much.
#[test]
fn group_consumes_the_byte_budget_once() {
    let _lock = free_space_lock();
    let fx = Fixture::new();
    let content = content(0);
    fx.cache_branch_hardlinks(&["one.bin", "two.bin"], &content, 1_000);
    fx.cache_branch_file("next.bin", &content, 2_000);
    fx.cache_branch_file("last.bin", &content, 3_000);
    let free = fx.cache_branch_free();
    let config = fx.config(&(free + 1).to_string(), &(free + MIB + MIB / 2).to_string());

    let err = assert_exit(&fx.run(&config), 0);

    assert!(err.contains("2 moved"), "stderr:\n{err}");
    assert!(fx.on_backing_branch("one.bin").exists());
    assert!(fx.on_backing_branch("next.bin").exists());
    assert!(fx.on_cache_branch("last.bin").exists());
}
