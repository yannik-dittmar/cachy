mod config;
mod fsops;
mod run;
mod scan;
mod transaction;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use config::Config;
use fsops::RealFs;
use run::run;

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
    },
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
        Command::Run { config } => match Config::load(&config).and_then(|c| run(&RealFs, &c)) {
            Ok(outcome) if outcome.succeeded() => ExitCode::SUCCESS,
            Ok(_) => ExitCode::from(EXIT_MOVE_FAILED),
            Err(e) => {
                eprintln!("cachy: error: {e:#}");
                ExitCode::from(EXIT_REFUSED)
            }
        },
    }
}
