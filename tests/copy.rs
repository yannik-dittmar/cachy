//! The copy a move makes: sparse files stay sparse, every byte arrives, and
//! each completed move reports the hash of its source.

mod common;

use std::fs;
use std::os::unix::fs::{FileExt, MetadataExt};

use common::{Fixture, assert_exit, skip};

const MIB: u64 = 1 << 20;

/// Space `path` takes up on its filesystem.
fn allocated(path: &std::path::Path) -> u64 {
    fs::metadata(path).unwrap().blocks() * 512
}

/// Creates `rel` on the cache branch as a 64 MiB file with 1 MiB of data at
/// its start and in its middle, holes in between, and a hole at its end.
/// Returns its content.
fn sparse_file(fx: &Fixture, rel: &str) -> Vec<u8> {
    let len = 64 * MIB;
    let data: Vec<u8> = (0..MIB).map(|b| (b % 251) as u8 + 1).collect();
    let mut expected = vec![0; len as usize];
    fx.cache_branch_file(rel, b"", 1_000);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(fx.on_cache_branch(rel))
        .unwrap();
    for offset in [0, 32 * MIB] {
        file.write_all_at(&data, offset).unwrap();
        expected[offset as usize..(offset + MIB) as usize].copy_from_slice(&data);
    }
    file.set_len(len).unwrap();
    drop(file);
    common::set_times(&fx.on_cache_branch(rel), 1_000, 1_000);
    expected
}

#[test]
fn sparse_file_moves_with_its_holes() {
    let fx = Fixture::new();
    let expected = sparse_file(&fx, "disk.img");
    if allocated(&fx.on_cache_branch("disk.img")) > 4 * MIB {
        skip("the cache branch does not store holes");
        return;
    }

    assert_exit(&fx.run(&fx.move_everything_config()), 0);

    let moved = fx.on_backing_branch("disk.img");
    assert!(fs::read(&moved).unwrap() == expected, "content differs");
    let taken = allocated(&moved);
    assert!(
        taken <= 4 * MIB,
        "2 MiB of data take up {taken} bytes on the backing branch"
    );
}

#[test]
fn dense_and_empty_files_move_intact() {
    let fx = Fixture::new();
    let dense: Vec<u8> = (0..3 * MIB + 17).map(|b| (b * 13 % 256) as u8).collect();
    fx.cache_branch_file("dense.bin", &dense, 1_000);
    fx.cache_branch_file("empty", b"", 1_000);

    assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(fs::read(fx.on_backing_branch("dense.bin")).unwrap() == dense);
    assert_eq!(fs::read(fx.on_backing_branch("empty")).unwrap(), b"");
    assert!(!fx.on_cache_branch("dense.bin").exists());
    assert!(!fx.on_cache_branch("empty").exists());
}

#[test]
fn every_moved_file_reports_the_hash_of_its_source() {
    let fx = Fixture::new();
    fx.cache_branch_file("hello.txt", b"hello, world\n", 1_000);
    fx.cache_branch_file("empty", b"", 1_000);
    let sparse = sparse_file(&fx, "dir/disk.img");

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);
    let branch = fx.backing_branch.path().display();

    // XXH3-128 of each content, computed independently with the reference
    // C library (Python's `xxhash`).
    for (rel, hash) in [
        ("hello.txt", "aef7d73364a11a632fc4faefcc422cb8"),
        ("empty", "99aa06d3014798d86001c324468d497f"),
    ] {
        assert!(
            err.contains(&format!(
                "cachy: info: moved {rel} to {branch} (xxh3 {hash})"
            )),
            "no hash for {rel}; stderr:\n{err}"
        );
    }
    let sparse_hash = format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&sparse));
    assert!(
        err.contains(&format!(
            "cachy: info: moved dir/disk.img to {branch} (xxh3 {sparse_hash})"
        )),
        "no hash for the sparse file; stderr:\n{err}"
    );
}
