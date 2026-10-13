mod common;

use std::fs;

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
fn malformed_minimum_age_is_refused() {
    for age in ["15", "15x", "1.5h", "m", "-5m"] {
        let fx = Fixture::new();
        let config = fx
            .config("1G", "2G")
            .replace("minimum_age: 0s", &format!("minimum_age: {age}"));

        let err = assert_exit(&fx.run(&config), 1);

        assert!(err.contains("minimum_age"), "stderr:\n{err}");
        assert!(err.contains(&format!("`{age}`")), "stderr:\n{err}");
    }
}

#[test]
fn unknown_lease_type_is_refused() {
    let fx = Fixture::new();
    let config = fx.config("1G", "2G") + "lease_type: exclusive\n";

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("lease_type"), "stderr:\n{err}");
    assert!(err.contains("exclusive"), "stderr:\n{err}");
}

#[test]
fn malformed_byte_size_is_refused() {
    let fx = Fixture::new();
    let config = fx.config("1.5G", "2G");

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("threshold"), "stderr:\n{err}");
    assert!(err.contains("`1.5G`"), "stderr:\n{err}");
}

#[test]
fn unknown_metadata_loss_kind_is_refused() {
    let fx = Fixture::new();
    let config = fx.config("1G", "2G") + "allow_metadata_loss: [owner]\n";

    let err = assert_exit(&fx.run(&config), 1);

    assert!(err.contains("allow_metadata_loss"), "stderr:\n{err}");
    assert!(err.contains("owner"), "stderr:\n{err}");
}

#[test]
fn example_config_is_valid() {
    let example =
        fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/cachy.example.yml")).unwrap();
    let fx = Fixture::new();
    let config = example
        .replace("/mnt/cache", fx.cache_branch.path().to_str().unwrap())
        .replace("/mnt/storage", fx.backing_branch.path().to_str().unwrap());

    assert_exit(&fx.run(&config), 0);
}

#[test]
fn malformed_or_out_of_range_percentage_is_refused() {
    for percent in ["101%", "100.5%", "1e3%", "5.%", "-1%", "0.0000000000001%"] {
        let fx = Fixture::new();

        let err = assert_exit(&fx.run(&fx.config("10%", percent)), 1);

        assert!(err.contains("target"), "stderr:\n{err}");
        assert!(err.contains(&format!("`{percent}`")), "stderr:\n{err}");
    }
}

#[test]
fn percentage_target_not_above_percentage_threshold_is_refused() {
    for (threshold, target) in [("20%", "10%"), ("10%", "10.000%"), ("12.5%", "12.49%")] {
        let fx = Fixture::new();

        let err = assert_exit(&fx.run(&fx.config(threshold, target)), 1);

        assert!(err.contains("above the threshold"), "stderr:\n{err}");
    }
}

#[test]
fn percentage_threshold_above_a_byte_target_on_this_filesystem_is_refused() {
    let fx = Fixture::new();

    let err = assert_exit(&fx.run(&fx.config("100%", "1K")), 1);

    assert!(err.contains("above the threshold"), "stderr:\n{err}");
    assert!(err.contains("100%"), "stderr:\n{err}");
    assert!(Fixture::tree(fx.backing_branch.path()).is_empty());
}
