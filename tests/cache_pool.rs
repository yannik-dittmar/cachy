mod common;

use std::fs;

use common::{Fixture, assert_exit, free_space_lock};

#[test]
fn moved_file_stays_visible_through_the_cache_pool() {
    let _lock = free_space_lock();
    let fx = Fixture::new();
    let Some(pool) = fx.cache_pool() else { return };
    let content = b"written through the cache pool".repeat(1000);
    pool.file("media/clip.bin", &content, 1_000);
    assert!(
        fx.on_cache_branch("media/clip.bin").exists(),
        "the cache pool did not create the file on the cache branch"
    );
    let free = fx.cache_branch_free();
    let config = fx.config(&(free + 1).to_string(), &(free + (1 << 30)).to_string());

    assert_exit(&fx.run(&config), 0);

    assert!(!fx.on_cache_branch("media/clip.bin").exists());
    assert_eq!(
        fs::read(fx.on_backing_branch("media/clip.bin")).unwrap(),
        content
    );
    assert_eq!(fs::read(pool.path("media/clip.bin")).unwrap(), content);
}
