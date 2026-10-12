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

/// Which opens by other processes keep a move unit from being moved.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseType {
    /// Opens for writing (`F_RDLCK`).
    #[default]
    Read,
    /// Any open, even for reading only (`F_WRLCK`).
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
    struct Dur;

    impl serde::de::Visitor<'_> for Dur {
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

    de.deserialize_str(Dur)
}

fn parse_duration(s: &str) -> Option<Duration> {
    let (digits, unit) = match s.as_bytes().last()? {
        b's' => (&s[..s.len() - 1], 1),
        b'm' => (&s[..s.len() - 1], 60),
        b'h' => (&s[..s.len() - 1], 60 * 60),
        b'd' => (&s[..s.len() - 1], 24 * 60 * 60),
        _ => return None,
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(Duration::from_secs(
        digits.parse::<u64>().ok()?.checked_mul(unit)?,
    ))
}

fn parse_byte_size(s: &str) -> Option<u64> {
    let (digits, shift) = match s.as_bytes().last()? {
        b'K' => (&s[..s.len() - 1], 10),
        b'M' => (&s[..s.len() - 1], 20),
        b'G' => (&s[..s.len() - 1], 30),
        b'T' => (&s[..s.len() - 1], 40),
        _ => (s, 0),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(1 << shift)
}
