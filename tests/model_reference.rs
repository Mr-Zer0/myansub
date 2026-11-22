//! Binary-level model-reference resolution (M13-AC1, `docs/LLM.md` §10.1).
//!
//! A model reference with an unknown `provider:` prefix is a usage error (exit 2),
//! the same class as a clap/flag error. Offline: `--dry-run` needs no API key.

use tempfile::tempdir;

const SRT: &str = "1\n00:00:01,000 --> 00:00:02,000\nHello world\n";

fn bin() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_myansub"))
}

// AC: M13-AC1 — `--model openai:…` names an unknown provider; the command exits 2
// (usage error) before any token is spent. A `--dry-run` needs no key, so this is
// a pure resolution check.
#[test]
fn unknown_provider_in_model_flag_exits_2() {
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("movie.en.srt");
    std::fs::write(&source, SRT).expect("write source");

    let created = bin()
        .current_dir(dir.path())
        .args(["init", source.to_str().expect("utf8 path"), "--from", "en"])
        .status()
        .expect("spawn init");
    assert!(created.success(), "init should succeed");

    let status = bin()
        .current_dir(dir.path())
        .args(["analyze", "--model", "openai:gpt-4", "--dry-run"])
        .status()
        .expect("spawn analyze");
    assert_eq!(
        status.code(),
        Some(2),
        "an unknown provider prefix must be a usage error"
    );
}

// AC: M13-AC1 — a recognised provider prefix (and an unprefixed model) are accepted;
// the dry-run estimate runs to success (exit 0).
#[test]
fn known_provider_prefix_is_accepted() {
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("movie.en.srt");
    std::fs::write(&source, SRT).expect("write source");

    let created = bin()
        .current_dir(dir.path())
        .args(["init", source.to_str().expect("utf8 path"), "--from", "en"])
        .status()
        .expect("spawn init");
    assert!(created.success(), "init should succeed");

    let status = bin()
        .current_dir(dir.path())
        .args([
            "analyze",
            "--model",
            "openrouter:anthropic/claude-sonnet-5.5",
            "--dry-run",
        ])
        .status()
        .expect("spawn analyze");
    assert_eq!(
        status.code(),
        Some(0),
        "a known provider prefix must be accepted"
    );
}
