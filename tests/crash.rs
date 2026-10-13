//! Crash safety: a run killed at any filesystem operation, or failing at any
//! one with a syscall error, loses nothing, and the next run's recovery leaves
//! no leftover.
//!
//! These tests drive a `cachy` built with the test-only `fault-injection`
//! feature (`cargo test --features fault-injection`). `CACHY_FAULT=<fault>@<n>`
//! makes its `n`th filesystem operation kill the process (`kill`) or fail with
//! an errno (`eio`, `enospc`, `exdev`, `eintr`); a comma-separated list
//! injects several faults.

mod common;

use common::{Fixture, assert_exit};

#[cfg(not(feature = "fault-injection"))]
#[test]
fn fault_injection_is_unavailable_in_normal_builds() {
    let fx = Fixture::new();
    fx.cache_branch_file("file.bin", b"content", 1_000);

    let out = common::output(
        fx.command(&fx.move_everything_config())
            .env("CACHY_FAULT", "kill@1"),
    )
    .unwrap();

    assert_exit(&out, 0);
    assert_eq!(
        std::fs::read(fx.on_backing_branch("file.bin")).unwrap(),
        b"content"
    );
    common::skip("crash tests need `cargo test --features fault-injection`");
}

#[cfg(feature = "fault-injection")]
mod fault_injection {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::panic::{self, AssertUnwindSafe};
    use std::process::{Command, Output};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Condvar, LazyLock, Mutex};
    use std::thread;

    use super::*;

    const MIB: usize = 1 << 20;

    /// The move units of a representative run, oldest first: files in the
    /// branch root and in nested directories, an empty file, and a last file
    /// larger than one copy chunk, which is copied while the earlier moves
    /// wait in their retire.
    const FILES: [(&str, usize, i64); 4] = [
        ("notes.txt", 5, 1_000),
        ("media/cover.jpg", 300, 2_000),
        ("media/empty", 0, 3_000),
        ("media/show/episode.mkv", MIB + 100, 4_000),
    ];

    /// Files already on the backing branch, which no run may touch. One sits
    /// in a directory named like a staging directory.
    const BACKING_FILES: [&str; 2] = ["media/existing.txt", "other/.cachy-staging/user.txt"];

    /// The content of `FILES[i]`, generated once: in a debug build, generating
    /// it for every case would take longer than the runs.
    fn content(i: usize) -> &'static [u8] {
        static CONTENTS: LazyLock<Vec<Vec<u8>>> = LazyLock::new(|| {
            FILES
                .iter()
                .map(|&(_, len, seed)| (0..len).map(|b| (b * 7) as u8 ^ seed as u8).collect())
                .collect()
        });
        &CONTENTS[i]
    }

    /// The mode of each of `FILES`, and an xattr on it.
    fn mode(i: usize) -> u32 {
        [0o640, 0o4750, 0o600, 0o2644][i]
    }

    fn xattr(i: usize) -> (&'static str, Vec<u8>) {
        ("user.cachy-test", format!("file {i}").into_bytes())
    }

    fn create_representative_tree(fx: &Fixture) {
        for (i, (rel, _, mtime)) in FILES.into_iter().enumerate() {
            fx.cache_branch_file(rel, content(i), mtime);
            let path = fx.on_cache_branch(rel);
            let (name, value) = xattr(i);
            common::set_xattr(&path, name, &value).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode(i))).unwrap();
        }
        for rel in BACKING_FILES {
            fx.backing_branch_file(rel, rel.as_bytes(), 1_000);
        }
    }

    /// Runs `command` with `faults` (`<fault>@<n>[,…]`) injected. Returns
    /// the output and whether the run reached every faulty operation.
    ///
    /// Fails if a run that did not reach them skipped a move unit: it ended
    /// early, not for lack of operations, and [`for_every_operation`] would
    /// stop short of the operations after it.
    fn run_with_faults(mut command: Command, faults: &str) -> (Output, bool) {
        let out = common::output(command.env("CACHY_FAULT", faults)).unwrap();
        let err = common::stderr(&out);
        let injected = err.matches("cachy: fault injection:").count();
        let reached = injected == faults.split(',').count();
        assert!(
            reached || !err.contains("cachy: skipped"),
            "{faults}: the run skipped a move unit; stderr:\n{err}"
        );
        (out, reached)
    }

    /// `cachy run` with a retire wait short enough that hundreds of runs do
    /// not add up to minutes.
    fn with_short_retire_wait(fx: &Fixture, config: &str) -> Command {
        let mut command = fx.command(config);
        command.env("CACHY_RETIRE_WAIT_MS", "0");
        command
    }

    /// How many cases may run at once, across all tests: each holds a fixture
    /// on the cache branch's filesystem, which may be a small tmpfs.
    const SLOTS: usize = 16;
    static SLOTS_TAKEN: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

    fn in_slot<T>(f: impl FnOnce() -> T) -> T {
        let (taken, freed) = &SLOTS_TAKEN;
        let mut n = freed
            .wait_while(taken.lock().unwrap(), |n| *n >= SLOTS)
            .unwrap();
        *n += 1;
        drop(n);
        struct Release;
        impl Drop for Release {
            fn drop(&mut self) {
                let (taken, freed) = &SLOTS_TAKEN;
                *taken.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
                freed.notify_one();
            }
        }
        let _release = Release;
        f()
    }

    /// Calls `case(n)` for n = 1, 2, … until it returns false because the run
    /// no longer reaches the `n`th operation, on several threads at once.
    /// Fails if not even the first operation is reached.
    fn for_every_operation(case: impl Fn(u64) -> bool + Sync) {
        const THREADS: usize = 8;
        let next = AtomicU64::new(1);
        // The first operation that the run did not reach.
        let end = AtomicU64::new(u64::MAX);
        thread::scope(|s| {
            for _ in 0..THREADS {
                s.spawn(|| {
                    loop {
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        assert!(n < 10_000, "the run never ended");
                        if n >= end.load(Ordering::Relaxed) {
                            return;
                        }
                        match panic::catch_unwind(AssertUnwindSafe(|| in_slot(|| case(n)))) {
                            Ok(true) => {}
                            Ok(false) => {
                                end.fetch_min(n, Ordering::Relaxed);
                            }
                            Err(e) => {
                                // Stops the other threads too.
                                end.store(0, Ordering::Relaxed);
                                panic::resume_unwind(e);
                            }
                        }
                    }
                });
            }
        });
        assert!(
            end.into_inner() > 1,
            "no fault was injected: is cachy built with fault injection?"
        );
    }

    /// The core invariant: each relpath exists exactly once, on the cache
    /// branch or the backing branch, with its content and metadata, and the
    /// backing branch's own files are untouched.
    fn assert_each_exactly_once(fx: &Fixture, context: &str) {
        for (i, (rel, _, _)) in FILES.into_iter().enumerate() {
            let copies: Vec<_> = [fx.on_cache_branch(rel), fx.on_backing_branch(rel)]
                .into_iter()
                .filter(|p| p.exists())
                .collect();
            assert!(
                copies.len() == 1,
                "{context}: {rel} exists {} times",
                copies.len()
            );
            assert!(
                fs::read(&copies[0]).unwrap() == content(i),
                "{context}: {} differs",
                copies[0].display()
            );
            let meta = fs::metadata(&copies[0]).unwrap();
            assert_eq!(
                (meta.mode() & 0o7777, meta.mtime(), meta.uid(), meta.gid()),
                (
                    mode(i),
                    FILES[i].2,
                    rustix::process::geteuid().as_raw(),
                    rustix::process::getegid().as_raw()
                ),
                "{context}: {} has other metadata",
                copies[0].display()
            );
            let (name, value) = xattr(i);
            assert_eq!(
                common::xattrs(&copies[0]).get(name),
                Some(&value),
                "{context}: {} lost its xattr",
                copies[0].display()
            );
        }
        for rel in BACKING_FILES {
            assert_eq!(
                fs::read(fx.on_backing_branch(rel)).unwrap(),
                rel.as_bytes(),
                "{context}: {rel} changed"
            );
        }
    }

    fn assert_no_staging_directories(fx: &Fixture, context: &str) {
        assert!(
            !fx.staged_on_cache_branch("").exists() && !fx.staged_on_backing_branch("").exists(),
            "{context}: a staging directory is left"
        );
    }

    #[test]
    fn crash_at_every_operation_is_recovered_by_the_next_run() {
        for_every_operation(|n| {
            let fx = Fixture::new();
            create_representative_tree(&fx);
            let config = fx.move_everything_config();

            // With the real retire wait, so that several retired sources wait
            // in the staging directory at once.
            let (out, reached) = run_with_faults(fx.command(&config), &format!("kill@{n}"));
            if !reached {
                assert_exit(&out, 0);
                return false;
            }
            let out = common::output(&mut with_short_retire_wait(&fx, &config)).unwrap();

            let context = format!(
                "killed at operation {n}, then ran again; stderr:\n{}",
                common::stderr(&out)
            );
            assert_eq!(out.status.code(), Some(0), "{context}");
            assert_each_exactly_once(&fx, &context);
            for (rel, _, _) in FILES {
                assert!(
                    fx.on_backing_branch(rel).exists(),
                    "{context}: {rel} was not moved"
                );
            }
            assert_no_staging_directories(&fx, &context);
            true
        });
    }

    /// With `errno` injected at any operation, a run loses nothing, and the
    /// next run, even one that only recovers, leaves no leftover.
    fn error_at_every_operation(errno: &str) {
        for_every_operation(|n| {
            let fx = Fixture::new();
            create_representative_tree(&fx);

            let fault = format!("{errno}@{n}");
            let (out, reached) = run_with_faults(
                with_short_retire_wait(&fx, &fx.move_everything_config()),
                &fault,
            );
            if !reached {
                assert_exit(&out, 0);
                return false;
            }
            let context = format!("{fault}; stderr:\n{}", common::stderr(&out));
            assert!(
                matches!(out.status.code(), Some(0 | 2)),
                "{context}\nexit: {:?}",
                out.status
            );
            assert_each_exactly_once(&fx, &context);

            let out =
                common::output(&mut with_short_retire_wait(&fx, &fx.no_work_config())).unwrap();
            let context = format!("{fault}, then recovered; stderr:\n{}", common::stderr(&out));
            assert_eq!(out.status.code(), Some(0), "{context}");
            assert_each_exactly_once(&fx, &context);
            assert_no_staging_directories(&fx, &context);
            true
        });
    }

    #[test]
    fn eio_at_every_operation_loses_nothing_and_leaves_no_leftover() {
        error_at_every_operation("eio");
    }

    #[test]
    fn enospc_at_every_operation_loses_nothing_and_leaves_no_leftover() {
        error_at_every_operation("enospc");
    }

    #[test]
    fn exdev_at_every_operation_loses_nothing_and_leaves_no_leftover() {
        error_at_every_operation("exdev");
    }

    #[test]
    fn eintr_at_every_operation_loses_nothing_and_leaves_no_leftover() {
        error_at_every_operation("eintr");
    }

    /// Once a move has published its copy, a crash before its source is
    /// unlinked (with the source at its relpath or retired into the staging
    /// directory) leaves two copies; the next run keeps only the published one.
    #[test]
    fn crash_between_publish_and_retire_leaves_the_relpath_only_on_the_backing_branch() {
        let rel = "dir/file.bin";
        let states = Mutex::new((false, false));
        for_every_operation(|n| {
            let fx = Fixture::new();
            fx.cache_branch_file(rel, b"content", 1_000);
            let config = fx.move_everything_config();

            let (out, reached) =
                run_with_faults(with_short_retire_wait(&fx, &config), &format!("kill@{n}"));
            if !reached {
                assert_exit(&out, 0);
                return false;
            }
            if !fx.on_backing_branch(rel).exists() {
                return true;
            }
            let at_relpath = fx.on_cache_branch(rel).exists();
            let retired = fx.staged_on_cache_branch(rel).exists();
            if !at_relpath && !retired {
                return true;
            }
            let mut found = states.lock().unwrap();
            found.0 |= at_relpath;
            found.1 |= retired;
            drop(found);

            let out = common::output(&mut with_short_retire_wait(&fx, &config)).unwrap();

            let context = format!(
                "killed at operation {n}, then ran again; stderr:\n{}",
                common::stderr(&out)
            );
            assert_eq!(out.status.code(), Some(0), "{context}");
            assert!(!fx.on_cache_branch(rel).exists(), "{context}");
            assert_eq!(
                fs::read(fx.on_backing_branch(rel)).unwrap(),
                b"content",
                "{context}"
            );
            assert_no_staging_directories(&fx, &context);
            true
        });
        let (at_relpath, retired) = states.into_inner().unwrap();
        assert!(at_relpath, "no crash left the source at its relpath");
        assert!(retired, "no crash left the source retired");
    }

    /// Symlinks of a representative run, oldest first: with their link text.
    const SYMLINKS: [(&str, &str); 2] = [
        ("links/relative", "../media/file.txt"),
        ("links/deep/dangling", "/nowhere"),
    ];

    fn create_symlinks(fx: &Fixture) {
        fx.cache_branch_file("media/file.txt", b"content", 500);
        for (i, (rel, target)) in SYMLINKS.into_iter().enumerate() {
            let path = fx.on_cache_branch(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, &path).unwrap();
            common::set_times(&path, 1_000, 1_000 + i as i64);
        }
    }

    /// Each symlink exists exactly once, on the cache branch or the backing
    /// branch, with its link text and mtime.
    fn assert_each_symlink_exactly_once(fx: &Fixture, context: &str) {
        for (i, (rel, target)) in SYMLINKS.into_iter().enumerate() {
            let copies: Vec<_> = [fx.on_cache_branch(rel), fx.on_backing_branch(rel)]
                .into_iter()
                .filter(|p| fs::symlink_metadata(p).is_ok())
                .collect();
            assert!(
                copies.len() == 1,
                "{context}: {rel} exists {} times",
                copies.len()
            );
            assert_eq!(
                fs::read_link(&copies[0]).unwrap().to_str(),
                Some(target),
                "{context}: {} has other link text",
                copies[0].display()
            );
            let meta = fs::symlink_metadata(&copies[0]).unwrap();
            assert_eq!(
                meta.mtime(),
                1_000 + i as i64,
                "{context}: {} has another mtime",
                copies[0].display()
            );
        }
    }

    #[test]
    fn crash_at_every_operation_of_symlink_moves_is_recovered_by_the_next_run() {
        for_every_operation(|n| {
            let fx = Fixture::new();
            create_symlinks(&fx);
            let config = fx.move_everything_config();

            let (out, reached) =
                run_with_faults(with_short_retire_wait(&fx, &config), &format!("kill@{n}"));
            if !reached {
                assert_exit(&out, 0);
                return false;
            }
            let out = common::output(&mut with_short_retire_wait(&fx, &config)).unwrap();

            let context = format!(
                "killed at operation {n}, then ran again; stderr:\n{}",
                common::stderr(&out)
            );
            assert_eq!(out.status.code(), Some(0), "{context}");
            assert_each_symlink_exactly_once(&fx, &context);
            for (rel, _) in SYMLINKS {
                assert!(
                    fs::symlink_metadata(fx.on_backing_branch(rel)).is_ok(),
                    "{context}: {rel} was not moved"
                );
            }
            assert_no_staging_directories(&fx, &context);
            true
        });
    }

    #[test]
    fn error_at_every_operation_of_symlink_moves_loses_nothing() {
        for errno in ["eio", "enospc"] {
            for_every_operation(|n| {
                let fx = Fixture::new();
                create_symlinks(&fx);

                let fault = format!("{errno}@{n}");
                let (out, reached) = run_with_faults(
                    with_short_retire_wait(&fx, &fx.move_everything_config()),
                    &fault,
                );
                if !reached {
                    assert_exit(&out, 0);
                    return false;
                }
                let context = format!("{fault}; stderr:\n{}", common::stderr(&out));
                assert!(
                    matches!(out.status.code(), Some(0 | 2)),
                    "{context}\nexit: {:?}",
                    out.status
                );
                assert_each_symlink_exactly_once(&fx, &context);

                let out =
                    common::output(&mut with_short_retire_wait(&fx, &fx.no_work_config())).unwrap();
                let context = format!("{fault}, then recovered; stderr:\n{}", common::stderr(&out));
                assert_eq!(out.status.code(), Some(0), "{context}");
                assert_each_symlink_exactly_once(&fx, &context);
                assert_no_staging_directories(&fx, &context);
                true
            });
        }
    }

    /// The names of a hardlink group, sorted, and its content and mtime.
    const GROUP: [&str; 3] = ["a/file.bin", "a/link.bin", "b/deep/link.bin"];
    const GROUP_MTIME: i64 = 1_000;

    fn create_group(fx: &Fixture) {
        fx.cache_branch_hardlinks(&GROUP, b"content", GROUP_MTIME);
    }

    /// Each name of the group exists exactly once, on the cache branch or
    /// the backing branch, with its content and mtime, and the names on each
    /// branch are links to one inode. With `moved`, all are on the backing
    /// branch, as one inode with a link for each.
    fn assert_group_exactly_once(fx: &Fixture, context: &str, moved: bool) {
        let mut inodes = std::collections::BTreeMap::new();
        for name in GROUP {
            let copies: Vec<_> = [fx.on_cache_branch(name), fx.on_backing_branch(name)]
                .into_iter()
                .filter(|p| p.exists())
                .collect();
            assert!(
                copies.len() == 1,
                "{context}: {name} exists {} times",
                copies.len()
            );
            assert_eq!(fs::read(&copies[0]).unwrap(), b"content", "{context}");
            let meta = fs::metadata(&copies[0]).unwrap();
            assert_eq!(meta.mtime(), GROUP_MTIME, "{context}: {name}");
            let on_cache_branch = copies[0] == fx.on_cache_branch(name);
            let inode = (meta.dev(), meta.ino(), meta.nlink());
            assert_eq!(
                *inodes.entry(on_cache_branch).or_insert(inode),
                inode,
                "{context}: {name} is not a link to the other names on its branch"
            );
        }
        if moved {
            let nlinks: Vec<_> = inodes
                .into_iter()
                .map(|(on_cache_branch, (_, _, nlink))| (on_cache_branch, nlink))
                .collect();
            assert_eq!(
                nlinks,
                [(false, GROUP.len() as u64)],
                "{context}: the group was not moved whole"
            );
        }
    }

    #[test]
    fn crash_at_every_operation_of_a_hardlink_group_move_is_recovered_by_the_next_run() {
        for_every_operation(|n| {
            let fx = Fixture::new();
            create_group(&fx);
            let config = fx.move_everything_config();

            let (out, reached) =
                run_with_faults(with_short_retire_wait(&fx, &config), &format!("kill@{n}"));
            if !reached {
                assert_exit(&out, 0);
                return false;
            }
            let out = common::output(&mut with_short_retire_wait(&fx, &config)).unwrap();

            let context = format!(
                "killed at operation {n}, then ran again; stderr:\n{}",
                common::stderr(&out)
            );
            assert_eq!(out.status.code(), Some(0), "{context}");
            assert_group_exactly_once(&fx, &context, true);
            assert_no_staging_directories(&fx, &context);
            true
        });
    }

    #[test]
    fn error_at_every_operation_of_a_hardlink_group_move_loses_nothing() {
        for errno in ["eio", "enospc"] {
            for_every_operation(|n| {
                let fx = Fixture::new();
                create_group(&fx);

                let fault = format!("{errno}@{n}");
                let (out, reached) = run_with_faults(
                    with_short_retire_wait(&fx, &fx.move_everything_config()),
                    &fault,
                );
                if !reached {
                    assert_exit(&out, 0);
                    return false;
                }
                let context = format!("{fault}; stderr:\n{}", common::stderr(&out));
                assert!(
                    matches!(out.status.code(), Some(0 | 2)),
                    "{context}\nexit: {:?}",
                    out.status
                );
                assert_group_exactly_once(&fx, &context, false);

                let config = fx.move_everything_config();
                let out = common::output(&mut with_short_retire_wait(&fx, &config)).unwrap();
                let context = format!("{fault}, then ran again; stderr:\n{}", common::stderr(&out));
                assert_eq!(out.status.code(), Some(0), "{context}");
                assert_group_exactly_once(&fx, &context, true);
                assert_no_staging_directories(&fx, &context);
                true
            });
        }
    }

    /// Recovery itself can crash at any step; the run after that finishes it.
    #[test]
    fn crash_at_every_operation_of_recovery_is_recovered_by_the_next_run() {
        for_every_operation(|n| {
            let fx = Fixture::new();
            // One leftover of each kind: retired sources to delete and to
            // rename back (one into a directory that no longer exists), and a
            // temp file.
            fx.staged_cache_branch_file("same.bin", b"published", 1_000);
            fx.backing_branch_file("same.bin", b"published", 1_000);
            fx.staged_cache_branch_file("dir/changed.bin", b"written while retiring", 1_000);
            fx.backing_branch_file("dir/changed.bin", b"published", 1_000);
            fx.staged_cache_branch_file("gone/unpublished.bin", b"retired", 1_000);
            fx.cache_branch_file("dir/copying.bin", b"source", 1_000);
            fx.staged_backing_branch_file("dir/copying.bin", b"sou", 1_000);
            let config = fx.no_work_config();

            let (out, reached) =
                run_with_faults(with_short_retire_wait(&fx, &config), &format!("kill@{n}"));
            if !reached {
                assert_exit(&out, 0);
                return false;
            }
            let out = common::output(&mut with_short_retire_wait(&fx, &config)).unwrap();

            let context = format!(
                "killed at operation {n}, then ran again; stderr:\n{}",
                common::stderr(&out)
            );
            assert_eq!(out.status.code(), Some(0), "{context}");
            for (path, content) in [
                (fx.on_backing_branch("same.bin"), Some("published")),
                (fx.on_cache_branch("same.bin"), None),
                (
                    fx.on_cache_branch("dir/changed.bin"),
                    Some("written while retiring"),
                ),
                (fx.on_backing_branch("dir/changed.bin"), Some("published")),
                (fx.on_cache_branch("gone/unpublished.bin"), Some("retired")),
                (fx.on_cache_branch("dir/copying.bin"), Some("source")),
                (fx.on_backing_branch("dir/copying.bin"), None),
            ] {
                let found = fs::read(&path).ok();
                assert!(
                    found.as_deref() == content.map(str::as_bytes),
                    "{context}: {}: {:?}",
                    path.display(),
                    found.map(|c| String::from_utf8_lossy(&c).into_owned())
                );
            }
            assert_no_staging_directories(&fx, &context);
            true
        });
    }

    /// If the source's directory cannot be fsynced once the source is
    /// retired, the move fails with the source back in place, not left in the
    /// staging directory where a blocked opener would write into it unseen.
    #[test]
    fn failed_fsync_after_retiring_the_source_puts_it_back() {
        let rel = "dir/file.bin";
        let found = Mutex::new(false);
        for_every_operation(|n| {
            let fx = Fixture::new();
            fx.cache_branch_file(rel, b"content", 1_000);

            let (out, reached) = run_with_faults(
                with_short_retire_wait(&fx, &fx.move_everything_config()),
                &format!("eio@{n}"),
            );
            let err = common::stderr(&out);
            if !err.contains("failed: fsync source directory") {
                return reached;
            }
            *found.lock().unwrap() = true;

            assert_eq!(out.status.code(), Some(2), "stderr:\n{err}");
            assert_eq!(fs::read(fx.on_cache_branch(rel)).unwrap(), b"content");
            assert!(!fx.staged_on_cache_branch(rel).exists(), "stderr:\n{err}");
            assert!(!fx.on_backing_branch(rel).exists(), "stderr:\n{err}");
            true
        });
        assert!(
            found.into_inner().unwrap(),
            "no fsync of the source directory failed"
        );
    }

    /// A move rolls back when its source's directory cannot be fsynced after
    /// the source is retired. A crash or another error at any step of that
    /// rollback loses nothing, and the next run finishes the move.
    #[test]
    fn fault_at_every_operation_of_a_rollback_loses_nothing() {
        let rel = "dir/file.bin";
        let setup = || {
            let fx = Fixture::new();
            fx.cache_branch_file(rel, b"content", 1_000);
            fx
        };
        let rollback_start = Mutex::new(None);
        for_every_operation(|n| {
            let fx = setup();
            let command = with_short_retire_wait(&fx, &fx.move_everything_config());
            let (out, reached) = run_with_faults(command, &format!("eio@{n}"));
            if common::stderr(&out).contains("failed: fsync source directory") {
                *rollback_start.lock().unwrap() = Some(n);
            }
            reached
        });
        let first = rollback_start
            .into_inner()
            .unwrap()
            .expect("no fsync of the source directory failed");

        for fault in ["kill", "eio"] {
            for_every_operation(|n| {
                let fx = setup();
                let config = fx.move_everything_config();
                let faults = format!("eio@{first},{fault}@{}", first + n);

                let (out, reached) = run_with_faults(with_short_retire_wait(&fx, &config), &faults);
                if !reached {
                    assert_exit(&out, 2);
                    return false;
                }
                let out = common::output(&mut with_short_retire_wait(&fx, &config)).unwrap();

                let context = format!(
                    "{faults}, then ran again; stderr:\n{}",
                    common::stderr(&out)
                );
                assert_eq!(out.status.code(), Some(0), "{context}");
                assert!(!fx.on_cache_branch(rel).exists(), "{context}");
                assert_eq!(
                    fs::read(fx.on_backing_branch(rel)).unwrap(),
                    b"content",
                    "{context}"
                );
                assert_no_staging_directories(&fx, &context);
                true
            });
        }
    }

    /// Owner, mode and mtime are never optional: a failed chown, chmod or
    /// utimens fails the move even when xattr and ACL losses are allowed.
    #[test]
    fn failed_chown_chmod_or_utimens_fails_the_move() {
        let rel = "dir/file.bin";
        let found = Mutex::new(Vec::new());
        for_every_operation(|n| {
            let fx = Fixture::new();
            fx.cache_branch_file(rel, b"content", 1_000);
            let config = fx.move_everything_config() + "allow_metadata_loss: [xattrs, acls]\n";

            let (out, reached) =
                run_with_faults(with_short_retire_wait(&fx, &config), &format!("eio@{n}"));
            let err = common::stderr(&out);
            let Some(op) = ["set_owner", "set_mode", "set_times"]
                .into_iter()
                .find(|op| err.contains(&format!("({op})")))
            else {
                return reached;
            };
            found.lock().unwrap().push(op);

            assert_eq!(out.status.code(), Some(2), "{op}; stderr:\n{err}");
            assert!(err.contains("metadata: "), "{op}; stderr:\n{err}");
            if err.contains("cannot set the times of created directory dir: metadata") {
                // The move itself had finished; only the run fails.
                assert_eq!(fs::read(fx.on_backing_branch(rel)).unwrap(), b"content");
                assert!(!fx.on_cache_branch(rel).exists(), "{op}; stderr:\n{err}");
            } else {
                assert!(
                    err.contains("move of dir/file.bin failed"),
                    "{op}; stderr:\n{err}"
                );
                assert_eq!(fs::read(fx.on_cache_branch(rel)).unwrap(), b"content");
                assert!(!fx.on_backing_branch(rel).exists(), "{op}; stderr:\n{err}");
            }
            true
        });
        let mut found = found.into_inner().unwrap();
        found.sort();
        found.dedup();
        assert_eq!(found, ["set_mode", "set_owner", "set_times"]);
    }
}
