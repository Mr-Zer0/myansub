//! `myansub models list` (M13-AC16), offline: the Gemini listing comes from the
//! `[pricing]` config table — no network, no project required.

use std::io::Write;
use tempfile::tempdir;

fn bin() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_myansub"))
}

const CONFIG: &str = r#"
[pricing."gemini-flash-latest"]
input = 0.3
cached_input = 0.075
output = 2.5
storage_per_hour = 1.0
flex_multiplier = 0.5
"#;

// AC: M13-AC16 — `models list --provider gemini` prints the priced Gemini models
// with id, input/output price per 1M, context length, and structured-output support.
#[test]
fn models_list_gemini_from_price_table() {
    let dir = tempdir().expect("tempdir");
    let mut cfg = std::fs::File::create(dir.path().join("myansub.toml")).expect("create config");
    cfg.write_all(CONFIG.as_bytes()).expect("write config");

    let output = bin()
        .current_dir(dir.path())
        .args(["models", "list", "--provider", "gemini"])
        .output()
        .expect("spawn models list");

    assert!(output.status.success(), "models list should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("gemini:gemini-flash-latest"),
        "should list the gemini model: {stdout}"
    );
    assert!(stdout.contains("structured yes"), "{stdout}");
    assert!(stdout.contains("in $0.3000/1M"), "{stdout}");
}

// AC: M13-AC16 — the positional FILTER restricts the listing by id substring.
#[test]
fn models_list_filter_excludes_non_matching() {
    let dir = tempdir().expect("tempdir");
    let mut cfg = std::fs::File::create(dir.path().join("myansub.toml")).expect("create config");
    cfg.write_all(CONFIG.as_bytes()).expect("write config");

    let output = bin()
        .current_dir(dir.path())
        .args(["models", "list", "--provider", "gemini", "nonexistent-xyz"])
        .output()
        .expect("spawn models list");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("gemini-flash-latest"),
        "filter should exclude non-matching ids: {stdout}"
    );
}
