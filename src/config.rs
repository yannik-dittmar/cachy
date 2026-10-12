//! The YAML configuration: parsed and validated into a typed, immutable value.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub cache_branch: PathBuf,
    pub backing_branches: Vec<PathBuf>,
    #[serde(deserialize_with = "byte_size")]
    pub threshold: u64,
    #[serde(deserialize_with = "byte_size")]
    pub target: u64,
    #[serde(default = "default_minimum_age", deserialize_with = "duration")]
    pub minimum_age: Duration,
    #[serde(default)]
    pub lease_type: LeaseType,
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
        if config.backing_branches.len() != 1 {
            bail!(
                "backing_branches must list exactly one backing branch (found {})",
                config.backing_branches.len()
            );
        }
        if config.target <= config.threshold {
            bail!(
                "target ({} bytes) must be above the threshold ({} bytes)",
                config.target,
                config.threshold
            );
        }
        Ok(config)
    }
}

/// A byte size: a plain integer, or an integer with a binary `K`, `M`, `G` or
/// `T` suffix (`120G` is 120 × 1024³ bytes).
fn byte_size<'de, D: serde::Deserializer<'de>>(de: D) -> Result<u64, D::Error> {
    struct ByteSize;

    impl serde::de::Visitor<'_> for ByteSize {
        type Value = u64;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a byte size such as 120G")
        }

        fn visit_u64<E: serde::de::Error>(self, n: u64) -> Result<u64, E> {
            Ok(n)
        }

        fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<u64, E> {
            parse_byte_size(s).ok_or_else(|| {
                E::custom(format!(
                    "invalid byte size `{s}`: expected a whole number with an optional K, M, G or T suffix"
                ))
            })
        }
    }

    de.deserialize_any(ByteSize)
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

fn parse_duration(s: &str) -> Option<Duration> {
    const UNITS: &[(u8, u64)] = &[(b's', 1), (b'm', 60), (b'h', 60 * 60), (b'd', 24 * 60 * 60)];
    parse_with_unit(s, UNITS, None).map(Duration::from_secs)
}

fn parse_byte_size(s: &str) -> Option<u64> {
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
