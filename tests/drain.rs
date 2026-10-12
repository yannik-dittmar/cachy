//! `--drain`: a run without a check, which moves every eligible move unit.

mod common;

use std::fs::{self, OpenOptions};

use common::{Fixture, assert_exit, output};

fn eligible_and_in_use(fx: &Fixture) -> fs::File {
    fx.cache_branch_file("a.bin", b"a", 1_000);
    fx.cache_branch_file("deep/b.bin", b"b", 2_000);
    fx.cache_branch_file("in-use.bin", b"in use", 3_000);
    OpenOptions::new()
        .write(true)
        .open(fx.on_cache_branch("in-use.bin"))
        .unwrap()
}

#[test]
fn drain_moves_every_eligible_move_unit_although_free_space_is_above_the_threshold() {
    let fx = Fixture::new();
    let _writer = eligible_and_in_use(&fx);

    let mut command = fx.command(&fx.no_work_config());
    let err = assert_exit(&output(command.arg("--drain")).unwrap(), 0);

    assert!(err.contains("2 moved, 1 skipped"), "stderr:\n{err}");
    assert!(err.contains("in-use.bin: in use"), "stderr:\n{err}");
    assert_eq!(fs::read(fx.on_backing_branch("a.bin")).unwrap(), b"a");
    assert_eq!(fs::read(fx.on_backing_branch("deep/b.bin")).unwrap(), b"b");
    assert_eq!(
        Fixture::tree(fx.cache_branch.path()),
        ["deep", "in-use.bin"]
    );
    fx.assert_no_staging_directories();
}

#[test]
fn drain_dry_run_previews_a_full_drain_and_writes_nothing() {
    let fx = Fixture::new();
    let _writer = eligible_and_in_use(&fx);

    let mut command = fx.command(&fx.no_work_config());
    let out = output(command.args(["--drain", "--dry-run"])).unwrap();

    assert_exit(&out, 0);
    let planned = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        planned.lines().collect::<Vec<_>>(),
        [
            "byte budget: unlimited (drain)",
            "would move a.bin",
            "would move deep/b.bin",
            "would skip in-use.bin: in use",
        ],
        "stdout:\n{planned}"
    );
    assert_eq!(
        Fixture::tree(fx.cache_branch.path()),
        ["a.bin", "deep", "deep/b.bin", "in-use.bin"]
    );
    assert!(Fixture::tree(fx.backing_branch.path()).is_empty());
}
