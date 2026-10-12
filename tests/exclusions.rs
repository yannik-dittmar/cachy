//! Exclusions: gitignore patterns anchored at the cache branch root, which
//! keep matching files and directories on the cache branch. Also: the walk
//! never leaves the cache branch's filesystem.

mod common;

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use common::{Fixture, assert_exit, root_available, sudo};

fn config_excluding(fx: &Fixture, patterns: &[&str]) -> String {
    let mut config = fx.move_everything_config() + "exclude:\n";
    for pattern in patterns {
        config += &format!("  - '{pattern}'\n");
    }
    config
}

fn assert_kept(fx: &Fixture, rel: &str) {
    assert!(
        fx.on_cache_branch(rel).exists(),
        "{rel} left the cache branch"
    );
    assert!(
        !fx.on_backing_branch(rel).exists(),
        "{rel} appeared on the backing branch"
    );
}

fn assert_moved(fx: &Fixture, rel: &str) {
    assert!(
        !fx.on_cache_branch(rel).exists(),
        "{rel} still on the cache branch"
    );
    assert!(fx.on_backing_branch(rel).exists(), "{rel} not moved");
}

#[test]
fn directory_pattern_excludes_that_directory_only_at_the_cache_branch_root() {
    let fx = Fixture::new();
    fx.cache_branch_file("appdata/plex/db.sqlite", b"db", 1_000);
    fx.cache_branch_file("media/appdata/notes.txt", b"notes", 1_000);
    fx.cache_branch_file("appdata.txt", b"not a directory", 1_000);

    assert_exit(&fx.run(&config_excluding(&fx, &["appdata/"])), 0);

    assert_kept(&fx, "appdata/plex/db.sqlite");
    assert_moved(&fx, "media/appdata/notes.txt");
    assert_moved(&fx, "appdata.txt");
}

#[test]
fn file_pattern_excludes_that_file() {
    let fx = Fixture::new();
    fx.cache_branch_file("snapraid.content", b"content", 1_000);
    fx.cache_branch_file("movie.mkv", b"movie", 1_000);

    let err = assert_exit(&fx.run(&config_excluding(&fx, &["snapraid.content"])), 0);

    assert_kept(&fx, "snapraid.content");
    assert_moved(&fx, "movie.mkv");
    assert!(err.contains("1 moved, 0 skipped"), "stderr:\n{err}");
}

#[test]
fn nested_patterns_behave_like_gitignore() {
    let fx = Fixture::new();
    for rel in [
        "media/tmp/a.part",
        "media/tv/tmp/b.mkv",
        "media/tv/c.part",
        "media/tv/c.mkv",
        "downloads/x.iso",
        "downloads/keep/y.iso",
        "a/.cache/z",
        "a/b/.cache/z",
    ] {
        fx.cache_branch_file(rel, rel.as_bytes(), 1_000);
    }

    assert_exit(
        &fx.run(&config_excluding(
            &fx,
            &[
                "media/tmp",
                "media/**/*.part",
                "downloads/*",
                "!downloads/keep/",
                "**/.cache/",
            ],
        )),
        0,
    );

    assert_kept(&fx, "media/tmp/a.part");
    assert_moved(&fx, "media/tv/tmp/b.mkv");
    assert_kept(&fx, "media/tv/c.part");
    assert_moved(&fx, "media/tv/c.mkv");
    assert_kept(&fx, "downloads/x.iso");
    assert_moved(&fx, "downloads/keep/y.iso");
    assert_kept(&fx, "a/.cache/z");
    assert_kept(&fx, "a/b/.cache/z");
}

/// Creates `depth` nested directories with 200-byte names under `dir`, so
/// that the deepest one's path is longer than `PATH_MAX`. Reading the deepest one by path fails, even
/// as root, so a walk that enters the tree warns about it.
fn too_deep_tree(dir: &Path, depth: usize) {
    let name = "d".repeat(200);
    let mut fd = rustix::fs::open(
        dir,
        rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::RDONLY,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    for _ in 0..depth {
        rustix::fs::mkdirat(&fd, name.as_str(), rustix::fs::Mode::from_raw_mode(0o755)).unwrap();
        fd = rustix::fs::openat(
            &fd,
            name.as_str(),
            rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::RDONLY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
    }
}

#[test]
fn excluded_directories_are_not_traversed() {
    let populate = |fx: &Fixture| {
        fs::create_dir(fx.on_cache_branch("excluded")).unwrap();
        too_deep_tree(&fx.on_cache_branch("excluded"), 24);
        fx.cache_branch_file("movie.mkv", b"movie", 1_000);
    };
    // Without the exclusion, the walk enters the tree and warns.
    let control = Fixture::new();
    populate(&control);
    let err = assert_exit(&control.run(&control.move_everything_config()), 0);
    assert!(err.contains("warning: skipping"), "stderr:\n{err}");

    let fx = Fixture::new();
    populate(&fx);

    let err = assert_exit(&fx.run(&config_excluding(&fx, &["excluded/"])), 0);

    assert!(!err.contains("warning: skipping"), "stderr:\n{err}");
    assert_moved(&fx, "movie.mkv");
}

#[test]
fn exclusions_never_match_outside_the_cache_branch() {
    let fx = Fixture::new();
    fx.cache_branch_file("movie.mkv", b"movie", 1_000);
    // The cache branch is a temp dir named cachy-cache-XXXXXX.
    let config = config_excluding(
        &fx,
        &["**/cachy-cache-*", "**/cachy-cache-*/**", "cachy-cache-*/"],
    );

    assert_exit(&fx.run(&config), 0);

    assert_moved(&fx, "movie.mkv");
}

#[test]
fn invalid_patterns_are_refused() {
    for pattern in ["", "   ", "media/[z-a]"] {
        let fx = Fixture::new();

        let err = assert_exit(&fx.run(&config_excluding(&fx, &[pattern])), 1);

        assert!(
            err.contains("exclude"),
            "pattern `{pattern}`, stderr:\n{err}"
        );
    }
}

#[test]
fn directory_on_another_filesystem_inside_the_cache_branch_is_not_walked() {
    if !root_available() {
        return;
    }
    let fx = Fixture::new();
    let mountpoint = fx.on_cache_branch("nested");
    fs::create_dir(&mountpoint).unwrap();
    sudo(&[
        OsStr::new("mount"),
        OsStr::new("-t"),
        OsStr::new("tmpfs"),
        OsStr::new("-o"),
        OsStr::new(&format!(
            "size=16M,uid={},gid={}",
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw()
        )),
        OsStr::new("cachy-test-nested"),
        mountpoint.as_os_str(),
    ])
    .unwrap();
    let unmount = Unmount(&mountpoint);
    fx.cache_branch_file("nested/on-other-fs.bin", b"other", 1_000);
    fx.cache_branch_file("movie.mkv", b"movie", 1_000);

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(err.contains("1 moved, 0 skipped"), "stderr:\n{err}");
    assert_kept(&fx, "nested/on-other-fs.bin");
    assert_moved(&fx, "movie.mkv");
    drop(unmount);
}

struct Unmount<'a>(&'a Path);

impl Drop for Unmount<'_> {
    fn drop(&mut self) {
        sudo(&[OsStr::new("umount"), self.0.as_os_str()]).unwrap();
    }
}
