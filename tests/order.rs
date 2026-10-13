//! The order in which a run takes move units: by the configured order key
//! (`atime` by default, or `mtime`), then by mtime, then by relpath.

mod common;

use std::process::Command;

use common::{Fixture, assert_exit, output, root_available, set_times};

/// The move units that a dry run with `config` would move, in order.
fn planned(fx: &Fixture, config: &str) -> (Vec<String>, String) {
    let out = output(fx.command(config).arg("--dry-run")).unwrap();
    let err = assert_exit(&out, 0);
    let planned = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("would move "))
        .map(str::to_owned)
        .collect();
    (planned, err)
}

/// Files whose atime order (`a` oldest) is the reverse of their mtime order
/// (`c` oldest).
fn accessed_in_reverse_of_modified(fx: &Fixture) {
    for (rel, atime, mtime) in [
        ("x/a.bin", 1_000, 3_000),
        ("b.bin", 2_000, 2_000),
        ("c.bin", 3_000, 1_000),
    ] {
        fx.cache_branch_file(rel, rel.as_bytes(), mtime);
        set_times(&fx.on_cache_branch(rel), atime, mtime);
    }
}

#[test]
fn move_units_are_taken_least_recently_accessed_first_by_default() {
    let fx = Fixture::new();
    accessed_in_reverse_of_modified(&fx);

    let (planned, _) = planned(&fx, &fx.move_everything_config());

    assert_eq!(planned, ["x/a.bin", "b.bin", "c.bin"]);
}

#[test]
fn order_mtime_takes_move_units_oldest_modified_first() {
    let fx = Fixture::new();
    accessed_in_reverse_of_modified(&fx);

    let (planned, _) = planned(&fx, &(fx.move_everything_config() + "order: mtime\n"));

    assert_eq!(planned, ["c.bin", "b.bin", "x/a.bin"]);
}

#[test]
fn equal_atimes_are_ordered_by_mtime_then_relpath_in_every_run() {
    let fx = Fixture::new();
    // Created in an order unrelated to the expected one.
    for (rel, mtime) in [
        ("m/z.bin", 2_000),
        ("b.bin", 2_000),
        ("m/a.bin", 2_000),
        ("a.bin", 2_000),
        ("late.bin", 3_000),
        ("early.bin", 1_000),
    ] {
        fx.cache_branch_file(rel, rel.as_bytes(), mtime);
        set_times(&fx.on_cache_branch(rel), 5_000, mtime);
    }
    let expected = [
        "early.bin",
        "a.bin",
        "b.bin",
        "m/a.bin",
        "m/z.bin",
        "late.bin",
    ];

    for _ in 0..3 {
        let (planned, _) = planned(&fx, &fx.move_everything_config());
        assert_eq!(planned, expected);
    }
}

#[test]
fn noatime_cache_branch_falls_back_to_mtime_with_one_warning() {
    if !root_available() {
        return;
    }
    let fx = Fixture::new();
    accessed_in_reverse_of_modified(&fx);
    let _noatime = NoatimeMount::over(fx.cache_branch.path());

    let (planned, err) = planned(&fx, &fx.move_everything_config());

    assert_eq!(planned, ["c.bin", "b.bin", "x/a.bin"]);
    assert_eq!(err.matches("noatime").count(), 1, "stderr:\n{err}");
}

/// A bind mount of a directory over itself without access-time updates,
/// unmounted on drop.
struct NoatimeMount(std::path::PathBuf);

impl NoatimeMount {
    fn over(dir: &std::path::Path) -> NoatimeMount {
        as_root(&[
            "mount",
            "--bind",
            dir.to_str().unwrap(),
            dir.to_str().unwrap(),
        ]);
        let mount = NoatimeMount(dir.to_owned());
        as_root(&["mount", "-o", "remount,bind,noatime", dir.to_str().unwrap()]);
        mount
    }
}

impl Drop for NoatimeMount {
    fn drop(&mut self) {
        as_root(&["umount", self.0.to_str().unwrap()]);
    }
}

/// Runs a command as root, through sudo unless the tests run as root. Check
/// [`root_available`] first.
fn as_root(args: &[&str]) {
    let mut command = if rustix::process::geteuid().is_root() {
        let mut command = Command::new(args[0]);
        command.args(&args[1..]);
        command
    } else {
        let mut command = Command::new("sudo");
        command.arg("-n").args(args);
        command
    };
    let out = output(&mut command).unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
