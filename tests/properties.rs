//! Property tests: random cache-branch trees, with odd byte names, hardlink
//! groups, symlinks, sparse and zero-byte files and exclusions, checked
//! against the core invariant of a run:
//!
//! - every move unit exists exactly once per relpath, on the cache branch or
//!   on the backing branch, with equal content and metadata;
//! - excluded move units, and hardlink groups with an excluded name, stay on
//!   the cache branch untouched;
//! - afterwards either the target is reached or no eligible move unit remains
//!   on the cache branch.
//!
//! Each case runs the real binary. `PROPTEST_CASES` sets the number of cases
//! (32 by default); a failing case shrinks to a minimal tree, which proptest
//! saves under `tests/proptest-regressions/` and replays first on later runs.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use proptest::test_runner::{Config, FileFailurePersistence, TestCaseError};

use common::{Fixture, STAGING, assert_exit, set_xattr, xattrs};

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;

/// A relpath, as its components' bytes.
type RelPath = Vec<Vec<u8>>;

/// Names that exclusion patterns use, and so match.
const PATTERN_NAMES: [&str; 3] = ["keep", "tmp", "cache"];

/// The extension that the `*.part` pattern matches.
const EXTENSION: &str = ".part";

/// Names that a shell, a glob or a gitignore would read specially, or that
/// are not valid UTF-8.
const ODD_NAMES: [&[u8]; 16] = [
    b" leading space",
    b"trailing space ",
    b"-rf",
    b"new\nline",
    b"tab\there",
    b"back\\slash",
    b"*",
    b"[ab]",
    b"#hash",
    b"!bang",
    b"...",
    b".hidden",
    b"\xff\xfe not utf-8",
    "caf\u{e9}".as_bytes(),
    "\u{1f4be}".as_bytes(),
    b"x.part.not",
];

fn name() -> impl Strategy<Value = Vec<u8>> {
    let plain = prop::sample::select(vec!["a", "b", "keep", "tmp", "cache", "x.part"])
        .prop_map(|n| n.as_bytes().to_vec());
    let odd = prop::sample::select(ODD_NAMES.to_vec()).prop_map(<[u8]>::to_vec);
    // Any bytes but NUL and `/`, which no name can hold.
    let random = prop::collection::vec(
        any::<u8>().prop_filter("not NUL or /", |b| *b != 0 && *b != b'/'),
        1..12,
    )
    .prop_filter("not . or ..", |n| n != b"." && n != b"..");
    let with_extension = prop_oneof![random.clone(), odd.clone()].prop_map(|mut n| {
        n.extend_from_slice(EXTENSION.as_bytes());
        n
    });
    prop_oneof![4 => plain, 2 => odd, 2 => random, 1 => with_extension]
}

fn relpath() -> impl Strategy<Value = RelPath> {
    prop::collection::vec(name(), 1..4)
}

#[derive(Clone, Debug)]
enum Body {
    Dense(Vec<u8>),
    /// A file of `len` bytes with data at the given offsets and holes
    /// elsewhere, including at its end.
    Sparse {
        len: u64,
        extents: Vec<(u64, Vec<u8>)>,
    },
}

impl Body {
    fn content(&self) -> Vec<u8> {
        match self {
            Body::Dense(bytes) => bytes.clone(),
            Body::Sparse { len, extents } => {
                let mut content = vec![0; *len as usize];
                for (offset, data) in extents {
                    content[*offset as usize..*offset as usize + data.len()].copy_from_slice(data);
                }
                content
            }
        }
    }
}

fn body() -> impl Strategy<Value = Body> {
    let dense = prop_oneof![
        Just(Vec::new()),
        prop::collection::vec(any::<u8>(), 1..4096),
        // Spans several copy buffers.
        (1..=3usize, any::<u8>()).prop_map(|(mib, seed)| (0..mib * MIB as usize + 7)
            .map(|i| (i as u8) ^ seed)
            .collect()),
    ]
    .prop_map(Body::Dense);
    // Data stays far below half the length, even on a filesystem that
    // allocates 128 KiB for each extent (ZFS), so a copy that fills the holes
    // shows.
    let sparse = (2..=8u64)
        .prop_flat_map(|mib| {
            let len = mib * MIB;
            let extent = (
                0..len / KIB - 4,
                prop::collection::vec(any::<u8>(), 1..4096),
            )
                .prop_map(|(kib, data)| (kib * KIB, data));
            (Just(len), prop::collection::vec(extent, 0..3))
        })
        .prop_map(|(len, extents)| Body::Sparse { len, extents });
    prop_oneof![3 => dense, 1 => sparse]
}

#[derive(Clone, Copy, Debug)]
struct Times {
    atime: (i64, u32),
    mtime: (i64, u32),
}

fn times() -> impl Strategy<Value = Times> {
    // Up to 2023: a file with an mtime in the future is too young to move.
    let time = (1_000i64..1_700_000_000, 0u32..1_000_000_000);
    (time.clone(), time).prop_map(|(atime, mtime)| Times { atime, mtime })
}

#[derive(Clone, Debug)]
enum Unit {
    /// A regular file, and a hardlink group if `links` is not empty.
    File {
        path: RelPath,
        links: Vec<RelPath>,
        body: Body,
        mode: u32,
        xattrs: Vec<(u8, Vec<u8>)>,
        times: Times,
    },
    Symlink {
        path: RelPath,
        target: Vec<u8>,
        times: Times,
    },
}

fn unit() -> impl Strategy<Value = Unit> {
    let mode = prop::sample::select(vec![
        0o644, 0o600, 0o640, 0o755, 0o700, 0o444, 0o400, 0o4755,
    ]);
    let xattr = (0..3u8, prop::collection::vec(any::<u8>(), 0..32));
    let file = (
        relpath(),
        prop::collection::vec(relpath(), 0..3),
        body(),
        mode,
        prop::collection::vec(xattr, 0..2),
        times(),
    )
        .prop_map(|(path, links, body, mode, xattrs, times)| Unit::File {
            path,
            links,
            body,
            mode,
            xattrs,
            times,
        });
    let target = prop_oneof![
        relpath().prop_map(|p| p.join(&b'/')),
        prop::collection::vec(any::<u8>().prop_filter("not NUL", |b| *b != 0), 1..64),
    ];
    let symlink = (relpath(), target, times()).prop_map(|(path, target, times)| Unit::Symlink {
        path,
        target,
        times,
    });
    prop_oneof![3 => file, 1 => symlink]
}

#[derive(Clone, Debug)]
enum Pattern {
    /// `*.part`: names with the extension, at any depth.
    Extension,
    /// `<name>`: files and directories of that name, at any depth.
    Name(&'static str),
    /// `<name>/`: directories of that name, at any depth.
    Directory(&'static str),
    /// `/<name>/`: the directory of that name at the cache branch root.
    Anchored(&'static str),
}

impl Pattern {
    fn line(&self) -> String {
        match self {
            Pattern::Extension => format!("*{EXTENSION}"),
            Pattern::Name(n) => (*n).to_owned(),
            Pattern::Directory(n) => format!("{n}/"),
            Pattern::Anchored(n) => format!("/{n}/"),
        }
    }

    /// Whether this pattern matches the component at `depth`, which is a
    /// directory if `is_dir`; the oracle that the run is checked against.
    fn matches(&self, component: &[u8], depth: usize, is_dir: bool) -> bool {
        match self {
            Pattern::Extension => component.ends_with(EXTENSION.as_bytes()),
            Pattern::Name(n) => component == n.as_bytes(),
            Pattern::Directory(n) => is_dir && component == n.as_bytes(),
            Pattern::Anchored(n) => depth == 0 && is_dir && component == n.as_bytes(),
        }
    }
}

fn pattern() -> impl Strategy<Value = Pattern> {
    let name = prop::sample::select(PATTERN_NAMES.to_vec());
    prop_oneof![
        Just(Pattern::Extension),
        name.clone().prop_map(Pattern::Name),
        name.clone().prop_map(Pattern::Directory),
        name.prop_map(Pattern::Anchored),
    ]
}

/// How much the run is asked to free.
#[derive(Clone, Copy, Debug)]
enum Goal {
    /// Every eligible move unit, as `--drain` would.
    Drain,
    /// A target this many thousandths of the eligible move units' space
    /// above free space; above 1000, unreachable.
    Target(u64),
}

fn goal() -> impl Strategy<Value = Goal> {
    prop_oneof![Just(Goal::Drain), (0..1_200u64).prop_map(Goal::Target)]
}

fn excluded(patterns: &[Pattern], path: &RelPath) -> bool {
    path.iter().enumerate().any(|(depth, component)| {
        let is_dir = depth + 1 < path.len();
        patterns.iter().any(|p| p.matches(component, depth, is_dir))
    })
}

fn path_on(branch: &Path, rel: &RelPath) -> PathBuf {
    rel.iter()
        .fold(branch.to_path_buf(), |p, c| p.join(OsStr::from_bytes(c)))
}

fn show(rel: &RelPath) -> String {
    String::from_utf8_lossy(&rel.join(&b'/')).into_owned()
}

/// A move unit as created on the cache branch, with what a run must keep.
#[derive(Debug)]
struct Created {
    names: Vec<RelPath>,
    /// Content, or link text for a symlink.
    content: Vec<u8>,
    symlink: bool,
    sparse: bool,
    excluded: bool,
    times: Times,
    before: fs::Metadata,
    xattrs: BTreeMap<String, Vec<u8>>,
}

/// The paths that the tree created so far occupies: files and symlinks,
/// and directories.
#[derive(Default)]
struct Occupied {
    leaves: BTreeSet<RelPath>,
    dirs: BTreeSet<RelPath>,
}

impl Occupied {
    /// Claims `path` for a file or symlink, unless the tree already uses it
    /// or one of its ancestors as something else.
    fn claim(&mut self, path: &RelPath) -> bool {
        let root_staging = path[0] == STAGING.as_bytes();
        let ancestor_is_leaf = (1..path.len()).any(|i| self.leaves.contains(&path[..i]));
        if root_staging
            || ancestor_is_leaf
            || self.leaves.contains(path)
            || self.dirs.contains(path)
        {
            return false;
        }
        self.leaves.insert(path.clone());
        for i in 1..path.len() {
            self.dirs.insert(path[..i].to_vec());
        }
        true
    }
}

fn create(fx: &Fixture, units: &[Unit], patterns: &[Pattern]) -> Vec<Created> {
    let mut occupied = Occupied::default();
    let mut created = Vec::new();
    for unit in units {
        match unit {
            Unit::File {
                path,
                links,
                body,
                mode,
                xattrs: attrs,
                times,
            } => {
                if !occupied.claim(path) {
                    continue;
                }
                let file = path_on(fx.cache_branch.path(), path);
                common::write_file(&file, b"", 0);
                write_body(&file, body);
                for (key, value) in attrs {
                    set_xattr(&file, &format!("user.prop{key}"), value).unwrap();
                }
                fs::set_permissions(&file, fs::Permissions::from_mode(*mode)).unwrap();
                set_times_ns(&file, *times);
                let mut names = vec![path.clone()];
                for link in links {
                    if occupied.claim(link) {
                        let link_path = path_on(fx.cache_branch.path(), link);
                        fs::create_dir_all(link_path.parent().unwrap()).unwrap();
                        fs::hard_link(&file, &link_path).unwrap();
                        names.push(link.clone());
                    }
                }
                created.push(Created {
                    excluded: names.iter().any(|n| excluded(patterns, n)),
                    names,
                    content: body.content(),
                    symlink: false,
                    sparse: matches!(body, Body::Sparse { .. }),
                    times: *times,
                    before: fs::symlink_metadata(&file).unwrap(),
                    xattrs: xattrs(&file),
                });
            }
            Unit::Symlink {
                path,
                target,
                times,
            } => {
                if !occupied.claim(path) {
                    continue;
                }
                let link = path_on(fx.cache_branch.path(), path);
                fs::create_dir_all(link.parent().unwrap()).unwrap();
                symlink(OsStr::from_bytes(target), &link).unwrap();
                set_times_ns(&link, *times);
                created.push(Created {
                    names: vec![path.clone()],
                    content: target.clone(),
                    symlink: true,
                    sparse: false,
                    excluded: excluded(patterns, path),
                    times: *times,
                    before: fs::symlink_metadata(&link).unwrap(),
                    xattrs: BTreeMap::new(),
                });
            }
        }
    }
    created
}

fn write_body(path: &Path, body: &Body) {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    match body {
        Body::Dense(bytes) => file.write_all_at(bytes, 0).unwrap(),
        Body::Sparse { len, extents } => {
            for (offset, data) in extents {
                file.write_all_at(data, *offset).unwrap();
            }
            file.set_len(*len).unwrap();
        }
    }
}

fn set_times_ns(path: &Path, times: Times) {
    let ts = |(tv_sec, tv_nsec): (i64, u32)| rustix::fs::Timespec {
        tv_sec,
        tv_nsec: tv_nsec.into(),
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        path,
        &rustix::fs::Timestamps {
            last_access: ts(times.atime),
            last_modification: ts(times.mtime),
        },
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
}

/// Relpaths of the files and symlinks under `root`, not following symlinks.
fn leaves(root: &Path) -> BTreeSet<RelPath> {
    fn walk(dir: &Path, prefix: &RelPath, out: &mut BTreeSet<RelPath>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let mut rel = prefix.clone();
            rel.push(entry.file_name().as_bytes().to_vec());
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), &rel, out);
            } else {
                out.insert(rel);
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(root, &Vec::new(), &mut out);
    out
}

fn allocated(meta: &fs::Metadata) -> u64 {
    meta.blocks() * 512
}

/// Checks that `unit` is on `branch`, as it was created, under all its
/// names. A unit left on the cache branch must be untouched: the same inode,
/// with the same ctime.
fn check_unit(unit: &Created, branch: &Path, moved: bool) -> Result<(), TestCaseError> {
    let first = path_on(branch, &unit.names[0]);
    let after = fs::symlink_metadata(&first).unwrap();
    let before = &unit.before;
    let what = show(&unit.names[0]);
    if unit.symlink {
        prop_assert!(after.file_type().is_symlink(), "{what}: not a symlink");
        prop_assert_eq!(
            fs::read_link(&first).unwrap().into_os_string().into_vec(),
            unit.content.clone(),
            "{}: link text",
            what
        );
    } else {
        prop_assert!(after.file_type().is_file(), "{what}: not a regular file");
        prop_assert!(
            fs::read(&first).unwrap() == unit.content,
            "{what}: content differs"
        );
        prop_assert_eq!(after.mode(), before.mode(), "{}: mode", what);
        prop_assert_eq!(
            after.nlink(),
            unit.names.len() as u64,
            "{}: link count",
            what
        );
        prop_assert_eq!(&xattrs(&first), &unit.xattrs, "{}: xattrs", what);
        if unit.sparse && moved && allocated(before) < after.size() / 4 {
            prop_assert!(
                allocated(&after) < after.size() / 2,
                "{what}: {} bytes allocated, the holes were filled",
                allocated(&after)
            );
        }
    }
    prop_assert_eq!(after.size(), before.size(), "{}: size", what);
    prop_assert_eq!(
        (after.uid(), after.gid()),
        (before.uid(), before.gid()),
        "{}: owner",
        what
    );
    prop_assert_eq!(
        (after.atime(), after.atime_nsec()),
        (unit.times.atime.0, i64::from(unit.times.atime.1)),
        "{}: atime",
        what
    );
    prop_assert_eq!(
        (after.mtime(), after.mtime_nsec()),
        (before.mtime(), before.mtime_nsec()),
        "{}: mtime",
        what
    );
    if !moved {
        prop_assert_eq!(after.ino(), before.ino(), "{}: replaced", what);
        prop_assert_eq!(
            (after.ctime(), after.ctime_nsec()),
            (before.ctime(), before.ctime_nsec()),
            "{}: changed",
            what
        );
    }
    for name in &unit.names[1..] {
        let link = fs::symlink_metadata(path_on(branch, name)).unwrap();
        prop_assert_eq!(
            (link.dev(), link.ino()),
            (after.dev(), after.ino()),
            "{} is not a link to {}",
            show(name),
            what
        );
    }
    Ok(())
}

fn run_case(units: &[Unit], patterns: &[Pattern], goal: Goal) -> Result<(), TestCaseError> {
    let _lock = common::free_space_lock();
    let fx = Fixture::new();
    let created = create(&fx, units, patterns);

    let (mut config, target) = match goal {
        Goal::Drain => (fx.move_everything_config(), None),
        Goal::Target(permille) => {
            let free = fx.cache_branch_free();
            let eligible: u64 = created
                .iter()
                .filter(|u| !u.excluded)
                .map(|u| {
                    allocated(
                        &fs::symlink_metadata(path_on(fx.cache_branch.path(), &u.names[0]))
                            .unwrap(),
                    )
                })
                .sum();
            let threshold = free + 1;
            let target = threshold + 1 + eligible * permille / 1000;
            let config = fx.config(&threshold.to_string(), &target.to_string());
            (config, Some(target))
        }
    };
    if !patterns.is_empty() {
        config += "exclude:\n";
        for pattern in patterns {
            config += &format!("  - '{}'\n", pattern.line());
        }
    }

    // A fault-injection build retires sources without waiting, which makes
    // cases, and shrinking, much faster; a normal build ignores this.
    let out = common::output(fx.command(&config).env("CACHY_RETIRE_WAIT_MS", "0")).unwrap();
    let err = assert_exit(&out, 0);
    prop_assert!(err.contains(", 0 failed"), "stderr:\n{}", err);
    fx.sync_backing_branch();

    let on_cache = leaves(fx.cache_branch.path());
    let on_backing = leaves(fx.backing_branch.path());
    let mut expected_cache = BTreeSet::new();
    let mut expected_backing = BTreeSet::new();
    let mut eligible_left = Vec::new();
    for unit in &created {
        let where_first = (
            on_cache.contains(&unit.names[0]),
            on_backing.contains(&unit.names[0]),
        );
        let moved = match where_first {
            (true, false) => false,
            (false, true) => true,
            (true, true) => {
                return Err(TestCaseError::fail(format!(
                    "{} is on both branches",
                    show(&unit.names[0])
                )));
            }
            (false, false) => {
                return Err(TestCaseError::fail(format!(
                    "{} is on neither branch",
                    show(&unit.names[0])
                )));
            }
        };
        prop_assert!(
            !(moved && unit.excluded),
            "{} was moved, but is excluded",
            show(&unit.names[0])
        );
        if !moved && !unit.excluded {
            eligible_left.push(show(&unit.names[0]));
        }
        let (branch, expected) = if moved {
            (fx.backing_branch.path(), &mut expected_backing)
        } else {
            (fx.cache_branch.path(), &mut expected_cache)
        };
        expected.extend(unit.names.iter().cloned());
        check_unit(unit, branch, moved)?;
    }
    // Every name is on the branch its unit is on, and nothing else is on
    // either branch: no strays, and no staging directory.
    prop_assert_eq!(on_cache, expected_cache, "names on the cache branch");
    prop_assert_eq!(on_backing, expected_backing, "names on the backing branch");
    fx.assert_no_staging_directories();

    if !eligible_left.is_empty() {
        let free = fx.cache_branch_free();
        prop_assert!(
            target.is_some_and(|target| free >= target),
            "eligible move units are left on the cache branch: {eligible_left:?}, with {free} bytes free and target {target:?}\nstderr:\n{err}"
        );
    }
    Ok(())
}

/// The configured number of cases, or 32: each case runs cachy, which takes
/// about a second in a normal build. Shrinking is bounded in time too.
fn config() -> Config {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(32);
    Config {
        cases,
        max_shrink_iters: 1_024,
        max_shrink_time: 300_000,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/proptest-regressions/properties.txt",
        ))),
        ..Config::default()
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn run_keeps_every_byte_exactly_once(
        units in prop::collection::vec(unit(), 0..12),
        patterns in prop::collection::vec(pattern(), 0..3),
        goal in goal(),
    ) {
        run_case(&units, &patterns, goal)?;
    }
}
