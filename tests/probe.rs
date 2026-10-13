//! The capability probe: every invocation tries what cachy's guarantees
//! depend on, on scratch files in each filesystem's staging directory, and
//! refuses to run (exit 1) if a filesystem lacks it.
//!
//! The fault-injection build simulates missing capabilities:
//! `CACHY_PROBE_FAULT=<fault>@<operation>` fails the probe's operations of that
//! name (or its `n`th operation), and `CACHY_ZFS_VERSION=<version>` makes the
//! cache branch count as OpenZFS with that module version.

mod common;

use std::fs;
use std::path::Path;

use common::{Fixture, assert_exit, output};

/// Every entry under `root` with its content (empty for directories), sorted
/// by relpath.
fn contents(root: &Path) -> Vec<(String, Vec<u8>)> {
    Fixture::tree(root)
        .into_iter()
        .map(|rel| {
            let path = root.join(&rel);
            let content = if path.is_file() {
                fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            (rel, content)
        })
        .collect()
}

#[test]
fn probe_passes_and_leaves_no_scratch_files() {
    let fx = Fixture::new();
    fx.cache_branch_file("file.bin", b"content", 1_000);

    for config in [fx.no_work_config(), fx.move_everything_config()] {
        let err = assert_exit(&fx.run(&config), 0);
        assert!(!err.contains("probe"), "stderr:\n{err}");
        fx.assert_no_staging_directories();
    }
    assert_eq!(
        fs::read(fx.on_backing_branch("file.bin")).unwrap(),
        b"content"
    );
}

/// A dry run probes too, and leaves both branches as they were, including a
/// staging directory that holds a leftover of an unfinished move.
#[test]
fn dry_run_probes_and_writes_only_its_own_scratch_files() {
    let fx = Fixture::new();
    fx.cache_branch_file("file.bin", b"content", 1_000);
    fx.staged_cache_branch_file("dir/retired.bin", b"retired", 1_000);
    fx.backing_branch_file("existing.bin", b"existing", 1_000);
    let cache_before = contents(fx.cache_branch.path());
    let backing_before = contents(fx.backing_branch.path());

    let out = output(fx.command(&fx.move_everything_config()).arg("--dry-run")).unwrap();

    assert_exit(&out, 0);
    assert_eq!(contents(fx.cache_branch.path()), cache_before);
    assert_eq!(contents(fx.backing_branch.path()), backing_before);
}

#[cfg(feature = "fault-injection")]
mod simulated {
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    use super::*;

    const MIB: u64 = 1 << 20;

    fn with_probe_fault(mut command: Command, fault: &str) -> std::process::Output {
        output(command.env("CACHY_PROBE_FAULT", fault)).unwrap()
    }

    /// Asserts that a run refused before it changed anything: the cache
    /// branch's file is where it was, and no staging directory is left.
    fn assert_refused_untouched(fx: &Fixture, out: &std::process::Output) -> String {
        let err = assert_exit(out, 1);
        assert_eq!(
            fs::read(fx.on_cache_branch("file.bin")).unwrap(),
            b"content",
            "stderr:\n{err}"
        );
        assert!(!fx.on_backing_branch("file.bin").exists(), "stderr:\n{err}");
        fx.assert_no_staging_directories();
        err
    }

    #[test]
    fn missing_no_replace_rename_refuses_naming_the_filesystem() {
        let fx = Fixture::new();
        fx.cache_branch_file("file.bin", b"content", 1_000);

        for args in [&[][..], &["--dry-run"]] {
            let mut command = fx.command(&fx.move_everything_config());
            command.args(args);
            let out = with_probe_fault(command, "einval@rename_noreplace");

            let err = assert_refused_untouched(&fx, &out);
            for branch in [&fx.cache_branch, &fx.backing_branch] {
                assert!(
                    err.lines()
                        .any(|line| line.contains(&branch.path().display().to_string())
                            && line.contains("RENAME_NOREPLACE")),
                    "{args:?}: stderr:\n{err}"
                );
            }
        }
    }

    /// A move replaces a stale copy by exchanging it for its copy, so each
    /// backing branch that takes new files needs `RENAME_EXCHANGE`. The cache
    /// branch does not.
    #[test]
    fn missing_exchange_rename_on_a_backing_branch_refuses() {
        let fx = Fixture::new();
        fx.cache_branch_file("file.bin", b"content", 1_000);

        let out = with_probe_fault(
            fx.command(&fx.move_everything_config()),
            "einval@rename_exchange",
        );

        let err = assert_refused_untouched(&fx, &out);
        assert!(
            err.contains(&format!(
                "backing branch {} does not support exchange renames (RENAME_EXCHANGE)",
                fx.backing_branch.path().display()
            )),
            "stderr:\n{err}"
        );
        assert!(
            !err.contains(&fx.cache_branch.path().display().to_string()),
            "stderr:\n{err}"
        );
    }

    /// Leases are the guard unless the configuration chooses another, so
    /// their failure refuses even where `/proc` could tell what is in use.
    #[test]
    fn lease_failure_refuses() {
        for guard in ["", "open_file_guard: lease\n"] {
            for (lease_type, lease) in [("read", "read leases"), ("write", "write leases")] {
                let fx = Fixture::new();
                fx.cache_branch_file("file.bin", b"content", 1_000);
                let config =
                    fx.move_everything_config() + guard + &format!("lease_type: {lease_type}\n");

                let out = with_probe_fault(fx.command(&config), "einval@set_lease");

                let err = assert_refused_untouched(&fx, &out);
                assert!(
                    err.contains(&format!(
                        "cache branch {} does not support {lease}",
                        fx.cache_branch.path().display()
                    )),
                    "stderr:\n{err}"
                );
                assert!(err.contains("Invalid argument"), "stderr:\n{err}");
                assert!(
                    err.contains("unless open_file_guard is set to proc or off"),
                    "stderr:\n{err}"
                );
            }
        }
    }

    /// The probe checks that a scan of `/proc` finds a scratch file's opens;
    /// only the proc guard's check stats an open scratch file.
    #[test]
    fn proc_scan_failure_refuses() {
        let fx = Fixture::new();
        fx.cache_branch_file("file.bin", b"content", 1_000);
        let config = fx.move_everything_config() + "open_file_guard: proc\n";

        let out = with_probe_fault(fx.command(&config), "eio@fstat");

        let err = assert_refused_untouched(&fx, &out);
        assert!(
            err.contains(&format!(
                "cache branch {} does not support finding open files in /proc",
                fx.cache_branch.path().display()
            )),
            "stderr:\n{err}"
        );
    }

    #[test]
    fn explicit_proc_or_off_guard_runs_without_leases() {
        for guard in ["proc", "off"] {
            let fx = Fixture::new();
            fx.cache_branch_file("file.bin", b"content", 1_000);
            let config = fx.move_everything_config() + &format!("open_file_guard: {guard}\n");

            let out = with_probe_fault(fx.command(&config), "einval@set_lease");

            let err = assert_exit(&out, 0);
            assert!(err.contains("1 moved"), "stderr:\n{err}");
            assert_eq!(
                fs::read(fx.on_backing_branch("file.bin")).unwrap(),
                b"content"
            );
        }
    }

    /// Space `path` takes up on its filesystem.
    fn allocated(path: &Path) -> u64 {
        fs::metadata(path).unwrap().blocks() * 512
    }

    /// Creates `file.bin` on the cache branch as an 8 MiB file with 1 MiB of
    /// data at its start and a hole after it. Returns its content.
    fn sparse_file(fx: &Fixture) -> Vec<u8> {
        let data: Vec<u8> = (0..MIB).map(|b| (b % 251) as u8 + 1).collect();
        fx.cache_branch_file("file.bin", &data, 1_000);
        let file = fs::OpenOptions::new()
            .write(true)
            .open(fx.on_cache_branch("file.bin"))
            .unwrap();
        file.set_len(8 * MIB).unwrap();
        drop(file);
        common::set_times(&fx.on_cache_branch("file.bin"), 1_000, 1_000);
        let mut content = data;
        content.resize(8 * MIB as usize, 0);
        content
    }

    /// Asserts that a move of [`sparse_file`] wrote every byte of it, and
    /// that stderr warns that moves copy densely, for `reason`.
    fn assert_copied_densely(
        fx: &Fixture,
        out: &std::process::Output,
        content: &[u8],
        reason: &str,
    ) {
        let err = assert_exit(out, 0);
        fx.sync_backing_branch();
        let moved = fx.on_backing_branch("file.bin");
        assert!(fs::read(&moved).unwrap() == content, "content differs");
        assert!(
            allocated(&moved) >= 8 * MIB,
            "the copy takes up only {} bytes; stderr:\n{err}",
            allocated(&moved)
        );
        assert!(
            err.lines().any(|line| line.starts_with("cachy: warning:")
                && line.contains(reason)
                && line.contains("copy every byte")),
            "stderr:\n{err}"
        );
    }

    #[test]
    fn affected_openzfs_version_copies_densely_with_a_warning() {
        for version in ["2.2.0-1", "2.2.1-1ubuntu1"] {
            let fx = Fixture::new();
            let content = sparse_file(&fx);

            let out = output(
                fx.command(&(fx.move_everything_config() + "order: atime\n"))
                    .env("CACHY_ZFS_VERSION", version),
            )
            .unwrap();

            assert_copied_densely(&fx, &out, &content, &format!("OpenZFS {version}"));
        }
    }

    #[test]
    fn fixed_openzfs_version_keeps_holes() {
        let fx = Fixture::new();
        let content = sparse_file(&fx);
        if allocated(&fx.on_cache_branch("file.bin")) > 4 * MIB {
            common::skip("the cache branch does not store holes");
            return;
        }

        let out = output(
            fx.command(&(fx.move_everything_config() + "order: atime\n"))
                .env("CACHY_ZFS_VERSION", "2.2.2-1"),
        )
        .unwrap();

        let err = assert_exit(&out, 0);
        assert!(!err.contains("copy every byte"), "stderr:\n{err}");
        fx.sync_backing_branch();
        let moved = fx.on_backing_branch("file.bin");
        assert!(fs::read(&moved).unwrap() == content, "content differs");
        assert!(allocated(&moved) <= 4 * MIB, "stderr:\n{err}");
    }

    #[test]
    fn unreadable_openzfs_version_copies_densely_with_a_warning() {
        let fx = Fixture::new();
        let content = sparse_file(&fx);

        let out = output(
            fx.command(&(fx.move_everything_config() + "order: atime\n"))
                .env("CACHY_ZFS_VERSION", "unknown"),
        )
        .unwrap();

        assert_copied_densely(&fx, &out, &content, "OpenZFS module version");
    }

    #[test]
    fn failing_hole_reporting_copies_densely_with_a_warning() {
        let fx = Fixture::new();
        let content = sparse_file(&fx);

        let out = with_probe_fault(fx.command(&fx.move_everything_config()), "einval@seek_hole");

        assert_copied_densely(&fx, &out, &content, "hole reporting");
    }

    #[test]
    fn zfs_cache_branch_without_an_order_warns_that_atime_may_not_be_detectable() {
        let fx = Fixture::new();
        fx.cache_branch_file("file.bin", b"content", 1_000);
        // Whether a dry run with `config` warns about the order, with the
        // cache branch taken for ZFS if `zfs`.
        let warns = |config: String, zfs: bool| {
            let mut command = fx.command(&config);
            command.arg("--dry-run");
            if zfs {
                command.env("CACHY_ZFS_VERSION", "2.3.4-1");
            }
            let err = assert_exit(&output(&mut command).unwrap(), 0);
            err.lines().any(|line| {
                line.starts_with("cachy: warning:")
                    && line.contains("atime")
                    && line.contains("`order`")
            })
        };

        assert!(warns(fx.move_everything_config(), true));
        for order in ["atime", "mtime"] {
            let config = fx.move_everything_config() + &format!("order: {order}\n");
            assert!(!warns(config, true), "order: {order}");
        }
        // A check that finds no work orders nothing.
        assert!(!warns(fx.no_work_config(), true));
        if fx.cache_branch_fs != common::FsKind::Zfs {
            assert!(!warns(fx.move_everything_config(), false), "not on ZFS");
        }
    }

    /// Refusing to probe a filesystem without room for the scratch files would
    /// keep a full cache branch full; the run goes ahead with a warning.
    #[test]
    fn no_space_for_scratch_files_warns_and_runs() {
        for op in ["mkdir", "create_new", "write_at"] {
            let fx = Fixture::new();
            fx.cache_branch_file("file.bin", b"content", 1_000);

            let out = with_probe_fault(
                fx.command(&fx.move_everything_config()),
                &format!("enospc@{op}"),
            );

            let err = assert_exit(&out, 0);
            assert!(
                err.contains(&format!(
                    "cachy: warning: cannot probe cache branch {}: no space",
                    fx.cache_branch.path().display()
                )),
                "{op}: stderr:\n{err}"
            );
            assert_eq!(
                fs::read(fx.on_backing_branch("file.bin")).unwrap(),
                b"content"
            );
            fx.assert_no_staging_directories();
        }
    }

    #[test]
    fn other_errors_refuse() {
        let fx = Fixture::new();
        fx.cache_branch_file("file.bin", b"content", 1_000);

        let out = with_probe_fault(fx.command(&fx.move_everything_config()), "eio@create_new");

        let err = assert_refused_untouched(&fx, &out);
        assert!(
            err.contains(&format!(
                "cannot probe cache branch {}",
                fx.cache_branch.path().display()
            )),
            "stderr:\n{err}"
        );
    }

    /// The names of the files below `root`'s staging directory.
    fn staged_files(root: &Path) -> Vec<String> {
        let staging = root.join(common::STAGING);
        if !staging.exists() {
            return Vec::new();
        }
        Fixture::tree(&staging)
            .into_iter()
            .filter(|rel| !staging.join(rel).is_dir())
            .collect()
    }

    /// Calls `case(n)` for n = 1, 2, … until it returns false because the
    /// probe no longer reaches its `n`th operation, on several threads at
    /// once. Fails unless the probe reached more than a few operations.
    fn for_every_probe_operation(case: impl Fn(u64) -> bool + Sync) {
        const THREADS: u64 = 8;
        let next = AtomicU64::new(1);
        // The first operation that the probe did not reach.
        let end = AtomicU64::new(u64::MAX);
        thread::scope(|s| {
            for _ in 0..THREADS {
                s.spawn(|| {
                    loop {
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        assert!(n < 1_000, "the probe never ended");
                        if n >= end.load(Ordering::Relaxed) {
                            return;
                        }
                        if !case(n) {
                            end.fetch_min(n, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        let end = end.into_inner();
        assert!(end > 10, "the probe reached only {} operations", end - 1);
    }

    /// A crash or an error at any operation of the probe leaves scratch files
    /// behind at most until the next invocation, which removes them: neither a
    /// dry run nor recovery takes one for a leftover of a move.
    #[test]
    fn fault_at_every_probe_operation_is_undone_by_the_next_invocation() {
        for fault in ["kill", "eio"] {
            for_every_probe_operation(|n| {
                let fx = Fixture::new();
                fx.cache_branch_file("file.bin", b"content", 1_000);
                let config = fx.move_everything_config();

                let out = with_probe_fault(fx.command(&config), &format!("{fault}@{n}"));
                let err = common::stderr(&out);
                if !err.contains("cachy: fault injection:") {
                    assert_exit(&out, 0);
                    return false;
                }
                let context = format!("{fault}@{n}; stderr:\n{err}");

                let out = output(fx.command(&config).arg("--dry-run")).unwrap();
                let context = format!("{context}\nthen dry run; stderr:\n{}", common::stderr(&out));
                assert_eq!(out.status.code(), Some(0), "{context}");
                for root in [fx.cache_branch.path(), fx.backing_branch.path()] {
                    assert_eq!(staged_files(root), Vec::<String>::new(), "{context}");
                }

                let out = fx.run(&config);
                let context = format!("{context}\nthen ran; stderr:\n{}", common::stderr(&out));
                assert_eq!(out.status.code(), Some(0), "{context}");
                assert_eq!(
                    contents(fx.cache_branch.path()),
                    Vec::<(String, Vec<u8>)>::new(),
                    "{context}"
                );
                assert_eq!(
                    contents(fx.backing_branch.path()),
                    [("file.bin".to_owned(), b"content".to_vec())],
                    "{context}"
                );
                true
            });
        }
    }
}
