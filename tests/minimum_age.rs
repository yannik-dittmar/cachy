//! The minimum age: move units changed too recently are never moved, guarding
//! against writers that hold no open file between writes.

mod common;

use std::fs;
use std::thread;
use std::time::Duration;

use common::{Fixture, assert_exit};

fn move_everything(fx: &Fixture, minimum_age: &str) -> String {
    fx.config("1T", "2T")
        .replace("minimum_age: 0s", &format!("minimum_age: {minimum_age}"))
}

#[test]
fn move_units_changed_within_the_minimum_age_are_skipped_as_too_young() {
    let fx = Fixture::new();
    fx.cache_branch_file("settled.bin", b"settled", 1_000);
    thread::sleep(Duration::from_millis(1_500));
    // Its mtime is old, but its ctime (which cannot be set back) is recent.
    fx.cache_branch_file("fresh.bin", b"fresh", 1_000);

    let err = assert_exit(&fx.run(&move_everything(&fx, "1s")), 0);

    assert!(err.contains("fresh.bin: too young"), "stderr:\n{err}");
    assert_eq!(fs::read(fx.on_cache_branch("fresh.bin")).unwrap(), b"fresh");
    assert!(!fx.on_backing_branch("fresh.bin").exists());
    assert_eq!(
        fs::read(fx.on_backing_branch("settled.bin")).unwrap(),
        b"settled"
    );
    assert!(!fx.on_cache_branch("settled.bin").exists());
}

#[test]
fn fresh_file_is_too_young_under_the_default_minimum_age() {
    let fx = Fixture::new();
    fx.cache_branch_file("fresh.bin", b"fresh", 1_000);
    let config = move_everything(&fx, "0s").replace("minimum_age: 0s\n", "");

    let err = assert_exit(&fx.run(&config), 0);

    assert!(err.contains("fresh.bin: too young"), "stderr:\n{err}");
    assert!(fx.on_cache_branch("fresh.bin").exists());
}
