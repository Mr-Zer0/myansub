//! Project resolution tests (`docs/CLI.md` §2). Every filesystem case uses a
//! `tempfile` directory; nothing is written into the repo (`docs/TESTING.md` §1).

use std::fs;
use std::path::Path;

use myansub::project::{
    PROJECT_DB_FILE, ProjectError, project_db_path, project_dir_for_source, resolve_project,
};
use tempfile::tempdir;

fn touch_db(dir: &Path) {
    fs::create_dir_all(dir).expect("create project dir");
    fs::write(dir.join(PROJECT_DB_FILE), b"").expect("write project.db");
}

#[test]
fn derives_project_dir_by_appending_suffix_to_full_file_name() {
    let dir = project_dir_for_source(Path::new("/movies/movie.ja.srt"));
    assert_eq!(dir, Path::new("/movies/movie.ja.srt.myansub"));
}

#[test]
fn project_db_path_is_inside_the_project_dir() {
    assert_eq!(
        project_db_path(Path::new("/p/movie.srt.myansub")),
        Path::new("/p/movie.srt.myansub/project.db")
    );
}

// AC: CLI.md §2 case 1 — --project DIR wins over everything else.
#[test]
fn project_flag_wins_over_source_and_cwd() {
    let tmp = tempdir().expect("tempdir");
    let chosen = tmp.path().join("chosen");
    touch_db(&chosen);

    let loc = resolve_project(
        Some(&chosen),
        Some(Path::new("/elsewhere/movie.srt")),
        tmp.path(),
    )
    .expect("resolve with --project");

    assert_eq!(loc.dir, chosen);
    assert!(loc.exists, "project.db present → exists");
}

// AC: CLI.md §2 case 2 — a source FILE resolves to <FILE>.myansub/.
#[test]
fn source_file_resolves_to_sibling_myansub_dir() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.ja.srt");
    fs::write(&source, b"1\n").expect("write source");

    let loc = resolve_project(None, Some(&source), tmp.path()).expect("resolve with source");

    assert_eq!(loc.dir, tmp.path().join("movie.ja.srt.myansub"));
    assert!(!loc.exists, "not yet initialized");
}

// AC: CLI.md §2 case 3a — cwd containing project.db is the project.
#[test]
fn cwd_with_project_db_is_used() {
    let tmp = tempdir().expect("tempdir");
    touch_db(tmp.path());

    let loc = resolve_project(None, None, tmp.path()).expect("resolve from cwd");

    assert_eq!(loc.dir, tmp.path());
    assert!(loc.exists);
}

// AC: CLI.md §2 case 3b — the single *.myansub/ child is chosen.
#[test]
fn single_myansub_child_is_chosen() {
    let tmp = tempdir().expect("tempdir");
    let child = tmp.path().join("movie.srt.myansub");
    touch_db(&child);

    let loc = resolve_project(None, None, tmp.path()).expect("resolve single child");

    assert_eq!(loc.dir, child);
    assert!(loc.exists);
}

// AC: CLI.md §2 case 3 — multiple *.myansub/ children are ambiguous (exit 3),
// and the error lists the candidates.
#[test]
fn multiple_myansub_children_are_ambiguous() {
    let tmp = tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join("a.srt.myansub")).expect("mk a");
    fs::create_dir_all(tmp.path().join("b.srt.myansub")).expect("mk b");

    let err = resolve_project(None, None, tmp.path()).expect_err("ambiguous");

    match err {
        ProjectError::Ambiguous { candidates, .. } => {
            assert!(candidates.iter().any(|c| c == "a.srt.myansub"));
            assert!(candidates.iter().any(|c| c == "b.srt.myansub"));
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

// AC: CLI.md §2 case 3 — no project.db and no *.myansub/ child → NotFound (exit 3).
#[test]
fn empty_cwd_has_no_project() {
    let tmp = tempdir().expect("tempdir");

    let err = resolve_project(None, None, tmp.path()).expect_err("not found");

    assert!(matches!(err, ProjectError::NotFound { .. }));
}
