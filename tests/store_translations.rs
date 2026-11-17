//! M6-B1 repository tests for `translations` / `translation_history`
//! (`docs/DATA_MODEL.md` §2/§5/§6). In-memory stores, no network.

use myansub::domain::{Cue, QualityFlag};
use myansub::store::Store;
use myansub::store::repo_cues::insert_cues;
use myansub::store::repo_translations::{
    NewTranslation, TranslationOrigin, cue_id_by_idx, get_translation, list_translation_history,
    list_translations, upsert_translation,
};

fn cue(idx: u32, masked: &str, translatable: bool) -> Cue {
    Cue {
        idx,
        start_ms: u64::from(idx) * 1000,
        end_ms: u64::from(idx) * 1000 + 900,
        text_raw: masked.to_string(),
        text_masked: masked.to_string(),
        lead_tags: None,
        inline_tags: vec![],
        style: None,
        speaker: None,
        translatable,
    }
}

async fn store_with_cues() -> Store {
    let store = Store::open_in_memory().expect("open");
    store
        .call(|conn| {
            insert_cues(
                conn,
                &[cue(1, "Hello.", true), cue(2, "Where are you going?", true)],
            )
        })
        .await
        .expect("insert cues");
    store
}

async fn cue_id(store: &Store, idx: u32) -> i64 {
    store
        .call(move |c| Ok(cue_id_by_idx(c)?.get(&idx).copied()))
        .await
        .expect("map")
        .expect("cue present")
}

// AC: M6-B1 — an upserted translation round-trips: text, origin, flags,
// analysis_version, and model are all read back unchanged.
#[tokio::test]
async fn upsert_then_load_round_trips() {
    let store = store_with_cues().await;
    let id = cue_id(&store, 1).await;

    let t = NewTranslation {
        cue_id: id,
        text: "မင်္ဂလာပါ".to_string(),
        origin: TranslationOrigin::Llm,
        locked: false,
        stale: false,
        flags: vec![QualityFlag::TooFast],
        analysis_version: 3,
        model: Some("gemini-flash-latest".to_string()),
    };
    store
        .call(move |c| upsert_translation(c, &t))
        .await
        .expect("upsert");

    let row = store
        .call(move |c| get_translation(c, id))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.text, "မင်္ဂလာပါ");
    assert_eq!(row.origin, TranslationOrigin::Llm);
    assert_eq!(row.flags, vec![QualityFlag::TooFast]);
    assert_eq!(row.analysis_version, 3);
    assert_eq!(row.model.as_deref(), Some("gemini-flash-latest"));
    assert!(!row.locked);
}

// AC: M6-B1 — changing an existing translation's text appends the previous text to
// translation_history (`docs/DATA_MODEL.md` §6); an unchanged re-upsert does not.
#[tokio::test]
async fn history_is_appended_only_when_text_changes() {
    let store = store_with_cues().await;
    let id = cue_id(&store, 1).await;

    let base = NewTranslation {
        cue_id: id,
        text: "first".to_string(),
        origin: TranslationOrigin::Llm,
        locked: false,
        stale: false,
        flags: vec![],
        analysis_version: 1,
        model: None,
    };
    store
        .call({
            let base = base.clone();
            move |c| upsert_translation(c, &base)
        })
        .await
        .expect("insert");

    // Re-upsert identical text: no history.
    store
        .call({
            let base = base.clone();
            move |c| upsert_translation(c, &base)
        })
        .await
        .expect("noop upsert");
    let hist = store
        .call(move |c| list_translation_history(c, id))
        .await
        .expect("history");
    assert!(hist.is_empty(), "unchanged text appends no history");

    // Change the text: the previous text is recorded.
    let changed = NewTranslation {
        text: "second".to_string(),
        ..base
    };
    store
        .call(move |c| upsert_translation(c, &changed))
        .await
        .expect("change");
    let hist = store
        .call(move |c| list_translation_history(c, id))
        .await
        .expect("history");
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].text, "first", "history holds the superseded text");

    let row = store
        .call(move |c| get_translation(c, id))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.text, "second", "the row now holds the new text");
}

// AC: M6-B1 — a locked translation is never overwritten (`docs/DATA_MODEL.md` §2);
// the upsert reports it did not write and the stored text is unchanged.
#[tokio::test]
async fn locked_row_is_not_overwritten() {
    let store = store_with_cues().await;
    let id = cue_id(&store, 1).await;

    let locked = NewTranslation {
        cue_id: id,
        text: "reviewer text".to_string(),
        origin: TranslationOrigin::User,
        locked: true,
        stale: false,
        flags: vec![],
        analysis_version: 1,
        model: None,
    };
    store
        .call(move |c| upsert_translation(c, &locked))
        .await
        .expect("insert locked");

    let attempt = NewTranslation {
        cue_id: id,
        text: "llm override".to_string(),
        origin: TranslationOrigin::Llm,
        locked: false,
        stale: false,
        flags: vec![],
        analysis_version: 2,
        model: Some("m".to_string()),
    };
    let written = store
        .call(move |c| upsert_translation(c, &attempt))
        .await
        .expect("upsert attempt");
    assert!(!written, "a locked row blocks the upsert");

    let row = store
        .call(move |c| get_translation(c, id))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.text, "reviewer text", "locked text is preserved");
    assert_eq!(row.origin, TranslationOrigin::User);
}

// AC: M6-B1 — list_translations returns rows in cue order.
#[tokio::test]
async fn list_translations_is_ordered_by_cue() {
    let store = store_with_cues().await;
    let id1 = cue_id(&store, 1).await;
    let id2 = cue_id(&store, 2).await;

    for (id, text) in [(id2, "two"), (id1, "one")] {
        let t = NewTranslation {
            cue_id: id,
            text: text.to_string(),
            origin: TranslationOrigin::Llm,
            locked: false,
            stale: false,
            flags: vec![],
            analysis_version: 1,
            model: None,
        };
        store
            .call(move |c| upsert_translation(c, &t))
            .await
            .expect("upsert");
    }

    let all = store.call(|c| list_translations(c)).await.expect("list");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].cue_id, id1);
    assert_eq!(all[1].cue_id, id2);
}
