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

/// Moves the regular file `rel` from the cache branch to the backing branch.
/// On failure before publish, the source is untouched and the temp file is
/// removed.
///
/// A stale copy at `rel` on the backing branch is not replaced yet: the
/// publish fails and the move with it.
pub fn move_file(
    fs: &dyn FsOps,
    cache_branch: &Branch,
    backing_branch: &Branch,
    rel: &Path,
) -> Result<()> {
    let (parent, name) = split(rel);

    let src = fs
        .open_read(cache_branch.fd.as_fd(), rel)
        .context("open source")?;
    let (backing_staging, staged_parent) = staging_dirs(fs, backing_branch, parent)
        .context("create staging directory on the backing branch")?;

    let temp = fs
        .create_new(staged_parent.as_fd(), name, TEMP_MODE)
        .context("create temp file")?;
    let staged = copy(fs, &src, &temp)
        .context("copy")
        .and_then(|()| fs.fsync(temp.as_fd()).context("fsync temp file"));
    drop(temp);
    let published = staged.and_then(|()| publish(fs, backing_branch, parent, name, &staged_parent));
    if published.is_err() {
        let _ = fs.unlink(staged_parent.as_fd(), name);
    }
    remove_empty_dirs(fs, &backing_staging, parent);
    published?;

    retire(fs, cache_branch, parent, name)
}

/// Makes the staged temp file visible as `<backing branch>/<parent>/<name>`,
/// never replacing an existing file.
fn publish(
    fs: &dyn FsOps,
    backing_branch: &Branch,
    parent: &Path,
    name: &Path,
    staged_parent: &OwnedFd,
) -> Result<()> {
    let dest_parent = make_dirs(fs, &backing_branch.fd, parent, DIR_MODE)
        .context("create destination directory")?;
    fs.rename_noreplace(staged_parent.as_fd(), name, dest_parent.as_fd(), name)
        .context("publish")?;
    fs.fsync(dest_parent.as_fd())
        .context("fsync destination directory")?;
    fs.fsync(staged_parent.as_fd())
        .context("fsync staging directory on the backing branch")?;
    Ok(())
}

/// Removes the source from the cache branch by first renaming it into the
/// cache branch's staging directory.
fn retire(fs: &dyn FsOps, cache_branch: &Branch, parent: &Path, name: &Path) -> Result<()> {
    let (cache_staging, retired_parent) = staging_dirs(fs, cache_branch, parent)
        .context("create staging directory on the cache branch")?;
    let src_parent = open_dirs(fs, &cache_branch.fd, parent).context("open source directory")?;

    fs.rename_noreplace(src_parent.as_fd(), name, retired_parent.as_fd(), name)
        .context("retire: rename source into staging directory")?;
    fs.fsync(src_parent.as_fd())
        .context("fsync source directory")?;
    fs.unlink(retired_parent.as_fd(), name)
        .context("retire: unlink source")?;
    fs.fsync(retired_parent.as_fd())
        .context("fsync staging directory on the cache branch")?;
    remove_empty_dirs(fs, &cache_staging, parent);
    Ok(())
}

/// Opens `branch`'s staging directory and, below it, the mirror of `parent`,
/// creating both as needed.
fn staging_dirs(fs: &dyn FsOps, branch: &Branch, parent: &Path) -> io::Result<(OwnedFd, OwnedFd)> {
    let staging = make_dirs(fs, &branch.fd, Path::new(STAGING), STAGING_MODE)?;
    let mirrored_parent = make_dirs(fs, &staging, parent, STAGING_MODE)?;
    Ok((staging, mirrored_parent))
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
