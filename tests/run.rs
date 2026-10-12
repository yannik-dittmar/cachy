mod common;

use std::fs;

use common::{Fixture, Fs, assert_exit, free_space_lock};

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
    if fx.cache_branch_fs() == Fs::Zfs {
        // ZFS shows freed space only once its transaction group syncs, so a
        // run that re-measures free space after each move overshoots the
        // target. Runs will stop on a byte budget instead (#13).
        eprintln!("skipped: free space on ZFS lags behind moves");
        return;
    }
    populate(&fx);
    let free = fx.cache_branch_free();
    let config = fx.config(
        &(free + 1).to_string(),
        &(free + 2 * MIB + MIB / 2).to_string(),
    );

    assert_exit(&fx.run(&config), 0);

    for i in 0..3 {
        assert_moved(&fx, i);
    }
    for i in 3..5 {
        assert_kept(&fx, i);
    }
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
    assert!(!fx.on_backing_branch(".cachy-staging").exists());
    assert!(!fx.on_cache_branch(".cachy-staging").exists());
}
