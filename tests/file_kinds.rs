//! Which kinds of files a run moves: regular files of any size, but never
//! FIFOs, sockets or device nodes.

mod common;

use std::fs;
use std::os::unix::fs::FileTypeExt;

use common::{Fixture, assert_exit, root_available, skip, sudo};
use rustix::fs::{FileType, Mode, mknodat};

#[test]
fn zero_byte_regular_files_are_moved() {
    let fx = Fixture::new();
    fx.cache_branch_file("photos/.nomedia", b"", 1_000);

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(err.contains("1 moved"), "stderr:\n{err}");
    assert!(!fx.on_cache_branch("photos/.nomedia").exists());
    assert_eq!(
        fs::read(fx.on_backing_branch("photos/.nomedia")).unwrap(),
        b""
    );
}

#[test]
fn fifos_and_sockets_stay_in_place_and_are_skipped_as_special_files() {
    let fx = Fixture::new();
    fs::create_dir(fx.on_cache_branch("run")).unwrap();
    mknodat(
        rustix::fs::CWD,
        fx.on_cache_branch("run/pipe"),
        FileType::Fifo,
        Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(fx.on_cache_branch("run/socket")).unwrap();
    fx.cache_branch_file("run/plain.txt", b"plain", 1_000);

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(err.contains("run/pipe: special file"), "stderr:\n{err}");
    assert!(err.contains("run/socket: special file"), "stderr:\n{err}");
    assert!(err.contains("1 moved, 2 skipped"), "stderr:\n{err}");
    let pipe = fs::symlink_metadata(fx.on_cache_branch("run/pipe")).unwrap();
    assert!(pipe.file_type().is_fifo());
    let socket = fs::symlink_metadata(fx.on_cache_branch("run/socket")).unwrap();
    assert!(socket.file_type().is_socket());
    assert_eq!(
        Fixture::tree(fx.backing_branch.path()),
        ["run", "run/plain.txt"]
    );
}

#[test]
fn device_nodes_stay_in_place_and_are_skipped_as_special_files() {
    if !root_available() {
        return;
    }
    let fx = Fixture::new();
    // /dev/null's device number; creating a device node needs root.
    let null = rustix::fs::makedev(1, 3);
    let created = if rustix::process::geteuid().is_root() {
        mknodat(
            rustix::fs::CWD,
            fx.on_cache_branch("null"),
            FileType::CharacterDevice,
            Mode::from_raw_mode(0o666),
            null,
        )
        .map_err(|e| e.to_string())
    } else {
        sudo(&[
            "mknod".as_ref(),
            fx.on_cache_branch("null").as_os_str(),
            "c".as_ref(),
            "1".as_ref(),
            "3".as_ref(),
        ])
    };
    if let Err(e) = created {
        // Some filesystems (or their mount options) refuse device nodes.
        skip(&format!("cannot create a device node: {e}"));
        return;
    }

    let err = assert_exit(&fx.run(&fx.move_everything_config()), 0);

    assert!(err.contains("null: special file"), "stderr:\n{err}");
    assert!(err.contains("0 moved, 1 skipped"), "stderr:\n{err}");
    let node = fs::symlink_metadata(fx.on_cache_branch("null")).unwrap();
    assert!(node.file_type().is_char_device());
    assert!(Fixture::tree(fx.backing_branch.path()).is_empty());
}
