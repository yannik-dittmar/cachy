//! The YAML configuration: parsed and validated into a typed, immutable value.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::exclude::Exclusions;
use crate::layout::BranchSpec;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub cache_branch: PathBuf,
    /// The cache pool's mount point, whose mergerfs runtime interface lists
    /// the backing branches.
    #[serde(default)]
    pub cache_pool: Option<PathBuf>,
    /// The backing branches, in mergerfs's branch syntax, for when there is no
    /// cache pool or its runtime interface is unavailable.
    #[serde(default, deserialize_with = "branch_specs")]
    pub backing_branches: Option<Vec<BranchSpec>>,
    #[serde(deserialize_with = "free_space")]
    pub threshold: FreeSpace,
    #[serde(deserialize_with = "free_space")]
    pub target: FreeSpace,
    /// The timestamp that decides which move units a run moves first.
    #[serde(default)]
    pub order: Order,
    #[serde(default = "default_minimum_age", deserialize_with = "duration")]
    pub minimum_age: Duration,
    /// Move units unchanged for longer than this count as work for any check,
    /// whatever the free space. Always above the minimum age.
    #[serde(default, deserialize_with = "optional_duration")]
    pub max_age: Option<Duration>,
    #[serde(default)]
    pub lease_type: LeaseType,
    /// Kinds of metadata that a move may lose, with a warning, when the
    /// backing branch cannot store them. Any other loss fails the move.
    #[serde(default)]
    pub allow_metadata_loss: Vec<MetadataKind>,
    #[serde(default)]
    pub verify: Verify,
    /// Gitignore patterns, as lines of a `.gitignore` at the cache branch
    /// root, for files and directories that stay on the cache branch.
    #[serde(default, deserialize_with = "exclusions")]
    pub exclude: Exclusions,
}

/// An amount of free space on the cache branch: a byte size, or a percentage
/// of the filesystem's size, resolved when a run starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreeSpace {
    Bytes(u64),
    /// `digits` × 10^-`decimals` percent, kept exact.
    Percent {
        digits: u64,
        decimals: u32,
    },
}

/// Most decimals a percentage may have; enough for a byte on a 100 TB
/// filesystem.
const MAX_PERCENT_DECIMALS: u32 = 12;

impl FreeSpace {
    /// The number of bytes this is on a filesystem of `size` bytes, rounded
    /// down.
    pub fn resolve(self, size: u64) -> u64 {
        match self {
            FreeSpace::Bytes(bytes) => bytes,
            FreeSpace::Percent { digits, decimals } => {
                let scale = 100 * 10u128.pow(decimals);
                (size as u128 * digits as u128 / scale) as u64
            }
        }
    }

    /// Whether this is above `other` whatever the filesystem's size, or
    /// `None` if that depends on the size.
    fn above(self, other: FreeSpace) -> Option<bool> {
        match (self, other) {
            (FreeSpace::Bytes(a), FreeSpace::Bytes(b)) => Some(a > b),
            (
                FreeSpace::Percent {
                    digits: a,
                    decimals: ad,
                },
                FreeSpace::Percent {
                    digits: b,
                    decimals: bd,
                },
            ) => Some(a as u128 * 10u128.pow(bd) > b as u128 * 10u128.pow(ad)),
            _ => None,
        }
    }
}

impl std::fmt::Display for FreeSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match *self {
            FreeSpace::Bytes(bytes) => write!(f, "{bytes} bytes"),
            FreeSpace::Percent { digits, decimals } => {
                let digits = format!("{digits:0>width$}", width = decimals as usize + 1);
                let (whole, fraction) = digits.split_at(digits.len() - decimals as usize);
                if fraction.is_empty() {
                    write!(f, "{whole}%")
                } else {
                    write!(f, "{whole}.{fraction}%")
                }
            }
        }
    }
}

/// The timestamp by which a run orders move units, oldest first.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    /// Last access: least recently used first.
    #[default]
    Atime,
    /// Last modification: oldest content first.
    Mtime,
}

/// How a move checks its copy before publishing it.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Verify {
    /// Only hash the source while copying it.
    #[default]
    Hash,
    /// Also read the copy back from storage once it is fsynced, and fail the
    /// move unless its hash matches the source's.
    Readback,
}

/// Metadata that a move preserves only where the backing branch supports it,
/// if the configuration allows its loss.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MetadataKind {
    /// Extended attributes other than ACLs.
    Xattrs,
    /// POSIX ACLs (`system.posix_acl_access` and `system.posix_acl_default`).
    Acls,
}

/// The kernel lease cachy holds on a move unit while moving it, which decides
/// what makes the move unit in use.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseType {
    /// A read lease (`F_RDLCK`): only opens for writing make a move unit in
    /// use, so files that are only being read still move.
    #[default]
    Read,
    /// A write lease (`F_WRLCK`): any open, even for reading only, makes a
    /// move unit in use.
    Write,
}

fn branch_specs<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Option<Vec<BranchSpec>>, D::Error> {
    let specs = Vec::<String>::deserialize(de)?;
    specs
        .iter()
        .map(|spec| BranchSpec::parse(spec.as_bytes()))
        .collect::<Result<_, _>>()
        .map(Some)
        .map_err(|e| serde::de::Error::custom(format!("backing_branches: {e}")))
}

fn exclusions<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Exclusions, D::Error> {
    let patterns = Vec::<String>::deserialize(de)?;
    Exclusions::new(&patterns).map_err(|e| serde::de::Error::custom(format!("exclude: {e}")))
}

fn default_minimum_age() -> Duration {
    Duration::from_secs(15 * 60)
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        Config::parse(&text).with_context(|| format!("invalid config {}", path.display()))
    }

    fn parse(text: &str) -> Result<Config> {
        let config: Config = serde_yaml_ng::from_str(text)?;
        match &config.backing_branches {
            None if config.cache_pool.is_none() => {
                bail!(
                    "set cache_pool, backing_branches or both, so cachy finds the backing branches"
                )
            }
            Some(specs) if specs.is_empty() => {
                bail!("backing_branches must list at least one backing branch")
            }
            _ => {}
        }
        if config.target.above(config.threshold) == Some(false) {
            bail!(
                "target ({}) must be above the threshold ({})",
                config.target,
                config.threshold
            );
        }
        config.check_max_age()?;
        Ok(config)
    }

    /// Replaces the configured max age, such as with one given on the command
    /// line.
    pub fn override_max_age(&mut self, max_age: Duration) -> Result<()> {
        self.max_age = Some(max_age);
        self.check_max_age()
    }

    fn check_max_age(&self) -> Result<()> {
        if let Some(max_age) = self.max_age
            && max_age <= self.minimum_age
        {
            bail!(
                "max_age ({max_age:?}) must be above the minimum age ({:?})",
                self.minimum_age
            );
        }
        Ok(())
    }
}

/// A threshold or target: a byte size, or a percentage such as `12.5%`.
fn free_space<'de, D: serde::Deserializer<'de>>(de: D) -> Result<FreeSpace, D::Error> {
    struct FreeSpaceVisitor;

    impl serde::de::Visitor<'_> for FreeSpaceVisitor {
        type Value = FreeSpace;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a byte size such as 120G, or a percentage such as 10%")
        }

        fn visit_u64<E: serde::de::Error>(self, n: u64) -> Result<FreeSpace, E> {
            Ok(FreeSpace::Bytes(n))
        }

        fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<FreeSpace, E> {
            if let Some(percent) = s.strip_suffix('%') {
                return parse_percent(percent).ok_or_else(|| {
                    E::custom(format!(
                        "invalid percentage `{s}`: expected a number from 0 to 100, with at most {MAX_PERCENT_DECIMALS} decimals"
                    ))
                });
            }
            parse_byte_size(s).map(FreeSpace::Bytes).ok_or_else(|| {
                E::custom(format!(
                    "invalid byte size `{s}`: expected a whole number with an optional K, M, G or T suffix, or a percentage"
                ))
            })
        }
    }

    de.deserialize_any(FreeSpaceVisitor)
}

/// Parses a percentage without its `%`, such as `12.5`, from 0 to 100.
fn parse_percent(s: &str) -> Option<FreeSpace> {
    let (whole, fraction) = s.split_once('.').unwrap_or((s, ""));
    let all_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !all_digits(whole) || !all_digits(fraction) {
        return None;
    }
    if s.contains('.') && fraction.is_empty() {
        return None;
    }
    let decimals = fraction.len() as u32;
    if decimals > MAX_PERCENT_DECIMALS {
        return None;
    }
    let digits: u64 = format!("{whole}{fraction}").parse().ok()?;
    let percent = FreeSpace::Percent { digits, decimals };
    let hundred = FreeSpace::Percent {
        digits: 100,
        decimals: 0,
    };
    (!percent.above(hundred)?).then_some(percent)
}

/// A duration: an integer with an `s`, `m`, `h` or `d` suffix (`15m` is 900
/// seconds).
fn duration<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Duration, D::Error> {
    struct DurationVisitor;

    impl serde::de::Visitor<'_> for DurationVisitor {
        type Value = Duration;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a duration such as 15m")
        }

        fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Duration, E> {
            parse_duration(s).ok_or_else(|| {
                E::custom(format!(
                    "invalid duration `{s}`: expected a whole number with an s, m, h or d suffix"
                ))
            })
        }
    }

    de.deserialize_str(DurationVisitor)
}

fn optional_duration<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Option<Duration>, D::Error> {
    duration(de).map(Some)
}

/// Parses a duration such as `15m`: an integer with an `s`, `m`, `h` or `d`
/// suffix.
pub fn parse_duration(s: &str) -> Option<Duration> {
    const UNITS: &[(u8, u64)] = &[(b's', 1), (b'm', 60), (b'h', 60 * 60), (b'd', 24 * 60 * 60)];
    parse_with_unit(s, UNITS, None).map(Duration::from_secs)
}

pub fn parse_byte_size(s: &str) -> Option<u64> {
    const UNITS: &[(u8, u64)] = &[
        (b'K', 1 << 10),
        (b'M', 1 << 20),
        (b'G', 1 << 30),
        (b'T', 1 << 40),
    ];
    parse_with_unit(s, UNITS, Some(1))
}

/// Parses a whole number followed by one of the unit suffixes in `units`, or
/// by no suffix if `bare` gives the unit for that case.
fn parse_with_unit(s: &str, units: &[(u8, u64)], bare: Option<u64>) -> Option<u64> {
    let suffixed = units
        .iter()
        .find(|(suffix, _)| s.as_bytes().last() == Some(suffix));
    let (digits, unit) = match suffixed {
        Some((_, unit)) => (&s[..s.len() - 1], *unit),
        None => (s, bare?),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(unit)
}
