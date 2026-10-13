//! Layout discovery: which backing branches cachy may write to, with their
//! mergerfs modes and `minfreespace`.
//!
//! With a cache pool configured, the branches come from its mergerfs runtime
//! interface: the `user.mergerfs.*` xattrs of `<cache pool>/.mergerfs`. Its
//! first branch must be the cache branch, or applications would see a copy
//! on an earlier branch in place of the cache branch's. If the runtime
//! interface is unavailable (`xattr=nosys`, or no mergerfs mount), an
//! explicit `backing_branches` list in the config is used instead.
//!
//! The runtime interface is read outside the filesystem-operations port, like
//! every check that cachy makes before it changes anything.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{Config, parse_byte_size};
use crate::log;
use crate::transaction::Branch;

/// The control file through which mergerfs serves its runtime interface.
const CONTROL_FILE: &str = ".mergerfs";

const EXPECTED_MINFREESPACE: &str = "expected a whole number with an optional K, M, G or T suffix";

/// A branch's mergerfs mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Read-write: takes new files.
    ReadWrite,
    /// Read-only: never written to.
    ReadOnly,
    /// No-create: existing files may change, but it takes no new files.
    NoCreate,
}

/// A branch as mergerfs writes it: `<path>[=<mode>[,<minfreespace>]]`.
#[derive(Clone, Debug)]
pub struct BranchSpec {
    pub path: PathBuf,
    pub mode: Mode,
    /// The branch's own `minfreespace`, overriding the cache pool's.
    pub minfreespace: Option<u64>,
}

impl BranchSpec {
    /// Parses one branch in mergerfs's syntax. A missing mode is `RW`.
    pub fn parse(spec: &[u8]) -> Result<BranchSpec, String> {
        let invalid = |why: String| format!("invalid branch `{}`: {why}", spec.escape_ascii());
        let (path, options) = match spec.iter().rposition(|&b| b == b'=') {
            Some(at) => (&spec[..at], Some(&spec[at + 1..])),
            None => (spec, None),
        };
        if path.is_empty() {
            return Err(invalid("empty path".to_owned()));
        }
        let (mode, minfreespace) = match options {
            None => (Mode::ReadWrite, None),
            Some(options) => {
                let (mode, minfreespace) = match options.iter().position(|&b| b == b',') {
                    Some(at) => (&options[..at], Some(&options[at + 1..])),
                    None => (options, None),
                };
                let mode = match mode {
                    b"RW" => Mode::ReadWrite,
                    b"RO" => Mode::ReadOnly,
                    b"NC" => Mode::NoCreate,
                    _ => {
                        return Err(invalid(format!(
                            "unknown mode `{}`: expected RW, RO or NC",
                            mode.escape_ascii()
                        )));
                    }
                };
                let minfreespace = minfreespace
                    .map(|bytes| parse_minfreespace(bytes).map_err(invalid))
                    .transpose()?;
                (mode, minfreespace)
            }
        };
        Ok(BranchSpec {
            path: PathBuf::from(OsStr::from_bytes(path)),
            mode,
            minfreespace,
        })
    }
}

fn parse_minfreespace(bytes: &[u8]) -> Result<u64, String> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(parse_byte_size)
        .ok_or_else(|| {
            format!(
                "invalid minfreespace `{}`: {EXPECTED_MINFREESPACE}",
                bytes.escape_ascii()
            )
        })
}

/// A backing branch, held open, with what placement must honour.
pub struct BackingBranch {
    pub branch: Branch,
    pub mode: Mode,
    /// Free space that a move must leave on the branch.
    pub minfreespace: u64,
}

impl BackingBranch {
    /// Whether mergerfs, or cachy, may create files on it: it is neither `RO`
    /// nor `NC`.
    pub fn takes_new_files(&self) -> bool {
        self.mode == Mode::ReadWrite
    }
}

/// Finds the backing branches of the cache branch, refusing a cache pool
/// whose first branch is not the cache branch. Warns, on stderr, when the
/// cache pool's `moveonenospc` can move files back onto the cache branch, and
/// when `threshold` (in bytes) is below the cache branch's `minfreespace`.
pub fn discover(
    config: &Config,
    cache_branch: &Branch,
    threshold: u64,
) -> Result<Vec<BackingBranch>> {
    let discovered = match &config.cache_pool {
        Some(cache_pool) => read_cache_pool(config, cache_pool, cache_branch, threshold)?,
        None => None,
    };
    // Explicit branches take no minfreespace unless they give one.
    let specs = discovered.unwrap_or_else(|| {
        config
            .backing_branches
            .clone()
            .expect("a config without a readable cache pool lists backing branches")
    });
    let backing_branches = specs
        .into_iter()
        .map(|spec| {
            let branch = Branch::open(&spec.path)
                .with_context(|| format!("cannot open backing branch {}", spec.path.display()))?;
            if branch.same_dir(cache_branch)? {
                bail!(
                    "backing branch {} is the cache branch {}",
                    spec.path.display(),
                    cache_branch.path.display()
                );
            }
            Ok(BackingBranch {
                branch,
                mode: spec.mode,
                minfreespace: spec.minfreespace.unwrap_or(0),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if !backing_branches.iter().any(BackingBranch::takes_new_files) {
        if backing_branches.is_empty() {
            bail!("the cache pool has no backing branch besides the cache branch");
        }
        bail!("no backing branch takes new files: every one is RO or NC");
    }
    Ok(backing_branches)
}

/// The backing branches that the cache pool's runtime interface lists, each
/// with its `minfreespace` resolved, or `None` (with a warning) if it is
/// unavailable and the config lists backing branches instead.
fn read_cache_pool(
    config: &Config,
    cache_pool: &Path,
    cache_branch: &Branch,
    threshold: u64,
) -> Result<Option<Vec<BranchSpec>>> {
    let control = cache_pool.join(CONTROL_FILE);
    let branches = match read_value(&control, "branches") {
        Ok(branches) => branches,
        Err(e) if config.backing_branches.is_some() => {
            log::warning!(
                "cannot read the cache pool's branches from {}: {e}; using backing_branches from the config, without checking that the cache branch is the cache pool's first branch",
                control.display()
            );
            return Ok(None);
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "cannot read the cache pool's branches from {}; set backing_branches in the config to list them instead",
                    control.display()
                )
            });
        }
    };
    let in_cache_pool = |e: String| anyhow!("cache pool {}: {e}", cache_pool.display());
    let mut specs = branches
        .split(|&b| b == b':')
        .map(BranchSpec::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(in_cache_pool)?;
    let minfreespace = read_value(&control, "minfreespace").with_context(|| {
        format!(
            "cannot read the cache pool's minfreespace from {}",
            control.display()
        )
    })?;
    let minfreespace = parse_minfreespace(&minfreespace).map_err(in_cache_pool)?;
    for spec in &mut specs {
        spec.minfreespace.get_or_insert(minfreespace);
    }

    let first = specs.remove(0);
    let first_is_cache_branch = Branch::open(&first.path)
        .and_then(|first| first.same_dir(cache_branch))
        .unwrap_or(false);
    if !first_is_cache_branch {
        bail!(
            "the cache branch {} is not the first branch of the cache pool {}: its first branch is {}, so applications would see that branch's copy of a file in place of the cache branch's",
            cache_branch.path.display(),
            cache_pool.display(),
            first.path.display()
        );
    }

    let cache_minfreespace = first.minfreespace.unwrap_or(minfreespace);
    if threshold < cache_minfreespace {
        log::warning!(
            "the threshold ({threshold} bytes) is below the cache pool's minfreespace for the cache branch ({cache_minfreespace} bytes): mergerfs creates new files on the backing branches before a check finds work"
        );
    }
    // Unreadable on old mergerfs versions, which then cannot be checked.
    if let Ok(moveonenospc) = read_value(&control, "moveonenospc") {
        // `erofs` is the one create policy that never chooses a branch, and
        // policies never choose a branch that takes no new files.
        let enabled = !matches!(&moveonenospc[..], b"false" | b"erofs");
        if enabled && first.mode == Mode::ReadWrite {
            log::warning!(
                "the cache pool has moveonenospc={}: when a write to a backing branch runs out of space, mergerfs can move the file back onto the cache branch; set moveonenospc=false",
                moveonenospc.escape_ascii()
            );
        }
    }
    Ok(Some(specs))
}

/// The value of `user.mergerfs.<key>` on the control file `control`.
fn read_value(control: &Path, key: &str) -> io::Result<Vec<u8>> {
    let name = format!("user.mergerfs.{key}");
    let mut value = vec![0; 4096];
    loop {
        match rustix::fs::getxattr(control, name.as_str(), &mut value[..]) {
            Ok(len) => {
                value.truncate(len);
                // Some versions count a terminating NUL.
                while value.last() == Some(&0) {
                    value.pop();
                }
                return Ok(value);
            }
            Err(rustix::io::Errno::RANGE) if value.len() < 1 << 20 => {
                value.resize(value.len() * 4, 0);
            }
            Err(e) => return Err(e.into()),
        }
    }
}
