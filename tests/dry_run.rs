//! `--dry-run`: writes nothing, and prints the byte budget, the move units a
//! run would move, in order, and every skip with its reason.

mod common;

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use common::{Fixture, assert_exit, output};
use rustix::fs::{FileType, Mode, OFlags, mknodat};

/// Every entry under `root` with its content (empty for directories), atime
/// and mtime, sorted by relpath. Reading the content leaves atimes as they
/// are.
fn contents(root: &Path) -> Vec<(String, Vec<u8>, (i64, i64))> {
    Fixture::tree(root)
        .into_iter()
        .map(|rel| {
            let path = root.join(&rel);
            let content = if fs::symlink_metadata(&path).unwrap().is_file() {
                let file = rustix::fs::open(&path, OFlags::RDONLY | OFlags::NOATIME, Mode::empty())
                    .unwrap();
                let mut content = Vec::new();
                fs::File::from(file).read_to_end(&mut content).unwrap();
                content
            } else {
                Vec::new()
            };
            let meta = fs::symlink_metadata(&path).unwrap();
            (rel, content, (meta.atime(), meta.mtime()))
        })
        .collect()
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn dry_run_writes_nothing_and_lists_planned_moves_in_order_and_skips_with_reasons() {
    let fx = Fixture::new();
    fx.cache_branch_file("new.bin", b"newest", 3_000);
    fx.cache_branch_file("media/old.bin", b"oldest", 1_000);
    fx.cache_branch_file("downloads/partial.bin", b"partial", 2_000);
    fx.backing_branch_file("existing.txt", b"on the backing branch", 500);
    mknodat(
        rustix::fs::CWD,
        fx.on_cache_branch("pipe"),
        FileType::Fifo,
        Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();
    let _writer = OpenOptions::new()
        .write(true)
        .open(fx.on_cache_branch("downloads/partial.bin"))
        .unwrap();
    let cache_before = contents(fx.cache_branch.path());
    let backing_before = contents(fx.backing_branch.path());

    let out = output(fx.command(&fx.move_everything_config()).arg("--dry-run")).unwrap();

    assert_exit(&out, 0);
    let planned = stdout(&out);
    assert_eq!(contents(fx.cache_branch.path()), cache_before);
    assert_eq!(contents(fx.backing_branch.path()), backing_before);
    let lines: Vec<&str> = planned.lines().collect();
    assert!(
        lines[0].starts_with("byte budget: ") && lines[0].ends_with(" bytes"),
        "stdout:\n{planned}"
    );
    // The fifo's mtime is fresh, so it comes last.
    assert_eq!(
        lines[1..],
        [
            "would move media/old.bin",
            "would skip downloads/partial.bin: in use",
            "would move new.bin",
            "would skip pipe: special file",
        ],
        "stdout:\n{planned}"
    );
}
