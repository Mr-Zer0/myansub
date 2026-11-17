//! M2-B repository tests: project/cues round-trips, unit status transitions,
//! one-unit-one-transaction atomicity (M2-AC2), and in-flight recovery (M2-AC3).
//!
//! Follows `docs/TESTING.md`: `tempfile` directories, no network. In-memory
//! stores are used for speed; the reopen-on-same-path recovery test (M2-AC3)
//! requires a file-backed database so state survives the reopen.

use myansub::domain::Cue;
use myansub::store::db::StoreError;
use myansub::store::{Store, repo_cues, repo_project, repo_units};
use repo_project::{Project, Stage, StageStatus};
use repo_units::{NewTranslationBatch, UnitStatus};
use tempfile::tempdir;

fn sample_project() -> Project {
    Project {
        source_path: "/movies/ep1.srt".to_string(),
        source_hash: "sha256:abc".to_string(),
        source_format: "srt".to_string(),
        source_encoding: "utf-8".to_string(),
        encoding_confidence: Some(0.97),
        source_lang: "en".to_string(),
        target_lang: "my".to_string(),
        format_meta: Some(r#"{"script_info":"[Script Info]"}"#.to_string()),
    }
}

fn sample_cues() -> Vec<Cue> {
    vec![
        Cue {
            idx: 1,
            start_ms: 0,
            end_ms: 1500,
            text_raw: "{\\an8}Hello ⟦0⟧".to_string(),
            text_masked: "Hello ⟦0⟧".to_string(),
            lead_tags: Some("{\\an8}".to_string()),
            inline_tags: vec!["<i>".to_string()],
            style: Some("Default".to_string()),
            speaker: Some("c1".to_string()),
            translatable: true,
        },
        Cue {
            idx: 2,
            start_ms: 1600,
            end_ms: 2000,
            text_raw: "...".to_string(),
            text_masked: "...".to_string(),
            lead_tags: None,
            inline_tags: vec![],
            style: None,
            speaker: None,
            translatable: false,
        },
        Cue {
            idx: 3,
            start_ms: 2100,
            end_ms: 4000,
            text_raw: "Two ⟦0⟧ tags ⟦1⟧".to_string(),
            text_masked: "Two ⟦0⟧ tags ⟦1⟧".to_string(),
            lead_tags: None,
            inline_tags: vec!["<b>".to_string(), "</b>".to_string()],
            style: None,
            speaker: None,
            translatable: true,
        },
    ]
}

// --- repo_project ---------------------------------------------------------

#[tokio::test]
async fn project_round_trips_through_insert_and_get() {
    let store = Store::open_in_memory().expect("open");
    let project = sample_project();
    let p = project.clone();
    store
        .call(move |conn| repo_project::insert_project(conn, &p))
        .await
        .expect("insert project");

    let loaded = store
        .call(|conn| repo_project::get_project(conn))
        .await
        .expect("get project")
        .expect("project present");
    assert_eq!(loaded, project);
}

// minor #4 — round-trip with the nullable columns absent, to catch a NULL-column
// mapping regression (the Some(..) case is covered above).
#[tokio::test]
async fn project_round_trips_with_null_columns() {
    let store = Store::open_in_memory().expect("open");
    let project = Project {
        source_path: "/movies/ep2.vtt".to_string(),
        source_hash: "sha256:def".to_string(),
        source_format: "vtt".to_string(),
        source_encoding: "utf-8".to_string(),
        encoding_confidence: None,
        source_lang: "ja".to_string(),
        target_lang: "my".to_string(),
        format_meta: None,
    };
    let p = project.clone();
    store
        .call(move |conn| repo_project::insert_project(conn, &p))
        .await
        .expect("insert project");

    let loaded = store
        .call(|conn| repo_project::get_project(conn))
        .await
        .expect("get project")
        .expect("project present");
    assert_eq!(loaded, project);
    assert_eq!(loaded.encoding_confidence, None);
    assert_eq!(loaded.format_meta, None);
}

#[tokio::test]
async fn get_project_is_none_on_empty_database() {
    let store = Store::open_in_memory().expect("open");
    let loaded = store
        .call(|conn| repo_project::get_project(conn))
        .await
        .expect("get");
    assert!(loaded.is_none());
}

#[tokio::test]
async fn stage_state_sets_reads_and_upserts() {
    let store = Store::open_in_memory().expect("open");

    // Unknown stage reads as None.
    let before = store
        .call(|conn| repo_project::get_stage(conn, Stage::Analysis))
        .await
        .expect("get stage");
    assert_eq!(before, None);

    store
        .call(|conn| repo_project::set_stage(conn, Stage::Analysis, StageStatus::Running))
        .await
        .expect("set running");
    let running = store
        .call(|conn| repo_project::get_stage(conn, Stage::Analysis))
        .await
        .expect("get stage");
    assert_eq!(running, Some(StageStatus::Running));

    // Upsert overwrites the same stage's status.
    store
        .call(|conn| repo_project::set_stage(conn, Stage::Analysis, StageStatus::Completed))
        .await
        .expect("set completed");
    let completed = store
        .call(|conn| repo_project::get_stage(conn, Stage::Analysis))
        .await
        .expect("get stage");
    assert_eq!(completed, Some(StageStatus::Completed));
}

// --- repo_cues ------------------------------------------------------------

#[tokio::test]
async fn cues_round_trip_preserves_all_fields() {
    let store = Store::open_in_memory().expect("open");
    let cues = sample_cues();
    let c = cues.clone();
    store
        .call(move |conn| repo_cues::insert_cues(conn, &c))
        .await
        .expect("insert cues");

    let loaded = store
        .call(|conn| repo_cues::load_cues(conn))
        .await
        .expect("load cues");
    assert_eq!(loaded, cues);
}

#[tokio::test]
async fn cues_load_is_ordered_by_idx() {
    let store = Store::open_in_memory().expect("open");
    let mut cues = sample_cues();
    cues.reverse(); // insert out of order
    store
        .call(move |conn| repo_cues::insert_cues(conn, &cues))
        .await
        .expect("insert cues");

    let loaded = store
        .call(|conn| repo_cues::load_cues(conn))
        .await
        .expect("load cues");
    let idxs: Vec<u32> = loaded.iter().map(|c| c.idx).collect();
    assert_eq!(idxs, vec![1, 2, 3]);
}

// --- repo_units: transitions ---------------------------------------------

#[tokio::test]
async fn claim_moves_pending_to_in_flight_and_increments_attempts() {
    let store = Store::open_in_memory().expect("open");
    store
        .call(|conn| repo_units::create_analysis_chunk(conn, 1, 1, 50))
        .await
        .expect("create chunk");

    let before = store
        .call(|conn| repo_units::get_analysis_chunk(conn, 1))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(before.status, UnitStatus::Pending);
    assert_eq!(before.attempts, 0);

    let claimed = store
        .call(|conn| repo_units::claim_analysis_chunk(conn, 1))
        .await
        .expect("claim");
    assert!(claimed, "claiming a pending unit must report the win");

    let after = store
        .call(|conn| repo_units::get_analysis_chunk(conn, 1))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(after.status, UnitStatus::InFlight);
    assert_eq!(after.attempts, 1);
}

// MAJOR #1 — claiming a unit that is not pending (already in_flight) reports the
// loss (`false`) and does not change status or bump attempts. The M4 scheduler
// relies on this to avoid dispatching — and paying for — a unit twice.
#[tokio::test]
async fn claim_on_non_pending_unit_reports_not_claimed() {
    let store = Store::open_in_memory().expect("open");
    store
        .call(|conn| repo_units::create_analysis_chunk(conn, 1, 1, 50))
        .await
        .expect("create chunk");
    assert!(
        store
            .call(|conn| repo_units::claim_analysis_chunk(conn, 1))
            .await
            .expect("first claim"),
        "first claim wins"
    );

    let second = store
        .call(|conn| repo_units::claim_analysis_chunk(conn, 1))
        .await
        .expect("second claim");
    assert!(!second, "a second claim on an in_flight unit must lose");

    let unit = store
        .call(|conn| repo_units::get_analysis_chunk(conn, 1))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(unit.status, UnitStatus::InFlight);
    assert_eq!(unit.attempts, 1, "a lost claim must not bump attempts");
}

// MAJOR #1 — completing a unit that is not in_flight (here already done) reports
// the no-op (`false`) and writes no (new) result.
#[tokio::test]
async fn complete_on_non_in_flight_unit_reports_noop() {
    let store = Store::open_in_memory().expect("open");
    store
        .call(|conn| repo_units::create_analysis_chunk(conn, 1, 1, 50))
        .await
        .expect("create chunk");
    // Still pending (never claimed): complete must be a no-op.
    let changed = store
        .call(|conn| repo_units::complete_analysis_chunk(conn, 1, r#"{"premise":"x"}"#))
        .await
        .expect("complete");
    assert!(!changed, "completing a non-in_flight unit must be a no-op");

    let unit = store
        .call(|conn| repo_units::get_analysis_chunk(conn, 1))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(unit.status, UnitStatus::Pending);
    assert_eq!(unit.result, None, "no result may be written on a no-op");
}

// minor #2 — failing a unit that is already done is a no-op: its status and
// result are preserved, so a rerun never redoes paid work.
#[tokio::test]
async fn fail_on_done_unit_is_noop() {
    let store = Store::open_in_memory().expect("open");
    store
        .call(|conn| repo_units::create_analysis_chunk(conn, 1, 1, 50))
        .await
        .expect("create chunk");
    store
        .call(|conn| repo_units::claim_analysis_chunk(conn, 1))
        .await
        .expect("claim");
    store
        .call(|conn| repo_units::complete_analysis_chunk(conn, 1, r#"{"premise":"x"}"#))
        .await
        .expect("complete");

    let changed = store
        .call(|conn| repo_units::fail_analysis_chunk(conn, 1, "boom"))
        .await
        .expect("fail");
    assert!(!changed, "failing a done unit must be a no-op");

    let unit = store
        .call(|conn| repo_units::get_analysis_chunk(conn, 1))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(unit.status, UnitStatus::Done, "done must be preserved");
    assert_eq!(
        unit.result.as_deref(),
        Some(r#"{"premise":"x"}"#),
        "a no-op fail must not clobber the result"
    );
    assert_eq!(unit.last_error, None);
}

// AC: M2-AC2 — a successful complete writes the result AND flips status to `done`
// atomically (one unit, one transaction; `docs/ARCHITECTURE.md` §6 rule 1).
#[tokio::test]
async fn complete_writes_result_and_done_status_atomically() {
    let store = Store::open_in_memory().expect("open");
    store
        .call(|conn| repo_units::create_analysis_chunk(conn, 1, 1, 50))
        .await
        .expect("create chunk");
    store
        .call(|conn| repo_units::claim_analysis_chunk(conn, 1))
        .await
        .expect("claim");

    store
        .call(|conn| repo_units::complete_analysis_chunk(conn, 1, r#"{"premise":"x"}"#))
        .await
        .expect("complete");

    let done = store
        .call(|conn| repo_units::get_analysis_chunk(conn, 1))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(done.status, UnitStatus::Done);
    assert_eq!(done.result.as_deref(), Some(r#"{"premise":"x"}"#));
}

// AC: M2-AC2 — a crash mid-transaction (error after the result UPDATE, before
// commit) rolls back entirely: the unit stays `in_flight` and no result persists.
// There is no partial unit (`docs/TESTING.md` §4).
#[tokio::test]
async fn failed_complete_transaction_rolls_back_leaving_no_partial_unit() {
    let store = Store::open_in_memory().expect("open");
    store
        .call(|conn| repo_units::create_analysis_chunk(conn, 1, 1, 50))
        .await
        .expect("create chunk");
    store
        .call(|conn| repo_units::claim_analysis_chunk(conn, 1))
        .await
        .expect("claim");

    let result = store
        .call(|conn| {
            let tx = conn.transaction()?;
            // Write the result and flip to `done` inside the transaction...
            repo_units::complete_analysis_chunk_in_tx(&tx, 1, r#"{"premise":"x"}"#)?;
            // ...then a deliberate constraint violation fails the transaction
            // before commit (duplicate primary key on chunk_no = 1).
            tx.execute(
                "INSERT INTO analysis_chunks (chunk_no, first_cue, last_cue, status) \
                 VALUES (1, 1, 1, 'pending')",
                [],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await;
    assert!(
        result.is_err(),
        "the injected failure must abort the commit"
    );

    let unit = store
        .call(|conn| repo_units::get_analysis_chunk(conn, 1))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(
        unit.status,
        UnitStatus::InFlight,
        "status must still be in_flight after rollback"
    );
    assert_eq!(
        unit.result, None,
        "no result may be persisted after rollback"
    );
}

#[tokio::test]
async fn fail_moves_in_flight_to_failed_with_last_error() {
    let store = Store::open_in_memory().expect("open");
    store
        .call(|conn| repo_units::create_analysis_chunk(conn, 1, 1, 50))
        .await
        .expect("create chunk");
    store
        .call(|conn| repo_units::claim_analysis_chunk(conn, 1))
        .await
        .expect("claim");

    store
        .call(|conn| repo_units::fail_analysis_chunk(conn, 1, "boom"))
        .await
        .expect("fail");

    let unit = store
        .call(|conn| repo_units::get_analysis_chunk(conn, 1))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(unit.status, UnitStatus::Failed);
    assert_eq!(unit.last_error.as_deref(), Some("boom"));
}

#[tokio::test]
async fn translation_batch_lifecycle_create_claim_complete() {
    let store = Store::open_in_memory().expect("open");
    let batch = NewTranslationBatch {
        batch_no: 1,
        first_cue: 1,
        last_cue: 40,
        input_hash: Some("hash1".to_string()),
        model: Some("gemini".to_string()),
        analysis_version: Some(2),
    };
    store
        .call(move |conn| repo_units::create_translation_batch(conn, &batch))
        .await
        .expect("create batch");

    let created = store
        .call(|conn| repo_units::get_translation_batch(conn, 1))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(created.status, UnitStatus::Pending);
    assert_eq!(created.input_hash.as_deref(), Some("hash1"));
    assert_eq!(created.model.as_deref(), Some("gemini"));
    assert_eq!(created.analysis_version, Some(2));

    store
        .call(|conn| repo_units::claim_translation_batch(conn, 1))
        .await
        .expect("claim");
    store
        .call(|conn| repo_units::complete_translation_batch(conn, 1))
        .await
        .expect("complete");

    let done = store
        .call(|conn| repo_units::get_translation_batch(conn, 1))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(done.status, UnitStatus::Done);
    assert_eq!(done.attempts, 1);
}

#[tokio::test]
async fn list_and_count_by_status() {
    let store = Store::open_in_memory().expect("open");
    store
        .call(|conn| {
            repo_units::create_analysis_chunk(conn, 1, 1, 10)?;
            repo_units::create_analysis_chunk(conn, 2, 11, 20)?;
            repo_units::create_analysis_chunk(conn, 3, 21, 30)
        })
        .await
        .expect("create chunks");
    store
        .call(|conn| repo_units::claim_analysis_chunk(conn, 2))
        .await
        .expect("claim");

    let pending = store
        .call(|conn| repo_units::list_analysis_chunks_by_status(conn, UnitStatus::Pending))
        .await
        .expect("list pending");
    let pending_nos: Vec<i64> = pending.iter().map(|c| c.chunk_no).collect();
    assert_eq!(pending_nos, vec![1, 3]);

    let in_flight_count = store
        .call(|conn| repo_units::count_analysis_chunks_by_status(conn, UnitStatus::InFlight))
        .await
        .expect("count in_flight");
    assert_eq!(in_flight_count, 1);
}

// --- repo_units: recovery (M2-AC3) ---------------------------------------

// AC: M2-AC3 — a direct recover() resets in_flight units in BOTH tables to
// pending in one transaction, and reports how many it reset.
#[tokio::test]
async fn recover_resets_in_flight_to_pending_in_both_tables() {
    let store = Store::open_in_memory().expect("open");
    store
        .call(|conn| {
            repo_units::create_analysis_chunk(conn, 1, 1, 10)?;
            repo_units::create_translation_batch(
                conn,
                &NewTranslationBatch {
                    batch_no: 1,
                    first_cue: 1,
                    last_cue: 10,
                    input_hash: None,
                    model: None,
                    analysis_version: None,
                },
            )
        })
        .await
        .expect("create units");
    store
        .call(|conn| repo_units::claim_analysis_chunk(conn, 1))
        .await
        .expect("claim chunk");
    store
        .call(|conn| repo_units::claim_translation_batch(conn, 1))
        .await
        .expect("claim batch");

    let reset = store.recover().await.expect("recover");
    assert_eq!(reset, 2, "both in_flight units must be reset");

    let chunk = store
        .call(|conn| repo_units::get_analysis_chunk(conn, 1))
        .await
        .expect("get chunk")
        .expect("present");
    let batch = store
        .call(|conn| repo_units::get_translation_batch(conn, 1))
        .await
        .expect("get batch")
        .expect("present");
    assert_eq!(chunk.status, UnitStatus::Pending);
    assert_eq!(batch.status, UnitStatus::Pending);
}

// AC: M2-AC3 — reopening the project (Store::open on the same file) resets any
// unit left in_flight by a crashed run back to pending. Requires a file-backed
// database so state survives the reopen.
#[tokio::test]
async fn reopen_resets_in_flight_units_to_pending() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("project.db");

    {
        let store = Store::open(&path).expect("open");
        store
            .call(|conn| {
                repo_units::create_analysis_chunk(conn, 1, 1, 10)?;
                repo_units::create_translation_batch(
                    conn,
                    &NewTranslationBatch {
                        batch_no: 1,
                        first_cue: 1,
                        last_cue: 10,
                        input_hash: None,
                        model: None,
                        analysis_version: None,
                    },
                )
            })
            .await
            .expect("create units");
        store
            .call(|conn| repo_units::claim_analysis_chunk(conn, 1))
            .await
            .expect("claim chunk");
        store
            .call(|conn| repo_units::claim_translation_batch(conn, 1))
            .await
            .expect("claim batch");
        // Simulate a crash: drop the store without completing the units.
    }

    let reopened = Store::open(&path).expect("reopen");
    let chunk = reopened
        .call(|conn| repo_units::get_analysis_chunk(conn, 1))
        .await
        .expect("get chunk")
        .expect("present");
    let batch = reopened
        .call(|conn| repo_units::get_translation_batch(conn, 1))
        .await
        .expect("get batch")
        .expect("present");
    assert_eq!(
        chunk.status,
        UnitStatus::Pending,
        "in_flight chunk must reset on reopen"
    );
    assert_eq!(
        batch.status,
        UnitStatus::Pending,
        "in_flight batch must reset on reopen"
    );
}

#[tokio::test]
async fn recover_is_noop_on_fresh_database() {
    let store = Store::open_in_memory().expect("open");
    let reset = store.recover().await.expect("recover");
    assert_eq!(reset, 0);
    // A bogus status that is not a recognised label surfaces as an error.
    let err = UnitStatus::from_db("weird");
    assert!(matches!(err, Err(StoreError::Unexpected(_))));
}

// --- repo_units: translation-batch requeues (M6-B2) ----------------------

/// A pending translation batch with the given `batch_no`.
fn new_batch(batch_no: i64) -> NewTranslationBatch {
    NewTranslationBatch {
        batch_no,
        first_cue: batch_no,
        last_cue: batch_no,
        input_hash: None,
        model: None,
        analysis_version: None,
    }
}

async fn batch_status(store: &Store, batch_no: i64) -> UnitStatus {
    store
        .call(move |conn| repo_units::get_translation_batch(conn, batch_no))
        .await
        .expect("get")
        .expect("present")
        .status
}

// AC: M6-B2 — requeue_failed_translation_batches resets only `failed` batches to
// `pending`, leaving `in_flight`, `pending`, and `done` batches untouched (so a
// `--retry-failed` run reruns exactly the failures).
#[tokio::test]
async fn requeue_failed_batches_resets_only_failed() {
    let store = Store::open_in_memory().expect("open");
    store
        .call(|conn| {
            for no in 1..=4 {
                repo_units::create_translation_batch(conn, &new_batch(no))?;
            }
            Ok(())
        })
        .await
        .expect("create");

    // batch 1 → failed, batch 2 → done, batch 3 → in_flight, batch 4 → pending.
    store
        .call(|conn| {
            repo_units::claim_translation_batch(conn, 1)?;
            repo_units::fail_translation_batch(conn, 1, "boom")?;
            repo_units::claim_translation_batch(conn, 2)?;
            repo_units::complete_translation_batch(conn, 2)?;
            repo_units::claim_translation_batch(conn, 3)?; // left in_flight
            Ok(())
        })
        .await
        .expect("arrange");

    let requeued = store
        .call(|conn| repo_units::requeue_failed_translation_batches(conn))
        .await
        .expect("requeue");
    assert_eq!(requeued, 1, "only the single failed batch is requeued");

    assert_eq!(
        batch_status(&store, 1).await,
        UnitStatus::Pending,
        "failed → pending"
    );
    assert_eq!(
        batch_status(&store, 2).await,
        UnitStatus::Done,
        "done untouched"
    );
    assert_eq!(
        batch_status(&store, 3).await,
        UnitStatus::InFlight,
        "in_flight untouched"
    );
    assert_eq!(
        batch_status(&store, 4).await,
        UnitStatus::Pending,
        "already-pending untouched"
    );
}

// AC: M6-B2 — requeue_translation_batch resets a single `done` or `failed` batch to
// `pending` and reports it, but leaves an `in_flight` or already-`pending` batch
// untouched (the `--redo`/`--stale` per-batch requeue seam).
#[tokio::test]
async fn requeue_one_batch_resets_only_done_or_failed() {
    let store = Store::open_in_memory().expect("open");
    store
        .call(|conn| {
            for no in 1..=4 {
                repo_units::create_translation_batch(conn, &new_batch(no))?;
            }
            Ok(())
        })
        .await
        .expect("create");

    // batch 1 → done, batch 2 → failed, batch 3 → in_flight, batch 4 → pending.
    store
        .call(|conn| {
            repo_units::claim_translation_batch(conn, 1)?;
            repo_units::complete_translation_batch(conn, 1)?;
            repo_units::claim_translation_batch(conn, 2)?;
            repo_units::fail_translation_batch(conn, 2, "boom")?;
            repo_units::claim_translation_batch(conn, 3)?; // left in_flight
            Ok(())
        })
        .await
        .expect("arrange");

    // A `done` batch is requeued.
    let done_requeued = store
        .call(|conn| repo_units::requeue_translation_batch(conn, 1))
        .await
        .expect("requeue done");
    assert!(done_requeued, "a done batch is requeued");
    assert_eq!(batch_status(&store, 1).await, UnitStatus::Pending);

    // A `failed` batch is requeued.
    let failed_requeued = store
        .call(|conn| repo_units::requeue_translation_batch(conn, 2))
        .await
        .expect("requeue failed");
    assert!(failed_requeued, "a failed batch is requeued");
    assert_eq!(batch_status(&store, 2).await, UnitStatus::Pending);

    // An `in_flight` batch is left untouched.
    let in_flight_requeued = store
        .call(|conn| repo_units::requeue_translation_batch(conn, 3))
        .await
        .expect("requeue in_flight");
    assert!(!in_flight_requeued, "an in_flight batch is not requeued");
    assert_eq!(batch_status(&store, 3).await, UnitStatus::InFlight);

    // An already-`pending` batch is left untouched.
    let pending_requeued = store
        .call(|conn| repo_units::requeue_translation_batch(conn, 4))
        .await
        .expect("requeue pending");
    assert!(!pending_requeued, "a pending batch is not requeued");
    assert_eq!(batch_status(&store, 4).await, UnitStatus::Pending);
}
