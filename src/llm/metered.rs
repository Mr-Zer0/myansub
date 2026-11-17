//! The usage-recording layer (`docs/LLM.md` §5/§9, `docs/ARCHITECTURE.md` §5).
//!
//! [`Metered`] is the **outermost** layer (`Metered<Cached<Retrying<Transport>>>`).
//! On every logical `generate` — success or terminal error — it writes exactly one
//! `llm_calls` row through the narrow [`UsageSink`] seam, including when the inner
//! [`Cached`](crate::llm::cache) layer served a local cache hit (cost 0,
//! `cache_hit_local = 1`) and including dry-run calls (`dry_run = 1`). Transport
//! retries happen inside `Retrying` and are invisible here, so one row is one
//! logical call, not one HTTP attempt.
//!
//! [`UsageSink`] is defined here (not in `store`) so `llm` owns the narrow seam
//! (`docs/ARCHITECTURE.md` §3); the store-backed implementation lives in
//! `store/repo_llm.rs`.
//!
//! Metering is **best-effort** for FR-9: a `UsageSink` write failure is logged at
//! `error` and swallowed (so a backend hiccup never discards an already-paid-for
//! response), which means the `llm_calls` ledger and cost reporting can under-count
//! when the sink fails.

use std::future::Future;
use std::time::Instant;

use thiserror::Error;

use crate::llm::client::{ErrorClass, LlmClient, LlmError, LlmRequest, LlmResponse};

/// An error from the usage sink. Store-agnostic, mirroring
/// [`CacheError`](crate::llm::cache::CacheError).
#[derive(Debug, Error)]
pub enum SinkError {
    /// The underlying store failed (I/O, SQLite).
    #[error("usage sink error: {0}")]
    Backend(String),
}

/// The terminal outcome of a logical call, stored in `llm_calls.outcome`
/// (`docs/DATA_MODEL.md` §2): `ok|retryable|fatal|cancelled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome {
    Ok,
    Retryable,
    Fatal,
    Cancelled,
}

impl CallOutcome {
    /// The label stored in the `outcome` column.
    pub fn as_str(self) -> &'static str {
        match self {
            CallOutcome::Ok => "ok",
            CallOutcome::Retryable => "retryable",
            CallOutcome::Fatal => "fatal",
            CallOutcome::Cancelled => "cancelled",
        }
    }
}

/// One `llm_calls` row (`docs/DATA_MODEL.md` §2). Token fields are `Option` so a
/// failed call (no usage) records `NULL` rather than a misleading zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmCallRecord {
    pub stage: String,
    pub unit: Option<i64>,
    pub model: String,
    pub service_tier: Option<String>,
    pub prompt_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub thinking_tokens: Option<i64>,
    pub latency_ms: Option<i64>,
    pub cache_hit_local: bool,
    pub dry_run: bool,
    pub outcome: CallOutcome,
}

/// The narrow persistence seam for usage recording (`docs/ARCHITECTURE.md` §3).
/// Defined in `llm`; implemented by `store`.
pub trait UsageSink {
    /// Persists one `llm_calls` row.
    fn record(&self, call: LlmCallRecord) -> impl Future<Output = Result<(), SinkError>> + Send;
}

/// Per-run call context the `llm_calls` row needs that the [`LlmRequest`] does not
/// carry (`docs/DATA_MODEL.md` §2). The pipeline (M5/M6) builds one `Metered` per
/// unit over a shared inner stack, supplying the stage, unit, and dry-run flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallContext {
    pub stage: String,
    pub unit: Option<i64>,
    pub dry_run: bool,
}

impl CallContext {
    /// A context for `stage` with no specific unit and `dry_run = false`.
    pub fn stage(stage: impl Into<String>) -> Self {
        Self {
            stage: stage.into(),
            unit: None,
            dry_run: false,
        }
    }

    /// Sets the unit id.
    pub fn with_unit(mut self, unit: i64) -> Self {
        self.unit = Some(unit);
        self
    }

    /// Marks the context as a dry run (rows get `dry_run = 1`).
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }
}

/// The usage-recording layer (`docs/LLM.md` §5).
#[derive(Debug, Clone)]
pub struct Metered<T, U> {
    inner: T,
    sink: U,
    context: CallContext,
}

impl<T, U> Metered<T, U> {
    /// Wraps `inner` (normally `Cached<Retrying<Transport>>`) to record usage via
    /// `sink` under `context`.
    pub fn new(inner: T, sink: U, context: CallContext) -> Self {
        Self {
            inner,
            sink,
            context,
        }
    }
}

impl<T: LlmClient + Sync, U: UsageSink + Sync> LlmClient for Metered<T, U> {
    async fn generate(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let model = req.model.clone();
        let service_tier = req.service_tier.label().to_string();

        let start = Instant::now();
        let result = self.inner.generate(req).await;
        let latency_ms = start.elapsed().as_millis() as i64;

        let record = self.build_record(model, service_tier, latency_ms, &result);
        if let Err(err) = self.sink.record(record).await {
            // A ledger write must not discard a successful, already-paid-for
            // response; surface the failure in logs and return the real result.
            tracing::error!("failed to record llm_call usage: {err}");
        }

        result
    }
}

impl<T, U> Metered<T, U> {
    fn build_record(
        &self,
        model: String,
        service_tier: String,
        latency_ms: i64,
        result: &Result<LlmResponse, LlmError>,
    ) -> LlmCallRecord {
        let base = |cache_hit_local: bool, outcome: CallOutcome| LlmCallRecord {
            stage: self.context.stage.clone(),
            unit: self.context.unit,
            model: model.clone(),
            service_tier: Some(service_tier.clone()),
            prompt_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            thinking_tokens: None,
            latency_ms: Some(latency_ms),
            cache_hit_local,
            dry_run: self.context.dry_run,
            outcome,
        };

        match result {
            Ok(response) if response.usage.cache_hit_local => {
                // A local cache hit consumed no API tokens, so the row records
                // zero counts with `cache_hit_local = 1` (cost 0, `docs/LLM.md`
                // §5).
                LlmCallRecord {
                    prompt_tokens: Some(0),
                    cached_tokens: Some(0),
                    output_tokens: Some(0),
                    thinking_tokens: Some(0),
                    ..base(true, CallOutcome::Ok)
                }
            }
            Ok(response) => {
                let usage = &response.usage;
                LlmCallRecord {
                    prompt_tokens: Some(usage.prompt_tokens as i64),
                    cached_tokens: Some(usage.cached_tokens as i64),
                    output_tokens: Some(usage.output_tokens as i64),
                    thinking_tokens: Some(usage.thinking_tokens as i64),
                    ..base(false, CallOutcome::Ok)
                }
            }
            Err(err) => base(false, outcome_for(err)),
        }
    }
}

/// Maps a terminal [`LlmError`] to its `llm_calls.outcome` label.
fn outcome_for(err: &LlmError) -> CallOutcome {
    match err {
        LlmError::Cancelled => CallOutcome::Cancelled,
        _ => match err.class() {
            ErrorClass::Fatal => CallOutcome::Fatal,
            _ => CallOutcome::Retryable,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::client::{
        CommitToken, ContentBlock, GenerationConfig, ServiceTier, ThinkingSetting, Usage,
    };
    use crate::llm::fake::Fake;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MemSink {
        rows: Arc<Mutex<Vec<LlmCallRecord>>>,
    }

    impl MemSink {
        fn rows(&self) -> Vec<LlmCallRecord> {
            self.rows.lock().expect("lock").clone()
        }
    }

    impl UsageSink for MemSink {
        async fn record(&self, call: LlmCallRecord) -> Result<(), SinkError> {
            self.rows.lock().expect("lock").push(call);
            Ok(())
        }
    }

    fn request() -> LlmRequest {
        LlmRequest {
            model: "gemini-flash-latest".to_string(),
            service_tier: ServiceTier::Flex,
            system_instruction: "sys".to_string(),
            contents: vec![ContentBlock::user("lines")],
            generation_config: GenerationConfig {
                max_output_tokens: 64,
                response_mime_type: "application/json".to_string(),
                response_schema: None,
                thinking: ThinkingSetting::Off,
            },
        }
    }

    fn response(usage: Usage) -> LlmResponse {
        LlmResponse {
            text: "ok".to_string(),
            usage,
            commit: CommitToken::noop(),
        }
    }

    #[tokio::test]
    async fn one_logical_call_records_exactly_one_row() {
        // AC: M3-AC4 — one generate writes exactly one row with the usage fields.
        let usage = Usage {
            prompt_tokens: 100,
            cached_tokens: 20,
            output_tokens: 40,
            thinking_tokens: 5,
            ..Usage::default()
        };
        let fake = Fake::new().push_response(response(usage));
        let sink = MemSink::default();
        let metered = Metered::new(
            fake,
            sink.clone(),
            CallContext::stage("translation").with_unit(7),
        );

        metered.generate(request()).await.expect("ok");

        let rows = sink.rows();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.stage, "translation");
        assert_eq!(row.unit, Some(7));
        assert_eq!(row.model, "gemini-flash-latest");
        assert_eq!(row.service_tier.as_deref(), Some("flex"));
        assert_eq!(row.prompt_tokens, Some(100));
        assert_eq!(row.cached_tokens, Some(20));
        assert_eq!(row.output_tokens, Some(40));
        assert_eq!(row.thinking_tokens, Some(5));
        assert!(!row.cache_hit_local);
        assert!(!row.dry_run);
        assert_eq!(row.outcome, CallOutcome::Ok);
    }

    #[tokio::test]
    async fn local_cache_hit_records_one_zero_cost_row() {
        // AC: M3-AC4 — a hit (cache_hit_local usage) still records exactly one
        // row, flagged cache_hit_local with zero-token (cost 0) semantics.
        let usage = Usage {
            prompt_tokens: 100,
            output_tokens: 40,
            cache_hit_local: true,
            ..Usage::default()
        };
        let fake = Fake::new().push_response(response(usage));
        let sink = MemSink::default();
        let metered = Metered::new(fake, sink.clone(), CallContext::stage("translation"));

        metered.generate(request()).await.expect("ok");

        let rows = sink.rows();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(row.cache_hit_local);
        assert_eq!(row.prompt_tokens, Some(0));
        assert_eq!(row.output_tokens, Some(0));
        assert_eq!(row.outcome, CallOutcome::Ok);
    }

    #[tokio::test]
    async fn dry_run_context_flags_the_row() {
        // AC: M3-AC4 — a dry-run call records dry_run = 1.
        let fake = Fake::new().push_response(response(Usage::default()));
        let sink = MemSink::default();
        let metered = Metered::new(
            fake,
            sink.clone(),
            CallContext::stage("analysis").dry_run(true),
        );

        metered.generate(request()).await.expect("ok");

        let rows = sink.rows();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].dry_run);
    }

    #[tokio::test]
    async fn terminal_error_records_exactly_one_row_with_outcome() {
        // AC: M3-AC4 — a terminal error still writes one row (no token counts).
        let fake = Fake::new().push_result(Err(LlmError::Fatal {
            status: 400,
            message: "bad".to_string(),
        }));
        let sink = MemSink::default();
        let metered = Metered::new(fake, sink.clone(), CallContext::stage("translation"));

        let err = metered.generate(request()).await.expect_err("fatal");
        assert!(matches!(err, LlmError::Fatal { .. }));

        let rows = sink.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, CallOutcome::Fatal);
        assert_eq!(rows[0].prompt_tokens, None);
    }
}
