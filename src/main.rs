mod config;
mod exclude;
#[cfg(feature = "fault-injection")]
mod fault;
mod fsops;
mod layout;
mod lock;
mod metadata;
mod placement;
mod probe;
mod proc_scan;
mod recovery;
mod run;
mod scan;
mod stop;
mod transaction;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};

use config::Config;
use run::{Options, run};

/// Exit code when cachy refused to run.
const EXIT_REFUSED: u8 = 1;
/// Exit code when at least one move failed, or the run stopped early.
const EXIT_MOVE_FAILED: u8 = 2;

#[derive(Parser)]
#[command(about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check the cache branch's free space and run if it is below the threshold.
    Run {
        /// Path to the YAML configuration file.
        #[arg(long)]
        config: PathBuf,
        /// Write nothing; print the byte budget, the move units a run would
        /// move, in order, and every skip with its reason.
        #[arg(long)]
        dry_run: bool,
        /// Skip the check and move every eligible move unit.
        #[arg(long)]
        drain: bool,
        /// Override the configured max age, such as `7d`.
        #[arg(long, value_parser = max_age)]
        max_age: Option<Duration>,
    },
}

fn max_age(s: &str) -> Result<Duration, String> {
    config::parse_duration(s)
        .ok_or_else(|| format!("expected a whole number with an s, m, h or d suffix, not `{s}`"))
}

/// The filesystem operations a run performs.
#[cfg(not(feature = "fault-injection"))]
fn fs() -> anyhow::Result<fsops::RealFs> {
    Ok(fsops::RealFs)
}

/// The filesystem operations a run performs, with any fault that the
/// environment asks for injected.
#[cfg(feature = "fault-injection")]
fn fs() -> anyhow::Result<fault::FaultyFs> {
    fault::FaultyFs::from_env()
}

/// The filesystem operations the capability probe performs.
#[cfg(not(feature = "fault-injection"))]
fn probe_fs() -> anyhow::Result<fsops::RealFs> {
    Ok(fsops::RealFs)
}

/// The filesystem operations the capability probe performs, counted apart
/// from a run's others, with any fault that the environment asks for
/// injected.
#[cfg(feature = "fault-injection")]
fn probe_fs() -> anyhow::Result<fault::FaultyFs> {
    fault::FaultyFs::probe_from_env()
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            // Usage errors are refusals; help output is not an error.
            return if e.use_stderr() {
                ExitCode::from(EXIT_REFUSED)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    match cli.command {
        Command::Run {
            config,
            dry_run,
            drain,
            max_age,
        } => {
            let config = Config::load(&config).and_then(|mut config| {
                if let Some(max_age) = max_age {
                    config
                        .override_max_age(max_age)
                        .context("invalid --max-age")?;
                }
                Ok(config)
            });
            let options = Options { dry_run, drain };
            match config.and_then(|c| run(&fs()?, &probe_fs()?, &c, options)) {
                Ok(outcome) if outcome.succeeded() => ExitCode::SUCCESS,
                Ok(_) => ExitCode::from(EXIT_MOVE_FAILED),
                Err(e) => {
                    eprintln!("cachy: error: {e:#}");
                    ExitCode::from(EXIT_REFUSED)
                }
            }
        }
    }
}
