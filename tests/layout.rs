//! Layout discovery: cachy reads the backing branches, their modes and
//! `minfreespace` from the cache pool's mergerfs runtime interface (the
//! `user.mergerfs.*` xattrs of `<cache pool>/.mergerfs`), falls back to an
//! explicit `backing_branches` list, refuses a cache pool whose first branch
//! is not the cache branch, and warns about mergerfs options that work
//! against it.
//!
//! [`FakeCachePool`] serves the runtime interface without mergerfs; the tests
//! against a real mergerfs mount run in the privileged tier.

mod common;

use std::fs;

use common::{FakeCachePool, Fixture, assert_exit};

const MOVEONENOSPC_WARNING: &str = "moveonenospc";
const MINFREESPACE_WARNING: &str = "below the cache pool's minfreespace";

fn branch(path: &std::path::Path, spec: &str) -> String {
    format!("{}={spec}", path.display())
}

#[test]
fn backing_branches_and_their_modes_are_discovered_from_the_cache_pool() {
    let fx = Fixture::new();
    let no_create = fx.extra_backing_branch();
    let pool = FakeCachePool::new(
        &[
            branch(fx.cache_branch.path(), "RW"),
            branch(no_create.path(), "NC"),
            branch(fx.backing_branch.path(), "RW"),
        ]
        .join(":"),
    );
    fx.cache_branch_file("media/a.bin", b"a", 1_000);
    let config = fx.layout_config(&format!("cache_pool: {}\n", pool.path().display()));

    let err = assert_exit(&fx.run(&config), 0);

    assert_eq!(
        fs::read(fx.on_backing_branch("media/a.bin")).unwrap(),
        b"a",
        "stderr:\n{err}"
    );
    assert!(Fixture::tree(no_create.path()).is_empty());
}

#[test]
fn cache_pool_whose_first_branch_is_not_the_cache_branch_is_refused() {
    let fx = Fixture::new();
    let pool = FakeCachePool::new(
        &[
            branch(fx.backing_branch.path(), "RW"),
            branch(fx.cache_branch.path(), "RW"),
        ]
        .join(":"),
    );
    fx.cache_branch_file("a.bin", b"a", 1_000);
    let config = fx.layout_config(&format!("cache_pool: {}\n", pool.path().display()));

    let err = assert_exit(&fx.run(&config), 1);

    assert!(
        err.contains(&format!(
            "the cache branch {} is not the first branch of the cache pool {}",
            fx.cache_branch.path().display(),
            pool.path().display()
        )),
        "stderr:\n{err}"
    );
    assert!(
        err.contains(&format!(
            "first branch is {}",
            fx.backing_branch.path().display()
        )),
        "stderr:\n{err}"
    );
    assert!(fx.on_cache_branch("a.bin").exists());
    assert!(Fixture::tree(fx.backing_branch.path()).is_empty());
}

#[test]
fn explicit_backing_branches_are_used_when_the_runtime_interface_is_unavailable() {
    let fx = Fixture::new();
    // A directory without a `.mergerfs` file, as a cache pool mounted with
    // `xattr=nosys` or not mounted at all looks.
    let not_a_pool = tempfile::tempdir().unwrap();
    fx.cache_branch_file("a.bin", b"a", 1_000);
    let config = fx.layout_config(&format!(
        "cache_pool: {}\nbacking_branches:\n  - {}\n",
        not_a_pool.path().display(),
        fx.backing_branch.path().display()
    ));

    let err = assert_exit(&fx.run(&config), 0);

    assert_eq!(fs::read(fx.on_backing_branch("a.bin")).unwrap(), b"a");
    assert!(
        err.contains("using backing_branches from the config"),
        "stderr:\n{err}"
    );
}

#[test]
fn unavailable_runtime_interface_without_backing_branches_is_refused() {
    let fx = Fixture::new();
    let not_a_pool = tempfile::tempdir().unwrap();
    let config = fx.layout_config(&format!("cache_pool: {}\n", not_a_pool.path().display()));

    let err = assert_exit(&fx.run(&config), 1);

    assert!(
        err.contains(&not_a_pool.path().join(".mergerfs").display().to_string()),
        "stderr:\n{err}"
    );
    assert!(err.contains("backing_branches"), "stderr:\n{err}");
}

#[test]
fn discovered_layout_takes_precedence_over_explicit_backing_branches() {
    let fx = Fixture::new();
    let listed = fx.extra_backing_branch();
    let pool = FakeCachePool::new(
        &[
            branch(fx.cache_branch.path(), "RW"),
            branch(fx.backing_branch.path(), "RW"),
        ]
        .join(":"),
    );
    fx.cache_branch_file("a.bin", b"a", 1_000);
    let config = fx.layout_config(&format!(
        "cache_pool: {}\nbacking_branches:\n  - {}\n",
        pool.path().display(),
        listed.path().display()
    ));

    assert_exit(&fx.run(&config), 0);

    assert!(fx.on_backing_branch("a.bin").exists());
    assert!(Fixture::tree(listed.path()).is_empty());
}

#[test]
fn moveonenospc_warning_fires_only_with_a_policy_that_can_choose_the_cache_branch() {
    for (moveonenospc, cache_mode, warns) in [
        ("pfrd", "RW", true),
        ("mfs", "RW", true),
        // mergerfs before 2.35 reports a plain `true`.
        ("true", "RW", true),
        ("false", "RW", false),
        // A policy that never chooses any branch.
        ("erofs", "RW", false),
        // Policies never choose a branch that takes no new files.
        ("pfrd", "NC", false),
        ("pfrd", "RO", false),
    ] {
        let fx = Fixture::new();
        let pool = FakeCachePool::new(
            &[
                branch(fx.cache_branch.path(), cache_mode),
                branch(fx.backing_branch.path(), "RW"),
            ]
            .join(":"),
        );
        pool.set("moveonenospc", moveonenospc);
        let config = fx.layout_config(&format!("cache_pool: {}\n", pool.path().display()));

        let err = assert_exit(&fx.run(&config), 0);

        assert_eq!(
            err.contains(MOVEONENOSPC_WARNING),
            warns,
            "moveonenospc={moveonenospc}, cache branch {cache_mode}; stderr:\n{err}"
        );
    }
}

#[test]
fn threshold_below_the_cache_branchs_minfreespace_warns() {
    // (global minfreespace, the cache branch's own, threshold, warns)
    for (global, own, threshold, warns) in [
        ("4G", "", "1G", true),
        ("4G", "", "4G", false),
        ("4G", "", "5G", false),
        // The cache branch's own minfreespace overrides the global one.
        ("4G", ",512M", "1G", false),
        ("1M", ",2G", "1G", true),
        ("4294967296", "", "1G", true),
    ] {
        let fx = Fixture::new();
        let pool = FakeCachePool::new(
            &[
                branch(fx.cache_branch.path(), &format!("RW{own}")),
                branch(fx.backing_branch.path(), "RW"),
            ]
            .join(":"),
        );
        pool.set("minfreespace", global);
        let config = fx
            .layout_config(&format!("cache_pool: {}\n", pool.path().display()))
            .replace("threshold: 1T", &format!("threshold: {threshold}"));

        let err = assert_exit(&fx.run(&config), 0);

        assert_eq!(
            err.contains(MINFREESPACE_WARNING),
            warns,
            "minfreespace {global}{own}, threshold {threshold}; stderr:\n{err}"
        );
    }
}

#[test]
fn malformed_runtime_interface_values_are_refused() {
    for (key, value, expected) in [
        ("branches", "/a=XX", "mode"),
        ("minfreespace", "lots", "minfreespace"),
    ] {
        let fx = Fixture::new();
        let pool = FakeCachePool::new(
            &[
                branch(fx.cache_branch.path(), "RW"),
                branch(fx.backing_branch.path(), "RW"),
            ]
            .join(":"),
        );
        pool.set(key, value);
        let config = fx.layout_config(&format!("cache_pool: {}\n", pool.path().display()));

        let err = assert_exit(&fx.run(&config), 1);

        assert!(err.contains(expected), "{key}={value}; stderr:\n{err}");
        assert!(
            err.contains(&format!("`{value}`")),
            "{key}={value}; stderr:\n{err}"
        );
    }
}

#[test]
fn real_cache_pool_layout_is_discovered_with_its_modes() {
    let fx = Fixture::new();
    let no_create = fx.extra_backing_branch();
    let Some(pool) = fx.cache_pool_with(
        &[
            branch(fx.cache_branch.path(), "RW"),
            branch(no_create.path(), "NC"),
            branch(fx.backing_branch.path(), "RW"),
        ]
        .join(":"),
        "moveonenospc=false",
    ) else {
        return;
    };
    pool.file("media/a.bin", b"a", 1_000);
    let config = fx.layout_config(&format!("cache_pool: {}\n", pool.mountpoint().display()));

    let err = assert_exit(&fx.run(&config), 0);

    assert_eq!(fs::read(fx.on_backing_branch("media/a.bin")).unwrap(), b"a");
    assert!(Fixture::tree(no_create.path()).is_empty());
    assert_eq!(fs::read(pool.path("media/a.bin")).unwrap(), b"a");
    assert!(!err.contains(MOVEONENOSPC_WARNING), "stderr:\n{err}");
    // The fixture's minfreespace is 1M, far below the threshold.
    assert!(!err.contains(MINFREESPACE_WARNING), "stderr:\n{err}");
}

#[test]
fn real_cache_pool_with_the_cache_branch_not_first_is_refused() {
    let fx = Fixture::new();
    let Some(pool) = fx.cache_pool_with(
        &format!(
            "{}:{}",
            fx.backing_branch.path().display(),
            fx.cache_branch.path().display()
        ),
        "",
    ) else {
        return;
    };
    let config = fx.layout_config(&format!("cache_pool: {}\n", pool.mountpoint().display()));

    let err = assert_exit(&fx.run(&config), 1);

    assert!(
        err.contains("is not the first branch of the cache pool"),
        "stderr:\n{err}"
    );
}

#[test]
fn real_cache_pool_warnings_fire_for_default_moveonenospc_and_large_minfreespace() {
    let fx = Fixture::new();
    // moveonenospc is on by default.
    let Some(pool) = fx.cache_pool_with(
        &format!(
            "{}=RW,8G:{}",
            fx.cache_branch.path().display(),
            fx.backing_branch.path().display()
        ),
        "",
    ) else {
        return;
    };
    let config = fx
        .layout_config(&format!("cache_pool: {}\n", pool.mountpoint().display()))
        .replace("threshold: 1T", "threshold: 1G");

    let err = assert_exit(&fx.run(&config), 0);

    assert!(err.contains(MOVEONENOSPC_WARNING), "stderr:\n{err}");
    assert!(err.contains(MINFREESPACE_WARNING), "stderr:\n{err}");
}
