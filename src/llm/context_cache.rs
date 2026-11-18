//! Gemini explicit context-cache lifecycle and the `auto` break-even decision
//! (`docs/LLM.md` §5.5, `docs/ARCHITECTURE.md` §3/§5, `docs/DATA_MODEL.md` §2).
//!
//! Two concerns live here:
//!
//! 1. **Break-even** ([`should_create_explicit_cache`]) — a *pure* decision over the
//!    `cache.provider` mode, the stable-prefix token count, the batch count, the
//!    estimated stage duration, and the model price. It implements `docs/LLM.md`
//!    §5.5: `off`/`implicit` never create; `explicit` always creates once the prefix
//!    reaches `min_cache_tokens`; `auto` creates only when the estimated saving is
//!    positive. No I/O, so it is densely unit-tested.
//!
//! 2. **Lifecycle** ([`get_or_create`] / [`delete_cache`]) — creates one Gemini
//!    `cachedContents` entry per analysis version + model over the stable prefix
//!    (system instruction + canonical analysis sheet, ADR-006), records it in
//!    `provider_cache`, reuses it on resume when it has not expired, recreates it
//!    when it has, and deletes it at stage end. The Gemini wire calls go through the
//!    narrow [`ContextCacheClient`] seam (implemented by `gemini.rs`, so the wire
//!    names stay there); the `provider_cache` persistence goes through the narrow
//!    [`ProviderCacheStore`] seam (implemented by `store/repo_llm.rs`). Both seams
//!    are defined here so `llm` never references `store` (ADR-030,
//!    `docs/ARCHITECTURE.md` §3).
//!
//! Expiry is tracked locally as `now + ttl` in epoch seconds rather than by parsing
//! the server's RFC3339 `expireTime`: `chrono` is not an approved dependency
//! (`docs/ARCHITECTURE.md` §10) and an injected integer clock keeps the
//! reuse-vs-recreate decision deterministic in tests (ADR-042).

use std::future::Future;

use thiserror::Error;

use crate::llm::client::LlmError;
use crate::llm::pricing::ModelPrice;

// ---------------------------------------------------------------------------
// Break-even (pure)
// ---------------------------------------------------------------------------

/// The `cache.provider` mode (`docs/CONFIG.md`, `docs/LLM.md` §5.5). Logical mirror
/// of the validated `[cache].provider` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheProvider {
    /// Never create explicit caches (and no implicit-cache assumption).
    Off,
    /// Rely on Gemini implicit caching only; never create explicit caches.
    Implicit,
    /// Always create an explicit cache once the prefix reaches `min_cache_tokens`.
    Explicit,
    /// Create an explicit cache only when the estimated saving is positive.
    Auto,
}

impl CacheProvider {
    /// Parses the validated `[cache].provider` label. Returns `None` for an
    /// unrecognised value (config validation already rejects those, so a loaded
    /// config always parses).
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "off" => Some(Self::Off),
            "implicit" => Some(Self::Implicit),
            "explicit" => Some(Self::Explicit),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

/// Why [`should_create_explicit_cache`] declined to create an explicit cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// `provider` is `off` or `implicit`: explicit caches are disabled.
    ProviderDisabled,
    /// The stable prefix is smaller than `min_cache_tokens`.
    BelowMinTokens,
    /// `provider` is `auto` and the estimated saving is not positive.
    NoSaving,
    /// `provider` is `auto` but the model has no price entry, so the saving cannot
    /// be estimated — skip rather than risk paying storage for no benefit.
    UnknownPrice,
}

/// The break-even decision (`docs/LLM.md` §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheDecision {
    /// Create an explicit cache for the stable prefix.
    Create,
    /// Do not create one; `reason` explains why.
    Skip { reason: SkipReason },
}

impl CacheDecision {
    /// Whether the decision is to create a cache.
    pub fn should_create(self) -> bool {
        matches!(self, CacheDecision::Create)
    }
}

/// Decides whether to create a Gemini explicit context cache for the stable prefix
/// (`docs/LLM.md` §5.5). Pure: no I/O.
///
/// - `off` / `implicit` → always [`CacheDecision::Skip`] ([`SkipReason::ProviderDisabled`]).
/// - `explicit` → [`CacheDecision::Create`] iff `prefix_tokens >= min_cache_tokens`.
/// - `auto` → create iff `prefix_tokens >= min_cache_tokens` **and** the estimated
///   saving is strictly positive:
///   ```text
///   saving = (batches - 1) * prefix_tokens * input_price
///              * (explicit_discount - implicit_hit_rate * implicit_discount)
///            - prefix_tokens * storage_price_per_hour * est_hours
///   ```
///   (both terms divided by 1e6, matching the per-1M-token price basis of
///   [`crate::llm::pricing`]).
///
/// Discount derivation (ADR-042): `[pricing]` has no `explicit_discount` /
/// `implicit_discount` keys, so both are derived from the input vs cached-input
/// prices — `discount = (input - cached_input) / input`, clamped to `[0, 1]`. The
/// explicit and implicit discounts are therefore equal under this model; the
/// formula keeps them as distinct terms so M9 tuning can diverge them later.
///
/// An `auto` decision with no price entry (`model_price == None`) is a
/// [`SkipReason::UnknownPrice`] skip (the saving cannot be computed). `explicit`
/// does not consult the price at all.
pub fn should_create_explicit_cache(
    provider: CacheProvider,
    prefix_tokens: u64,
    batches: u64,
    est_hours: f64,
    model_price: Option<&ModelPrice>,
    implicit_hit_rate: f64,
    min_cache_tokens: u64,
) -> CacheDecision {
    match provider {
        CacheProvider::Off | CacheProvider::Implicit => CacheDecision::Skip {
            reason: SkipReason::ProviderDisabled,
        },
        CacheProvider::Explicit => {
            if prefix_tokens >= min_cache_tokens {
                CacheDecision::Create
            } else {
                CacheDecision::Skip {
                    reason: SkipReason::BelowMinTokens,
                }
            }
        }
        CacheProvider::Auto => {
            if prefix_tokens < min_cache_tokens {
                return CacheDecision::Skip {
                    reason: SkipReason::BelowMinTokens,
                };
            }
            let Some(price) = model_price else {
                return CacheDecision::Skip {
                    reason: SkipReason::UnknownPrice,
                };
            };
            if estimated_saving_usd(prefix_tokens, batches, est_hours, price, implicit_hit_rate)
                > 0.0
            {
                CacheDecision::Create
            } else {
                CacheDecision::Skip {
                    reason: SkipReason::NoSaving,
                }
            }
        }
    }
}

/// The estimated USD saving of an explicit cache over the stage (`docs/LLM.md`
/// §5.5). Positive means creating the cache is worth it.
///
/// ```text
/// saving = (batches - 1) * prefix_tokens * input_price
///            * (explicit_discount - implicit_hit_rate * implicit_discount)
///          - prefix_tokens * storage_price_per_hour * est_hours
/// ```
///
/// Both terms are divided by 1e6 to match the per-1M-token price basis of
/// [`crate::llm::pricing`]. Because that common positive divisor cannot change the
/// sign, the create/skip outcome is unaffected by it; dividing keeps the returned
/// figure a real USD amount for any future reporting.
fn estimated_saving_usd(
    prefix_tokens: u64,
    batches: u64,
    est_hours: f64,
    price: &ModelPrice,
    implicit_hit_rate: f64,
) -> f64 {
    // `[pricing]` has no explicit/implicit discount keys, so both are derived from
    // the input vs cached-input prices (ADR-042). They are equal under this model;
    // kept as separate terms so M9 tuning can diverge them.
    let explicit_discount = input_discount(price);
    let implicit_discount = input_discount(price);
    let net_discount = explicit_discount - implicit_hit_rate * implicit_discount;

    let benefiting_batches = batches.saturating_sub(1) as f64;
    let prefix = prefix_tokens as f64;
    let gross_input_saving = benefiting_batches * prefix * price.input * net_discount;
    let storage_cost = prefix * price.storage_per_hour * est_hours;

    (gross_input_saving - storage_cost) / 1_000_000.0
}

/// The fractional input-price reduction when a prefix token is served from cache
/// (ADR-042): `(input - cached_input) / input`, clamped to `[0, 1]`. A zero (or
/// negative) input price yields a zero discount (no saving to be had).
fn input_discount(price: &ModelPrice) -> f64 {
    if price.input > 0.0 {
        ((price.input - price.cached_input) / price.input).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Lifecycle types + seams
// ---------------------------------------------------------------------------

/// A logical request to create one Gemini `cachedContents` entry over the stable
/// prefix (`docs/LLM.md` §2/§5.5). Provider-agnostic; `gemini.rs` maps it to the
/// wire shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateCacheRequest {
    /// The model the cache is bound to (the plain model name; `gemini.rs` qualifies
    /// it to `models/<name>`).
    pub model: String,
    /// The system instruction baked into the cache (empty to omit).
    pub system_instruction: String,
    /// The cached content — the canonical analysis sheet (ADR-006).
    pub content: String,
    /// Time-to-live in seconds (estimated duration + `ttl_margin_secs`).
    pub ttl_secs: u64,
}

/// The result of a successful create: the Gemini `cachedContents/...` resource name
/// and the token count the server reported for the cached content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheHandle {
    /// The `cachedContents/...` resource name used to reference and delete the cache.
    pub name: String,
    /// Tokens the server reported for the cached content (`0` when absent).
    pub token_count: u64,
}

/// A `provider_cache` row (`docs/DATA_MODEL.md` §2). `expires_at` is epoch seconds
/// (ADR-042), not the server's RFC3339 `expireTime`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCacheEntry {
    pub name: String,
    pub analysis_version: i64,
    pub model: String,
    pub token_count: u64,
    pub expires_at: i64,
}

/// Everything [`get_or_create`] needs to create a cache for one analysis version +
/// model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheSpec {
    pub analysis_version: i64,
    pub model: String,
    pub system_instruction: String,
    pub content: String,
    pub ttl_secs: u64,
}

/// The narrow Gemini `cachedContents` seam (`docs/ARCHITECTURE.md` §3/§5). Defined
/// here; implemented by `gemini.rs` so the wire names stay confined there.
pub trait ContextCacheClient {
    /// Creates a `cachedContents` entry and returns its handle.
    fn create_cache(
        &self,
        req: CreateCacheRequest,
    ) -> impl Future<Output = Result<CacheHandle, LlmError>> + Send;

    /// Deletes the `cachedContents` entry named `name` (a `cachedContents/...`
    /// resource name).
    fn delete_cache(&self, name: String) -> impl Future<Output = Result<(), LlmError>> + Send;
}

/// An error from the `provider_cache` backend. Store-agnostic, mirroring
/// [`CacheError`](crate::llm::cache::CacheError): the store-backed implementation
/// maps its own error into [`ProviderCacheStoreError::Backend`], keeping `llm` free
/// of any `store` type (ADR-030).
#[derive(Debug, Error)]
pub enum ProviderCacheStoreError {
    #[error("provider cache store error: {0}")]
    Backend(String),
}

/// The narrow `provider_cache` persistence seam (`docs/ARCHITECTURE.md` §3). Defined
/// here; implemented by `store/repo_llm.rs` (store depends on this trait, not the
/// reverse).
pub trait ProviderCacheStore {
    /// Reads the `provider_cache` row for `(analysis_version, model)`, if any.
    fn get(
        &self,
        analysis_version: i64,
        model: String,
    ) -> impl Future<Output = Result<Option<ProviderCacheEntry>, ProviderCacheStoreError>> + Send;

    /// Inserts or replaces a `provider_cache` row (keyed by `name`).
    fn upsert(
        &self,
        entry: ProviderCacheEntry,
    ) -> impl Future<Output = Result<(), ProviderCacheStoreError>> + Send;

    /// Deletes the `provider_cache` row named `name`.
    fn delete(
        &self,
        name: String,
    ) -> impl Future<Output = Result<(), ProviderCacheStoreError>> + Send;
}

/// An error from the explicit-cache lifecycle (`docs/ARCHITECTURE.md` §8).
#[derive(Debug, Error)]
pub enum ContextCacheError {
    /// A Gemini `cachedContents` transport call failed.
    #[error("explicit cache transport error: {0}")]
    Client(#[from] LlmError),
    /// A `provider_cache` persistence call failed.
    #[error("explicit cache store error: {0}")]
    Store(#[from] ProviderCacheStoreError),
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Returns the Gemini `cachedContents/...` name to reference for `spec`, creating or
/// recreating it as needed (`docs/LLM.md` §5.5):
///
/// - a stored row that has **not** expired (`expires_at > now`) is **reused** — no
///   create call is sent, and its name is returned;
/// - a stored row that **has** expired is deleted (remote best-effort + local row)
///   and a fresh cache is created (**recreate**);
/// - with no stored row, a fresh cache is created (**create**).
///
/// `now` is injected (epoch seconds) so the reuse-vs-recreate decision is
/// deterministic (ADR-042).
pub async fn get_or_create<C, P>(
    client: &C,
    store: &P,
    spec: &CacheSpec,
    now: i64,
) -> Result<String, ContextCacheError>
where
    C: ContextCacheClient + Sync,
    P: ProviderCacheStore + Sync,
{
    if let Some(existing) = store.get(spec.analysis_version, spec.model.clone()).await? {
        if existing.expires_at > now {
            tracing::debug!(cache = %existing.name, "reusing unexpired explicit cache");
            return Ok(existing.name);
        }
        // Expired: drop the stale cache before creating a fresh one. Remove the LOCAL
        // row FIRST, then best-effort remote delete (crash-safe ordering, see
        // [`delete_cache`]): a crash then leaves at worst an orphaned remote cache
        // (TTL reaps it), never a local row pointing at a deleted remote.
        tracing::debug!(cache = %existing.name, "explicit cache expired; recreating");
        let old_name = existing.name;
        store.delete(old_name.clone()).await?;
        best_effort_remote_delete(client, &old_name).await;
    }

    let handle = client
        .create_cache(CreateCacheRequest {
            model: spec.model.clone(),
            system_instruction: spec.system_instruction.clone(),
            content: spec.content.clone(),
            ttl_secs: spec.ttl_secs,
        })
        .await?;

    // Track expiry locally as now + ttl rather than parsing the server's RFC3339
    // `expireTime` (ADR-042: `chrono` is not approved, and an injected integer clock
    // keeps the reuse decision deterministic).
    let entry = ProviderCacheEntry {
        name: handle.name.clone(),
        analysis_version: spec.analysis_version,
        model: spec.model.clone(),
        token_count: handle.token_count,
        expires_at: now.saturating_add(spec.ttl_secs as i64),
    };
    store.upsert(entry).await?;
    Ok(handle.name)
}

/// Deletes the explicit cache named `name` at stage end (`docs/LLM.md` §5.5).
///
/// The LOCAL `provider_cache` row is removed FIRST, then the remote `cachedContents`
/// entry is deleted best-effort (a failure is logged, not propagated, since the cache
/// expires via its TTL). This ordering is crash-safe: a crash between the two steps
/// leaves at worst an orphaned remote cache (reaped by its TTL), never a local row
/// whose `expires_at` is still in the future pointing at a now-deleted remote cache
/// — which a later [`get_or_create`] would wrongly reuse as a live handle.
pub async fn delete_cache<C, P>(client: &C, store: &P, name: &str) -> Result<(), ContextCacheError>
where
    C: ContextCacheClient + Sync,
    P: ProviderCacheStore + Sync,
{
    store.delete(name.to_string()).await?;
    best_effort_remote_delete(client, name).await;
    Ok(())
}

/// Deletes a remote `cachedContents` entry, logging (not propagating) a failure: an
/// already-expired or already-deleted cache would otherwise abort the run, and the
/// cache expires via its TTL regardless (`docs/LLM.md` §5.5).
async fn best_effort_remote_delete<C: ContextCacheClient + Sync>(client: &C, name: &str) {
    if let Err(err) = client.delete_cache(name.to_string()).await {
        tracing::warn!(
            cache = %name,
            error = %err,
            "failed to delete explicit cache; it will expire via its TTL"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A [`ContextCacheClient`] that records each call in a shared log and can be
    /// configured to fail its delete, so a test can assert call ordering and
    /// best-effort delete tolerance deterministically (no network).
    struct RecordingClient {
        events: Arc<Mutex<Vec<String>>>,
        fail_delete: bool,
    }

    impl ContextCacheClient for RecordingClient {
        async fn create_cache(&self, _req: CreateCacheRequest) -> Result<CacheHandle, LlmError> {
            self.events
                .lock()
                .unwrap()
                .push("remote_create".to_string());
            Ok(CacheHandle {
                name: "cachedContents/new".to_string(),
                token_count: 1,
            })
        }

        async fn delete_cache(&self, name: String) -> Result<(), LlmError> {
            self.events
                .lock()
                .unwrap()
                .push(format!("remote_delete:{name}"));
            if self.fail_delete {
                Err(LlmError::Timeout)
            } else {
                Ok(())
            }
        }
    }

    /// An in-memory [`ProviderCacheStore`] holding at most one row, recording its
    /// delete in the shared log for ordering assertions.
    struct RecordingStore {
        events: Arc<Mutex<Vec<String>>>,
        row: Arc<Mutex<Option<ProviderCacheEntry>>>,
    }

    impl ProviderCacheStore for RecordingStore {
        async fn get(
            &self,
            analysis_version: i64,
            model: String,
        ) -> Result<Option<ProviderCacheEntry>, ProviderCacheStoreError> {
            Ok(self
                .row
                .lock()
                .unwrap()
                .clone()
                .filter(|e| e.analysis_version == analysis_version && e.model == model))
        }

        async fn upsert(&self, entry: ProviderCacheEntry) -> Result<(), ProviderCacheStoreError> {
            *self.row.lock().unwrap() = Some(entry);
            Ok(())
        }

        async fn delete(&self, name: String) -> Result<(), ProviderCacheStoreError> {
            self.events
                .lock()
                .unwrap()
                .push(format!("local_delete:{name}"));
            let mut guard = self.row.lock().unwrap();
            if guard.as_ref().is_some_and(|e| e.name == name) {
                *guard = None;
            }
            Ok(())
        }
    }

    // AC: M8-AC2 (crash ordering) — delete_cache removes the LOCAL row FIRST, then
    // deletes the remote best-effort, and still removes the local row (and returns Ok)
    // when the remote delete fails.
    #[tokio::test]
    async fn delete_cache_removes_local_before_remote_and_tolerates_remote_failure() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let row = Arc::new(Mutex::new(Some(ProviderCacheEntry {
            name: "cachedContents/x".to_string(),
            analysis_version: 3,
            model: "gemini-flash-latest".to_string(),
            token_count: 1,
            expires_at: 9_999_999,
        })));
        let client = RecordingClient {
            events: events.clone(),
            fail_delete: true, // the remote delete fails
        };
        let store = RecordingStore {
            events: events.clone(),
            row: row.clone(),
        };

        delete_cache(&client, &store, "cachedContents/x")
            .await
            .expect("delete is Ok despite a failed remote delete");

        // The local row is gone even though the remote delete failed.
        assert!(row.lock().unwrap().is_none());
        // Ordering: local delete happened strictly before the remote attempt.
        let log = events.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "local_delete:cachedContents/x".to_string(),
                "remote_delete:cachedContents/x".to_string(),
            ]
        );
    }

    // AC: M8-AC2 (crash ordering) — on recreate, the expired local row is deleted
    // before the remote delete and before the create, so a crash never leaves a local
    // row pointing at a deleted remote.
    #[tokio::test]
    async fn recreate_deletes_local_before_remote_then_creates() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let row = Arc::new(Mutex::new(Some(ProviderCacheEntry {
            name: "cachedContents/old".to_string(),
            analysis_version: 3,
            model: "gemini-flash-latest".to_string(),
            token_count: 1,
            expires_at: 10, // expired relative to `now` below
        })));
        let client = RecordingClient {
            events: events.clone(),
            fail_delete: false,
        };
        let store = RecordingStore {
            events: events.clone(),
            row: row.clone(),
        };
        let spec = CacheSpec {
            analysis_version: 3,
            model: "gemini-flash-latest".to_string(),
            system_instruction: "sys".to_string(),
            content: "sheet".to_string(),
            ttl_secs: 900,
        };

        let name = get_or_create(&client, &store, &spec, 1_000)
            .await
            .expect("recreate");
        assert_eq!(name, "cachedContents/new");

        let log = events.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "local_delete:cachedContents/old".to_string(),
                "remote_delete:cachedContents/old".to_string(),
                "remote_create".to_string(),
            ]
        );
    }

    /// A model price with a known input/cached-input/storage profile for the
    /// break-even tests. `input = 1.0`, `cached_input = 0.25` → discount `0.75`.
    fn price() -> ModelPrice {
        ModelPrice {
            input: 1.0,
            cached_input: 0.25,
            output: 2.0,
            storage_per_hour: 0.01,
            flex_multiplier: 0.5,
            long_prompt_threshold: None,
            long_input: None,
            long_output: None,
        }
    }

    // AC: M8-AC1 — off / implicit never create, regardless of the other inputs.
    #[test]
    fn off_and_implicit_always_skip() {
        for provider in [CacheProvider::Off, CacheProvider::Implicit] {
            let decision = should_create_explicit_cache(
                provider,
                1_000_000, // huge prefix
                100,       // many batches
                1.0,
                Some(&price()),
                0.0,
                2048,
            );
            assert_eq!(
                decision,
                CacheDecision::Skip {
                    reason: SkipReason::ProviderDisabled
                },
                "{provider:?} must never create"
            );
        }
    }

    // AC: M8-AC1 — explicit creates iff the prefix reaches min_cache_tokens
    // (price is irrelevant to explicit).
    #[test]
    fn explicit_creates_at_or_above_min_tokens() {
        // At the threshold: create.
        let at = should_create_explicit_cache(
            CacheProvider::Explicit,
            2048,
            1,   // even a single batch
            0.0, // and no duration
            None,
            0.5,
            2048,
        );
        assert_eq!(at, CacheDecision::Create);

        // Just below the threshold: skip.
        let below = should_create_explicit_cache(
            CacheProvider::Explicit,
            2047,
            100,
            1.0,
            Some(&price()),
            0.5,
            2048,
        );
        assert_eq!(
            below,
            CacheDecision::Skip {
                reason: SkipReason::BelowMinTokens
            }
        );
    }

    // AC: M8-AC1 — auto creates when the saving is clearly positive (many batches,
    // large prefix, cheap storage, short duration).
    #[test]
    fn auto_creates_when_saving_positive() {
        let decision = should_create_explicit_cache(
            CacheProvider::Auto,
            10_000,
            10,
            1.0,
            Some(&price()),
            0.5,
            2048,
        );
        assert_eq!(decision, CacheDecision::Create);
    }

    // AC: M8-AC1 — auto skips with a single batch: the `(batches - 1)` factor is
    // zero, so the whole gross saving vanishes and only storage cost remains.
    #[test]
    fn auto_skips_with_single_batch() {
        let decision = should_create_explicit_cache(
            CacheProvider::Auto,
            10_000,
            1,
            1.0,
            Some(&price()),
            0.5,
            2048,
        );
        assert_eq!(
            decision,
            CacheDecision::Skip {
                reason: SkipReason::NoSaving
            }
        );
    }

    // AC: M8-AC1 — auto skips when storage is expensive and the TTL is long enough
    // that storage outweighs the per-batch input saving.
    #[test]
    fn auto_skips_when_storage_dominates() {
        let mut p = price();
        p.storage_per_hour = 100.0;
        let decision = should_create_explicit_cache(
            CacheProvider::Auto,
            10_000,
            2,
            10.0, // long duration
            Some(&p),
            0.5,
            2048,
        );
        assert_eq!(
            decision,
            CacheDecision::Skip {
                reason: SkipReason::NoSaving
            }
        );
    }

    // AC: M8-AC1 — the est_hours term alone can flip the decision: the same inputs
    // create for a short stage and skip for a long one (storage grows with hours).
    #[test]
    fn auto_est_hours_flips_decision() {
        let mut p = price();
        // Tune storage so a short run saves but a long run does not.
        p.storage_per_hour = 0.03;
        let short =
            should_create_explicit_cache(CacheProvider::Auto, 10_000, 2, 0.1, Some(&p), 0.5, 2048);
        let long = should_create_explicit_cache(
            CacheProvider::Auto,
            10_000,
            2,
            1000.0,
            Some(&p),
            0.5,
            2048,
        );
        assert_eq!(short, CacheDecision::Create);
        assert_eq!(
            long,
            CacheDecision::Skip {
                reason: SkipReason::NoSaving
            }
        );
    }

    // AC: M8-AC1 — the implicit_hit_rate term scales the benefit: a hit rate of 1.0
    // cancels the explicit discount (net rate 0), so there is nothing to save.
    #[test]
    fn auto_full_implicit_hit_rate_cancels_saving() {
        let decision = should_create_explicit_cache(
            CacheProvider::Auto,
            10_000,
            100,
            1.0,
            Some(&price()),
            1.0, // implicit cache would have hit every time anyway
            2048,
        );
        assert_eq!(
            decision,
            CacheDecision::Skip {
                reason: SkipReason::NoSaving
            }
        );
        // The same inputs with a zero hit rate (no implicit benefit assumed) create.
        let zero = should_create_explicit_cache(
            CacheProvider::Auto,
            10_000,
            100,
            1.0,
            Some(&price()),
            0.0,
            2048,
        );
        assert_eq!(zero, CacheDecision::Create);
    }

    // AC: M8-AC1 — a zero input price yields a zero discount, so there is no input
    // saving to offset storage: auto skips.
    #[test]
    fn auto_zero_input_price_skips() {
        let mut p = price();
        p.input = 0.0;
        p.cached_input = 0.0;
        let decision = should_create_explicit_cache(
            CacheProvider::Auto,
            10_000,
            100,
            1.0,
            Some(&p),
            0.5,
            2048,
        );
        assert_eq!(
            decision,
            CacheDecision::Skip {
                reason: SkipReason::NoSaving
            }
        );
    }

    // AC: M8-AC1 — auto with no price entry cannot compute the saving, so it skips
    // with a dedicated reason (never pays storage blindly).
    #[test]
    fn auto_unknown_price_skips() {
        let decision =
            should_create_explicit_cache(CacheProvider::Auto, 10_000, 100, 1.0, None, 0.5, 2048);
        assert_eq!(
            decision,
            CacheDecision::Skip {
                reason: SkipReason::UnknownPrice
            }
        );
    }

    // AC: M8-AC1 — auto below the min-token threshold skips before any saving math.
    #[test]
    fn auto_below_min_tokens_skips() {
        let decision = should_create_explicit_cache(
            CacheProvider::Auto,
            100,
            100,
            1.0,
            Some(&price()),
            0.5,
            2048,
        );
        assert_eq!(
            decision,
            CacheDecision::Skip {
                reason: SkipReason::BelowMinTokens
            }
        );
    }

    #[test]
    fn provider_parse_round_trips_known_labels() {
        assert_eq!(CacheProvider::parse("off"), Some(CacheProvider::Off));
        assert_eq!(
            CacheProvider::parse("implicit"),
            Some(CacheProvider::Implicit)
        );
        assert_eq!(
            CacheProvider::parse("explicit"),
            Some(CacheProvider::Explicit)
        );
        assert_eq!(CacheProvider::parse("auto"), Some(CacheProvider::Auto));
        assert_eq!(CacheProvider::parse("nope"), None);
    }
}
