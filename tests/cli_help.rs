// Tests for the CLI surface defined in docs/CLI.md §6.

use clap::CommandFactory;
use myansub::cli::Cli;

/// The clap definition must be internally consistent.
#[test]
fn cli_definition_is_valid() {
    Cli::command().debug_assert();
}

// AC: M0-AC1
#[test]
fn help_lists_every_top_level_command() {
    let help = Cli::command().render_long_help().to_string();
    for command in [
        "init",
        "run",
        "list",
        "status",
        "analyze",
        "analysis",
        "translate",
        "review",
        "export",
        "cache",
    ] {
        assert!(
            help.contains(command),
            "top-level help is missing `{command}`:\n{help}"
        );
    }
}

#[test]
fn binary_name_is_myansub() {
    assert_eq!(Cli::command().get_name(), "myansub");
}

#[test]
fn analysis_has_every_subcommand() {
    let analysis = Cli::command()
        .get_subcommands()
        .find(|c| c.get_name() == "analysis")
        .expect("analysis command exists")
        .clone();
    let subs: Vec<&str> = analysis.get_subcommands().map(|c| c.get_name()).collect();
    for sub in ["show", "edit", "export", "import", "approve"] {
        assert!(subs.contains(&sub), "analysis is missing `{sub}`: {subs:?}");
    }
}

#[test]
fn review_has_every_subcommand() {
    let review = Cli::command()
        .get_subcommands()
        .find(|c| c.get_name() == "review")
        .expect("review command exists")
        .clone();
    let subs: Vec<&str> = review.get_subcommands().map(|c| c.get_name()).collect();
    for sub in [
        "list", "show", "set", "unlock", "edit", "export", "import", "approve",
    ] {
        assert!(subs.contains(&sub), "review is missing `{sub}`: {subs:?}");
    }
}

#[test]
fn cache_has_prune_subcommand() {
    let cache = Cli::command()
        .get_subcommands()
        .find(|c| c.get_name() == "cache")
        .expect("cache command exists")
        .clone();
    let subs: Vec<&str> = cache.get_subcommands().map(|c| c.get_name()).collect();
    assert!(
        subs.contains(&"prune"),
        "cache is missing `prune`: {subs:?}"
    );
}

#[test]
fn global_options_are_present() {
    let cli = Cli::command();
    let args: Vec<&str> = cli.get_arguments().filter_map(|a| a.get_long()).collect();
    for opt in ["project", "config", "yes"] {
        assert!(
            args.contains(&opt),
            "missing global option --{opt}: {args:?}"
        );
    }
}
