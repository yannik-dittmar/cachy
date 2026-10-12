//! The move transaction: relocates one regular file from the cache branch to a
//! backing branch, so that afterwards it exists on exactly one of them.
//!
//! Each staging directory mirrors relpaths (`<root>/.cachy-staging/<relpath>`),
//! so anything left in it after a crash names the move it belongs to.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path};

use anyhow::{Context, Result};

use crate::fsops::FsOps;

/// Name of the staging directory at the root of each filesystem cachy writes to.
pub const STAGING: &str = ".cachy-staging";

const COPY_BUF: usize = 1 << 20;
const DIR_MODE: u32 = 0o755;
const STAGING_MODE: u32 = 0o700;
const TEMP_MODE: u32 = 0o600;

/// A branch root, held open so every operation is relative to it.
pub struct Branch {
    pub path: std::path::PathBuf,
    pub fd: OwnedFd,
}

impl Branch {
    pub fn open(path: &Path) -> io::Result<Branch> {
        let dir = std::fs::File::open(path)?;
        if !dir.metadata()?.is_dir() {
            return Err(io::Error::from(io::ErrorKind::NotADirectory));
        }
        Ok(Branch {
            path: path.to_owned(),
            fd: dir.into(),
        })
    }
}

/// Moves the regular file `rel` from `cache` to `backing`. On failure before
/// publish, the source is untouched and the temp file is removed.
pub fn move_file(fs: &dyn FsOps, cache: &Branch, backing: &Branch, rel: &Path) -> Result<()> {
    let (parent, name) = split(rel);

    let src = fs.open_read(cache.fd.as_fd(), rel).context("open source")?;
    let backing_staging = make_dirs(fs, &backing.fd, Path::new(STAGING), STAGING_MODE)
        .context("create backing staging directory")?;
    let temp_dir = make_dirs(fs, &backing_staging, parent, STAGING_MODE)
        .context("create backing staging directory")?;

    let temp = fs
        .create_new(temp_dir.as_fd(), name, TEMP_MODE)
        .context("create temp file")?;
    let staged = copy(fs, &src, &temp)
        .context("copy")
        .and_then(|()| fs.fsync(temp.as_fd()).context("fsync temp file"));
    drop(temp);
    let published = staged.and_then(|()| publish(fs, backing, parent, name, &temp_dir));
    if let Err(e) = published {
        let _ = fs.unlink(temp_dir.as_fd(), name);
        remove_empty_dirs(fs, &backing_staging, parent);
        return Err(e);
    }
    remove_empty_dirs(fs, &backing_staging, parent);

    retire(fs, cache, parent, name)
}

/// Makes the staged temp file visible as `<backing>/<parent>/<name>`, never
/// replacing an existing file.
fn publish(
    fs: &dyn FsOps,
    backing: &Branch,
    parent: &Path,
    name: &Path,
    temp_dir: &OwnedFd,
) -> Result<()> {
    let dest_dir =
        make_dirs(fs, &backing.fd, parent, DIR_MODE).context("create destination directory")?;
    fs.rename_noreplace(temp_dir.as_fd(), name, dest_dir.as_fd(), name)
        .context("publish")?;
    fs.fsync(dest_dir.as_fd())
        .context("fsync destination directory")?;
    fs.fsync(temp_dir.as_fd())
        .context("fsync backing staging directory")?;
    Ok(())
}

/// Removes the source from the cache branch by first renaming it into the
/// cache branch's staging directory.
fn retire(fs: &dyn FsOps, cache: &Branch, parent: &Path, name: &Path) -> Result<()> {
    let cache_staging = make_dirs(fs, &cache.fd, Path::new(STAGING), STAGING_MODE)
        .context("create cache staging directory")?;
    let retire_dir = make_dirs(fs, &cache_staging, parent, STAGING_MODE)
        .context("create cache staging directory")?;
    let src_dir = open_dirs(fs, &cache.fd, parent).context("open source directory")?;

    fs.rename_noreplace(src_dir.as_fd(), name, retire_dir.as_fd(), name)
        .context("retire")?;
    fs.fsync(src_dir.as_fd())
        .context("fsync source directory")?;
    fs.unlink(retire_dir.as_fd(), name).context("retire")?;
    fs.fsync(retire_dir.as_fd())
        .context("fsync cache staging directory")?;
    remove_empty_dirs(fs, &cache_staging, parent);
    Ok(())
}

fn copy(fs: &dyn FsOps, src: &OwnedFd, dst: &OwnedFd) -> io::Result<()> {
    let mut buf = vec![0; COPY_BUF];
    loop {
        let n = fs.read(src.as_fd(), &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        let mut chunk = &buf[..n];
        while !chunk.is_empty() {
            let written = fs.write(dst.as_fd(), chunk)?;
            chunk = &chunk[written..];
        }
    }
}

/// Opens `path` below `root`, creating missing directories. Each created
/// directory's parent is fsynced so the new entry is durable.
fn make_dirs(fs: &dyn FsOps, root: &OwnedFd, path: &Path, mode: u32) -> io::Result<OwnedFd> {
    let mut dir = fs.open_dir(root.as_fd(), Path::new("."))?;
    for component in normal_components(path) {
        let created = match fs.mkdir(dir.as_fd(), component, mode) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(e),
        };
        let child = fs.open_dir(dir.as_fd(), component)?;
        if created {
            fs.fsync(dir.as_fd())?;
        }
        dir = child;
    }
    Ok(dir)
}

fn open_dirs(fs: &dyn FsOps, root: &OwnedFd, path: &Path) -> io::Result<OwnedFd> {
    let mut dir = fs.open_dir(root.as_fd(), Path::new("."))?;
    for component in normal_components(path) {
        dir = fs.open_dir(dir.as_fd(), component)?;
    }
    Ok(dir)
}

/// Removes `path` below `root` and its ancestors (but not `root`) while they
/// are empty.
fn remove_empty_dirs(fs: &dyn FsOps, root: &OwnedFd, path: &Path) {
    for dir in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        if fs.rmdir(root.as_fd(), dir).is_err() {
            return;
        }
    }
}

fn normal_components(path: &Path) -> impl Iterator<Item = &Path> {
    path.components().filter_map(|c| match c {
        Component::Normal(name) => Some(Path::new(name)),
        _ => None,
    })
}

fn split(rel: &Path) -> (&Path, &Path) {
    let parent = rel.parent().unwrap_or(Path::new(""));
    let name = Path::new(rel.file_name().expect("a move unit's relpath names a file"));
    (parent, name)
}
