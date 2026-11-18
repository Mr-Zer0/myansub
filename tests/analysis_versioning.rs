//! M5 analysis versioning + provenance repository tests (`docs/DATA_MODEL.md`
//! §2/§3). In-memory stores, no network (`docs/TESTING.md` §1).

use myansub::store::Store;
use myansub::store::repo_analysis::{
    AnalysisOrigin, approve_current_analysis, current_analysis, get_analysis_version,
    insert_analysis_version, list_analysis_versions,
};

const DOC_V1: &str = r#"{"schema_version":1,"premise":"p1","characters":[],"scenes":[]}"#;
const DOC_V2: &str = r#"{"schema_version":1,"premise":"p2","characters":[],"scenes":[]}"#;

// AC: M5 — insert → get round-trips the document and provenance; the first version
// is 1 and is unapproved.
#[tokio::test]
async fn insert_then_get_round_trips_provenance() {
    let store = Store::open_in_memory().expect("open");
    let (version, fetched) = store
        .call(|conn| {
            let version = insert_analysis_version(
                conn,
                DOC_V1,
                AnalysisOrigin::Llm,
                "sha256:abc",
                "prompt_v1|gemini-flash-latest",
            )?;
            let fetched = get_analysis_version(conn, version)?;
            Ok((version, fetched))
        })
        .await
        .expect("insert+get");

    assert_eq!(version, 1);
    let v = fetched.expect("version row present");
    assert_eq!(v.version, 1);
    assert_eq!(v.doc, DOC_V1);
    assert_eq!(v.origin, AnalysisOrigin::Llm);
    assert_eq!(v.source_digest, "sha256:abc");
    assert_eq!(v.signature, "prompt_v1|gemini-flash-latest");
    assert!(!v.approved, "a fresh version is unapproved");
}

// AC: M5 — a second insert gets version 2; the current analysis is the highest
// version.
#[tokio::test]
async fn second_insert_is_version_two_and_current() {
    let store = Store::open_in_memory().expect("open");
    let (v1, v2, current) = store
        .call(|conn| {
            let v1 =
                insert_analysis_version(conn, DOC_V1, AnalysisOrigin::Llm, "d", "prompt_v1|m")?;
            let v2 =
                insert_analysis_version(conn, DOC_V2, AnalysisOrigin::Llm, "d", "prompt_v1|m")?;
            let current = current_analysis(conn)?;
            Ok((v1, v2, current))
        })
        .await
        .expect("inserts");

    assert_eq!(v1, 1);
    assert_eq!(v2, 2);
    let current = current.expect("current present");
    assert_eq!(current.version, 2);
    assert!(!current.approved);
}

// AC: M5 — approving marks the highest version approved and leaves older versions
// untouched.
#[tokio::test]
async fn approve_current_marks_highest_version_only() {
    let store = Store::open_in_memory().expect("open");
    let (approved_flag, v1, v2) = store
        .call(|conn| {
            insert_analysis_version(conn, DOC_V1, AnalysisOrigin::Llm, "d", "prompt_v1|m")?;
            insert_analysis_version(conn, DOC_V2, AnalysisOrigin::Llm, "d", "prompt_v1|m")?;
            let changed = approve_current_analysis(conn)?;
            let v1 = get_analysis_version(conn, 1)?.expect("v1");
            let v2 = get_analysis_version(conn, 2)?.expect("v2");
            Ok((changed, v1, v2))
        })
        .await
        .expect("approve");

    assert!(approved_flag);
    assert!(!v1.approved, "older version stays unapproved");
    assert!(v2.approved, "the current version is approved");
}

// AC: M5 — approve on an empty table is a harmless no-op (returns false).
#[tokio::test]
async fn approve_with_no_versions_is_a_no_op() {
    let store = Store::open_in_memory().expect("open");
    let changed = store.call(approve_current_analysis).await.expect("approve");
    assert!(!changed);
}

// AC: M5 — listing returns versions in ascending order.
#[tokio::test]
async fn list_returns_versions_in_order() {
    let store = Store::open_in_memory().expect("open");
    let versions = store
        .call(|conn| {
            insert_analysis_version(conn, DOC_V1, AnalysisOrigin::Llm, "d", "prompt_v1|m")?;
            insert_analysis_version(conn, DOC_V2, AnalysisOrigin::User, "d", "prompt_v1|m")?;
            list_analysis_versions(conn)
        })
        .await
        .expect("list");

    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].version, 1);
    assert_eq!(versions[0].origin, AnalysisOrigin::Llm);
    assert_eq!(versions[1].version, 2);
    assert_eq!(versions[1].origin, AnalysisOrigin::User);
}
