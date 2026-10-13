//! The `/proc` scan: finds the processes that have a file open, and whether
//! for writing, from their descriptors in `/proc/<pid>/fd` and the flags that
//! `/proc/<pid>/fdinfo` gives for each. It is how the `proc` guard tells that
//! a move unit is in use where the filesystem has no leases.
//!
//! A descriptor is matched to the file by its device and inode, so the scan
//! finds it whatever name or mount the process opened the file through. It
//! changes nothing, so it runs outside the port.
//!
//! The scan does not see:
//!
//! - processes whose descriptors cachy cannot read: those of another user,
//!   unless cachy runs as root or with `CAP_SYS_PTRACE`, and those with a
//!   descriptor that cannot be statted, such as one on a dead FUSE mount. It
//!   counts them instead, so that one such process does not stop every move.
//! - processes outside cachy's PID namespace, which `/proc` does not list.
//! - a thread that unshared its descriptor table (`unshare(CLONE_FILES)`),
//!   whose descriptors only `/proc/<pid>/task/<tid>/fd` lists.
//! - a shared memory mapping whose descriptor has been closed.
//!
//! Descriptors opened with `O_PATH` name a file without opening it for
//! reading or writing, so the scan ignores them.

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{AtFlags, CWD, StatxFlags};

const PROC: &str = "/proc";

/// The opens of one file that a scan found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Opens {
    /// Some process has it open for writing.
    pub writing: bool,
    /// Some process has it open for reading only.
    pub reading: bool,
    /// The number of processes whose descriptors cachy could not read.
    pub hidden: usize,
}

/// Scans the descriptors of every process for the file with device and inode
/// `file`; those of cachy's own process too if `own`.
pub fn scan(file: (u64, u64), own: bool) -> io::Result<Opens> {
    let own_pid = std::process::id();
    let mut opens = Opens::default();
    for entry in fs::read_dir(PROC)? {
        let entry = entry?;
        let Some(pid) = std::str::from_utf8(entry.file_name().as_bytes())
            .ok()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own_pid && !own {
            continue;
        }
        match scan_process(&Path::new(PROC).join(pid.to_string()), file, &mut opens) {
            Ok(()) => {}
            Err(e) if gone(&e) => {}
            Err(_) => opens.hidden += 1,
        }
    }
    Ok(opens)
}

/// Adds the opens of `file` by the process at `/proc/<pid>` to `opens`. A
/// descriptor of the file whose fdinfo cannot be understood counts as open
/// for writing.
fn scan_process(process: &Path, file: (u64, u64), opens: &mut Opens) -> io::Result<()> {
    for entry in fs::read_dir(process.join("fd"))? {
        let fd = entry?.file_name();
        let Some(id) = identity(&process.join("fd").join(&fd))? else {
            continue;
        };
        if id != file {
            continue;
        }
        let info = match fs::read(process.join("fdinfo").join(&fd)) {
            Ok(info) => info,
            Err(e) if gone(&e) => continue,
            Err(e) => return Err(e),
        };
        match access(&info, file.1) {
            Some(Access::Write) | None => opens.writing = true,
            Some(Access::Read) => opens.reading = true,
            Some(Access::None) => {}
        }
    }
    Ok(())
}

/// The device and inode of the file that the descriptor link `link` names,
/// or `None` if the descriptor has been closed. Takes the attributes the
/// kernel has cached (`AT_STATX_DONT_SYNC`) rather than ask a network
/// filesystem's server for fresh ones.
fn identity(link: &Path) -> io::Result<Option<(u64, u64)>> {
    match rustix::fs::statx(CWD, link, AtFlags::STATX_DONT_SYNC, StatxFlags::INO) {
        Ok(stat) => Ok(Some((
            rustix::fs::makedev(stat.stx_dev_major, stat.stx_dev_minor),
            stat.stx_ino,
        ))),
        Err(e) => {
            let e = io::Error::from(e);
            if gone(&e) { Ok(None) } else { Err(e) }
        }
    }
}

/// What a descriptor allows its process to do with the file.
#[derive(Debug, PartialEq, Eq)]
enum Access {
    Write,
    Read,
    /// An `O_PATH` descriptor, which only names the file.
    None,
}

/// The access that the descriptor described by the fdinfo `info` gives to
/// the file with inode `ino`, or `None` if `info` has no flags. A descriptor
/// whose fdinfo names another inode was closed, and its number reused,
/// after it was matched; it gives no access to the file.
fn access(info: &[u8], ino: u64) -> Option<Access> {
    let field = |name: &str| {
        info.split(|&b| b == b'\n').find_map(|line| {
            let value = line.strip_prefix(name.as_bytes())?.strip_prefix(b":")?;
            std::str::from_utf8(value).ok().map(str::trim)
        })
    };
    if field("ino")
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|i| i != ino)
    {
        return Some(Access::None);
    }
    let flags = i32::from_str_radix(field("flags")?, 8).ok()?;
    Some(if flags & libc::O_PATH != 0 {
        Access::None
    } else if flags & libc::O_ACCMODE == libc::O_RDONLY {
        Access::Read
    } else {
        Access::Write
    })
}

/// Whether `e` means that the process or descriptor is gone.
fn gone(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    #[test]
    fn access_follows_the_flags() {
        let info = |flags: &str| format!("pos:\t0\nflags:\t{flags}\nmnt_id:\t29\nino:\t42\n");
        assert_eq!(access(info("0100000").as_bytes(), 42), Some(Access::Read));
        assert_eq!(access(info("0100001").as_bytes(), 42), Some(Access::Write));
        assert_eq!(access(info("0102002").as_bytes(), 42), Some(Access::Write));
        assert_eq!(access(info("012100000").as_bytes(), 42), Some(Access::None));
    }

    #[test]
    fn reused_descriptor_gives_no_access() {
        let info = b"pos:\t0\nflags:\t0100002\nmnt_id:\t29\nino:\t43\n";
        assert_eq!(access(info, 42), Some(Access::None));
    }

    #[test]
    fn fdinfo_without_an_inode_is_read_by_its_flags() {
        assert_eq!(access(b"pos:\t0\nflags:\t02\n", 42), Some(Access::Write));
    }

    #[test]
    fn fdinfo_without_flags_is_unrecognised() {
        assert_eq!(access(b"pos:\t0\nino:\t42\n", 42), None);
        assert_eq!(access(b"pos:\t0\nflags:\tnine\n", 42), None);
    }

    #[test]
    fn scan_finds_own_opens_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let writer = fs::File::create(&path).unwrap();
        let reader = fs::File::open(&path).unwrap();
        let stat = writer.metadata().unwrap();
        let file = (stat.dev(), stat.ino());

        let own = scan(file, true).unwrap();
        assert!(own.writing && own.reading, "{own:?}");
        let others = scan(file, false).unwrap();
        assert!(!others.writing && !others.reading, "{others:?}");
        drop((writer, reader));
    }
}
