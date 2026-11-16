//! clap derive definitions only (see `docs/CLI.md`). No command behavior lives
//! here; `main.rs` dispatches on these types.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Translate subtitle files into Burmese with a crash-safe, resumable pipeline.
#[derive(Debug, Parser)]
#[command(name = "myansub", version, about, long_about = None)]
pub struct Cli {
    /// Project directory (overrides project resolution).
    #[arg(short, long, global = true, value_name = "DIR")]
    pub project: Option<PathBuf>,

    /// Extra config file, highest file precedence.
    #[arg(short, long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Raise log level (-v, -vv); overridden by RUST_LOG.
    #[arg(short = 'v', global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Answer yes to cost and destructive-action confirmations.
    #[arg(long, global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Output format for generated subtitles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum OutputFormat {
    Srt,
    Vtt,
    Ass,
    Ssa,
    Sbv,
}

/// Service tier for an LLM request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ServiceTier {
    Flex,
    Standard,
}

/// Which review gate(s) to approve automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum AutoApprove {
    Analysis,
    Translation,
    All,
}

/// Output destination options shared by `init`, `run`, and `export`.
#[derive(Debug, Args)]
pub struct OutputOptions {
    /// Output format. Default: same as input.
    #[arg(long, value_name = "FMT")]
    pub output_format: Option<OutputFormat>,

    /// Output path. Default: `<source-stem>.my.<ext>` next to the source.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the project: decode, parse, mask, store cues and output prefs.
    Init(InitArgs),
    /// Create or resume the project and advance stages.
    Run(RunArgs),
    /// List projects in DIR (default: current directory).
    List(ListArgs),
    /// Show stage states, progress, failures, usage, and estimated cost.
    Status,
    /// Build the analysis context sheet.
    Analyze(AnalyzeArgs),
    /// Inspect and edit the analysis.
    Analysis {
        #[command(subcommand)]
        command: AnalysisCommand,
    },
    /// Translate cues in batches.
    Translate(TranslateArgs),
    /// Review and edit translations.
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
    /// Export the translated subtitle file.
    Export(ExportArgs),
    /// Manage the local response cache.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Source subtitle file.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Source language.
    #[arg(long, value_name = "LANG")]
    pub from: String,

    /// Override input text encoding.
    #[arg(long, value_name = "ENC")]
    pub encoding: Option<String>,

    #[command(flatten)]
    pub output: OutputOptions,

    /// Re-initialize when the source changed (destroys project data).
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Source subtitle file (required when creating the project).
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,

    /// Source language.
    #[arg(long, value_name = "LANG")]
    pub from: Option<String>,

    /// Override input text encoding.
    #[arg(long, value_name = "ENC")]
    pub encoding: Option<String>,

    #[command(flatten)]
    pub output: OutputOptions,

    /// Approve the named gate(s) automatically when reached.
    #[arg(long, value_name = "GATE")]
    pub auto_approve: Option<AutoApprove>,

    /// Permit auto-approving translation and exporting with failed units.
    #[arg(long)]
    pub allow_incomplete: bool,

    /// Run the pipeline without spending tokens or writing results.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Directory to list (default: current directory).
    #[arg(value_name = "DIR")]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct AnalyzeArgs {
    /// Model override.
    #[arg(long, value_name = "M")]
    pub model: Option<String>,

    /// Service tier override.
    #[arg(long, value_name = "T")]
    pub tier: Option<ServiceTier>,

    /// Produce a new analysis version even if a current one exists.
    #[arg(long)]
    pub redo: bool,

    /// Retry only failed units.
    #[arg(long)]
    pub retry_failed: bool,

    /// Run without spending tokens or writing results.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum AnalysisCommand {
    /// Show the current analysis.
    Show {
        /// Emit JSON instead of a formatted view.
        #[arg(long)]
        json: bool,
    },
    /// Edit the analysis in `$EDITOR`.
    Edit,
    /// Export the analysis to a file.
    Export {
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
    /// Import an edited analysis from a file.
    Import {
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
    /// Approve the current analysis version.
    Approve,
}

#[derive(Debug, Args)]
pub struct TranslateArgs {
    /// Model override.
    #[arg(long, value_name = "M")]
    pub model: Option<String>,

    /// Service tier override.
    #[arg(long, value_name = "T")]
    pub tier: Option<ServiceTier>,

    /// Concurrency override.
    #[arg(long, value_name = "N")]
    pub concurrency: Option<u32>,

    /// Re-translate stale, unlocked cues.
    #[arg(long)]
    pub stale: bool,

    /// Re-translate all unlocked cues.
    #[arg(long)]
    pub redo: bool,

    /// Retry only failed batches.
    #[arg(long)]
    pub retry_failed: bool,

    /// Run without spending tokens or writing results.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum ReviewCommand {
    /// List cues matching the given filters.
    List(ReviewListArgs),
    /// Show a single cue.
    Show {
        #[arg(value_name = "ID")]
        id: String,
    },
    /// Set the translation for a cue (locks it).
    Set {
        #[arg(value_name = "ID")]
        id: String,
        #[arg(value_name = "TEXT")]
        text: String,
    },
    /// Unlock a cue.
    Unlock {
        #[arg(value_name = "ID")]
        id: String,
    },
    /// Edit a selection of cues in `$EDITOR`.
    Edit(ReviewSelectionArgs),
    /// Export a selection of cues to a file.
    Export {
        #[arg(value_name = "PATH")]
        path: PathBuf,
        #[command(flatten)]
        selection: ReviewSelectionArgs,
    },
    /// Import edited cues from a file.
    Import {
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
    /// Approve the translation review stage.
    Approve,
}

#[derive(Debug, Args)]
pub struct ReviewListArgs {
    /// Only suspect cues.
    #[arg(long)]
    pub suspect: bool,
    /// Only failed cues.
    #[arg(long)]
    pub failed: bool,
    /// Only stale cues.
    #[arg(long)]
    pub stale: bool,
    /// Only cues carrying the given flag.
    #[arg(long, value_name = "F")]
    pub flag: Option<String>,
    /// Only cues in the id range `A-B`.
    #[arg(long, value_name = "A-B")]
    pub range: Option<String>,
}

/// `--suspect` and `--range` select a subset; they are mutually exclusive.
#[derive(Debug, Args)]
pub struct ReviewSelectionArgs {
    /// Only suspect cues.
    #[arg(long, conflicts_with = "range")]
    pub suspect: bool,
    /// Only cues in the id range `A-B`.
    #[arg(long, value_name = "A-B")]
    pub range: Option<String>,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    #[command(flatten)]
    pub output: OutputOptions,

    /// Export even when some units failed.
    #[arg(long)]
    pub allow_incomplete: bool,
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Delete cache entries.
    Prune {
        /// Only delete entries older than DAYS.
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,
    },
}
