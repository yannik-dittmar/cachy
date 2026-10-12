mod common;

use common::{Fixture, assert_exit, cachy};

#[test]
fn missing_key_is_refused_naming_the_key() {
    let fx = Fixture::new();
    let config = fx.config("1G", "2G").replace("target: 2G\n", "");

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("target"), "stderr:\n{err}");
}

#[test]
fn unknown_key_is_refused_naming_the_key() {
    let fx = Fixture::new();
    let config = fx.config("1G", "2G") + "treshold: 5G\n";

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("unknown field `treshold`"), "stderr:\n{err}");
}

#[test]
fn wrong_type_is_refused_naming_the_key() {
    let fx = Fixture::new();
    let config = fx.config("[1, 2]", "2G");

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("threshold"), "stderr:\n{err}");
    assert!(err.contains("byte size"), "stderr:\n{err}");
}

#[test]
fn target_equal_to_threshold_is_refused() {
    // 1T, 1024G, 1048576M and 1073741824K are all the same size.
    for (threshold, target) in [
        ("1T", "1024G"),
        ("1G", "1048576K"),
        ("1048576M", "1T"),
        ("1K", "1024"),
    ] {
        let fx = Fixture::new();

        let err = assert_exit(&fx.run(&fx.config(threshold, target)), 1);

        assert!(err.contains("target"), "stderr:\n{err}");
        assert!(err.contains("above the threshold"), "stderr:\n{err}");
    }
}

#[test]
fn target_below_threshold_is_refused() {
    let fx = Fixture::new();

    let err = assert_exit(&fx.run(&fx.config("2G", "1G")), 1);

    assert!(err.contains("above the threshold"), "stderr:\n{err}");
}

#[test]
fn more_than_one_backing_branch_is_refused() {
    let fx = Fixture::new();
    let config = fx
        .config("1G", "2G")
        .replace("backing_branches:\n", "backing_branches:\n  - /mnt/other\n");

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("backing_branches"), "stderr:\n{err}");
}

#[test]
fn empty_backing_branches_are_refused() {
    let fx = Fixture::new();
    let config = format!(
        "cache_branch: {}\nbacking_branches: []\nthreshold: 1G\ntarget: 2G\n",
        fx.cache_branch.path().display()
    );

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("backing_branches"), "stderr:\n{err}");
}

#[test]
fn missing_config_file_is_refused() {
    let err = assert_exit(&cachy(&["run", "--config", "/nonexistent/cachy.yml"]), 1);

    assert!(err.contains("/nonexistent/cachy.yml"), "stderr:\n{err}");
}

#[test]
fn missing_backing_branch_is_refused() {
    let fx = Fixture::new();
    let config = fx.config("1G", "2G").replace(
        &fx.backing_branch.path().display().to_string(),
        "/nonexistent/backing",
    );

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("/nonexistent/backing"), "stderr:\n{err}");
}

#[test]
fn malformed_byte_size_is_refused() {
    let fx = Fixture::new();
    let config = fx.config("1.5G", "2G");

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("threshold"), "stderr:\n{err}");
    assert!(err.contains("`1.5G`"), "stderr:\n{err}");
}
