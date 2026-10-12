mod common;

use std::fs;

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
