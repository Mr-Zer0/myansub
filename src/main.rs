//! Binary entry point: `.env` loading, tracing init, clap dispatch, and
//! exit-code mapping (see `docs/CLI.md` §7). `anyhow` is permitted only here.

use anyhow::bail;
use clap::Parser;
use myansub::cli::{Cli, Command};
use myansub::error::exit;
use tracing_subscriber::EnvFilter;

fn main() {
    // Missing `.env` is not an error (docs/CONFIG.md §1).
    let _ = dotenvy::dotenv();

    // clap handles `--help` (exit 0) and usage errors (exit 2) itself.
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let code = match dispatch(&cli) {
        Ok(()) => exit::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            exit::ERROR
        }
    };
    std::process::exit(code);
}

/// Initializes tracing. `RUST_LOG` wins; otherwise `-v`/`-vv` raise the level
/// above the default `info` (docs/ARCHITECTURE.md §9).
fn init_tracing(verbose: u8) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = match verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        };
        EnvFilter::new(format!("myansub={level}"))
    });
    // A failure here means a global subscriber was already set; ignore it.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Dispatches a parsed command. Every command is unimplemented in M0 and maps
/// to a generic error (exit 1).
fn dispatch(cli: &Cli) -> anyhow::Result<()> {
    let name = command_name(&cli.command);
    not_implemented(name)
}

fn not_implemented(command: &str) -> anyhow::Result<()> {
    bail!("`{command}` is not implemented yet")
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Init(_) => "init",
        Command::Run(_) => "run",
        Command::List(_) => "list",
        Command::Status => "status",
        Command::Analyze(_) => "analyze",
        Command::Analysis { .. } => "analysis",
        Command::Translate(_) => "translate",
        Command::Review { .. } => "review",
        Command::Export(_) => "export",
        Command::Cache { .. } => "cache",
    }
}
