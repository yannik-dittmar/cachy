mod common;

use std::path::Path;
use std::process::Command;

use common::{Fixture, FsKind, output, skip, zfs_dataset};

#[test]
fn zfs_branches_resolve_to_their_dataset() {
    let fx = Fixture::new();
    let branches = [
        (fx.cache_branch.path(), fx.cache_branch_fs),
        (fx.backing_branch.path(), fx.backing_branch_fs),
    ];
    if !branches.iter().any(|(_, fs)| *fs == FsKind::Zfs) {
        skip("no branch is on ZFS");
        return;
    }

    for (branch, _) in branches.iter().filter(|(_, fs)| *fs == FsKind::Zfs) {
        let dataset = zfs_dataset(branch);
        let out = output(Command::new("zfs").args(["list", "-H", "-o", "mountpoint", &dataset]))
            .expect("spawn zfs");
        assert!(out.status.success(), "zfs list {dataset}");
        let mountpoint = String::from_utf8(out.stdout).unwrap();
        assert!(
            branch.starts_with(Path::new(mountpoint.trim())),
            "{} is not below {dataset}'s mountpoint {mountpoint}",
            branch.display()
        );
    }
}
