mod common;

use std::fs::{self, OpenOptions};

use common::{Fixture, assert_exit, free_space_lock};

const GIB: u64 = 1 << 30;

#[test]
fn moved_file_stays_visible_through_the_cache_pool() {
    let _lock = free_space_lock();
    let fx = Fixture::new();
    let Some(cache_pool) = fx.cache_pool() else {
        return;
    };
    let content = b"written through the cache pool".repeat(1000);
    cache_pool.file("media/clip.bin", &content, 1_000);
    assert!(
        fx.on_cache_branch("media/clip.bin").exists(),
        "the cache pool did not create the file on the cache branch"
    );
    let free = fx.cache_branch_free();
    let config = fx.config(&(free + 1).to_string(), &(free + GIB).to_string());

    assert_exit(&fx.run(&config), 0);

    assert!(
        !fx.on_cache_branch("media/clip.bin").exists(),
        "the file is still on the cache branch"
    );
    assert!(
        fs::read(fx.on_backing_branch("media/clip.bin")).unwrap() == content,
        "the file differs on the backing branch"
    );
    assert!(
        fs::read(cache_pool.path("media/clip.bin")).unwrap() == content,
        "the file differs through the cache pool"
    );
}

#[test]
fn file_open_for_writing_through_the_cache_pool_is_skipped_as_in_use() {
    let _lock = free_space_lock();
    let fx = Fixture::new();
    let Some(cache_pool) = fx.cache_pool() else {
        return;
    };
    cache_pool.file("downloads/partial.bin", b"first pieces", 1_000);
    let _writer = OpenOptions::new()
        .write(true)
        .open(cache_pool.path("downloads/partial.bin"))
        .unwrap();
    let free = fx.cache_branch_free();
    let config = fx.config(&(free + 1).to_string(), &(free + GIB).to_string());

    let err = assert_exit(&fx.run(&config), 0);

    assert!(
        err.contains("downloads/partial.bin: in use"),
        "stderr:\n{err}"
    );
    assert_eq!(
        fs::read(fx.on_cache_branch("downloads/partial.bin")).unwrap(),
        b"first pieces"
    );
    assert!(!fx.on_backing_branch("downloads/partial.bin").exists());
}
