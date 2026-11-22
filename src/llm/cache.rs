//! The local response cache layer (`docs/LLM.md` §4, `docs/ARCHITECTURE.md` §5).
//!
//! [`Cached`] sits between `Metered` and `Retrying`
//! (`Metered<Cached<Retrying<Transport>>>`), so a cache hit never reaches the
//! retry/transport layers. On a miss it calls the inner client and tags the
//! response with a [`CommitToken`] carrying the [`CachePayload`] needed to store
//! it; the caller commits that payload only after validation succeeds
//! ([`Cached::commit`]). Invalid responses are never committed, so they are never
//! cached.
//!
//! The [`CacheStore`] trait is the *narrow* seam through which `llm` reaches the
//! database (`docs/ARCHITECTURE.md` §3). It is defined here; the store-backed
//! implementation lives in `store/repo_llm.rs` (store depends on this trait, not
//! the reverse), so `llm` itself never references `store`.

use std::future::Future;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::llm::canonical::canonical_json;
use crate::llm::client::{
    CachePayload, CommitToken, LlmClient, LlmError, LlmRequest, LlmResponse, Usage,
};
use crate::llm::prompts::PROMPT_VERSION;

/// An error from the cache backend (`CacheStore`). Store-agnostic: the
/// store-backed implementation maps its own error into [`CacheError::Backend`],
/// keeping `llm` free of any `store` type (`docs/ARCHITECTURE.md` §3).
#[derive(Debug, Error)]
pub enum CacheError {
    /// The underlying store failed (I/O, SQLite, serialization).
    #[error("cache backend error: {0}")]
    Backend(String),
}

/// A cached response read back from the store: just the stored text and
/// [`Usage`], which is all that is needed to reconstruct an [`LlmResponse`].
#[derive(Debug, Clone, PartialEq)]
pub struct CachedEntry {
    pub response: String,
    pub usage: Usage,
}

/// The narrow persistence seam for the local response cache
/// (`docs/ARCHITECTURE.md` §3). Defined in `llm`; implemented by `store`.
pub trait CacheStore {
    /// Looks up a cached response by key. `Ok(None)` is a miss.
    fn get(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<CachedEntry>, CacheError>> + Send;

    /// Stores (or replaces) a validated response under its key.
    fn put(&self, payload: &CachePayload) -> impl Future<Output = Result<(), CacheError>> + Send;
}

/// Computes the local cache key for a request (`docs/LLM.md` §4): the sha256 of
/// the canonical JSON of `{ model, prompt_version, systemInstruction, contents,
/// generationConfig }`.
///
/// The service tier is **excluded** (it is a separate request field, never part of
/// `generationConfig`), as are the explicit-cache handle and any request ids —
/// none of which are present in the fields hashed here. Using
/// [`canonical_json`](crate::llm::canonical::canonical_json) makes the key
/// byte-stable regardless of field order.
pub fn cache_key(req: &LlmRequest) -> String {
    let value = serde_json::json!({
        "model": req.model,
        "prompt_version": PROMPT_VERSION,
        "systemInstruction": req.system_instruction,
        "contents": req.contents,
        "generationConfig": req.generation_config,
    });
    let canonical = canonical_json(&value);
    let digest = Sha256::digest(canonical.as_bytes());
    hex_encode(&digest)
}

/// Lowercase hex encoding of a byte slice (no external hex dependency).
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a `String` cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The local response cache layer (`docs/LLM.md` §4).
#[derive(Debug, Clone)]
pub struct Cached<T, S> {
    inner: T,
    store: S,
}

impl<T, S> Cached<T, S> {
    /// Wraps `inner` (normally `Retrying<Transport>`) with the cache `store`.
    pub fn new(inner: T, store: S) -> Self {
        Self { inner, store }
    }
}

impl<T, S: CacheStore> Cached<T, S> {
    /// Stores the response carried by `token`, once the caller has validated it
    /// (`docs/LLM.md` §4). A token with no payload (a hit, a dry-run, or a
    /// transport with no cache) is a no-op, so an invalid response — whose token
    /// the caller drops instead of passing here — is never cached.
    ///
    /// This runs one `put`. The pipeline (M5/M6) will instead extract the
    /// [`CachePayload`] from the token and persist it through a store `*_in_tx`
    /// helper, so the cache write lands inside the unit's transaction
    /// (`docs/ARCHITECTURE.md` §6 rule 1); this standalone method is the
    /// out-of-transaction convenience used until then.
    pub async fn commit(&self, token: CommitToken) -> Result<(), CacheError> {
        match token.payload() {
            Some(payload) => self.store.put(payload).await,
            None => Ok(()),
        }
    }
}

impl<T: LlmClient + Sync, S: CacheStore + Sync> LlmClient for Cached<T, S> {
    async fn generate(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let key = cache_key(&req);

        match self.store.get(&key).await {
            Ok(Some(entry)) => {
                // A hit short-circuits the inner stack entirely, so it never
                // triggers a retry (`docs/ARCHITECTURE.md` §5). Nothing left to
                // commit, so the returned token is a no-op.
                let mut usage = entry.usage;
                usage.cache_hit_local = true;
                return Ok(LlmResponse {
                    text: entry.response,
                    usage,
                    commit: CommitToken::noop(),
                });
            }
            Ok(None) => {}
            Err(err) => {
                // A corrupt or unavailable cache must not block a translation:
                // log and fall through to the transport as if it were a miss.
                tracing::warn!("local cache lookup failed ({err}); treating as a miss");
            }
        }

        let model = req.model.clone();
        let response = self.inner.generate(req).await?;

        // Carry the payload so the caller can commit it after validating; the
        // transport's own (no-op) token is replaced here.
        let payload = CachePayload {
            key,
            model,
            prompt_version: PROMPT_VERSION,
            response: response.text.clone(),
            usage: response.usage.clone(),
        };
        Ok(LlmResponse {
            commit: CommitToken::to_store(payload),
            ..response
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::client::{ContentBlock, GenerationConfig, ServiceTier, ThinkingSetting, Usage};
    use crate::llm::fake::Fake;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MemCache {
        map: Arc<Mutex<HashMap<String, CachedEntry>>>,
    }

    impl CacheStore for MemCache {
        async fn get(&self, key: &str) -> Result<Option<CachedEntry>, CacheError> {
            Ok(self.map.lock().expect("lock").get(key).cloned())
        }

        async fn put(&self, payload: &CachePayload) -> Result<(), CacheError> {
            self.map.lock().expect("lock").insert(
                payload.key.clone(),
                CachedEntry {
                    response: payload.response.clone(),
                    usage: payload.usage.clone(),
                },
            );
            Ok(())
        }
    }

    fn request(tier: ServiceTier) -> LlmRequest {
        LlmRequest {
            model: "gemini-flash-latest".to_string(),
            service_tier: tier,
            system_instruction: "sys".to_string(),
            contents: vec![ContentBlock::user("sheet"), ContentBlock::user("lines")],
            generation_config: GenerationConfig {
                max_output_tokens: 128,
                response_mime_type: "application/json".to_string(),
                response_schema: None,
                thinking: ThinkingSetting::Off,
            },
        }
    }

    fn response(text: &str) -> LlmResponse {
        LlmResponse {
            text: text.to_string(),
            usage: Usage {
                prompt_tokens: 10,
                output_tokens: 6,
                ..Usage::default()
            },
            commit: CommitToken::noop(),
        }
    }

    #[test]
    fn cache_key_excludes_service_tier() {
        // AC: M3-AC3 — the key must be identical for flex and standard.
        assert_eq!(
            cache_key(&request(ServiceTier::Flex)),
            cache_key(&request(ServiceTier::Standard))
        );
    }

    #[test]
    fn cache_key_differs_when_contents_differ() {
        // AC: M3-AC3 — contents are part of the hashed set; different batches must
        // not collide. Would fail if `contents` were dropped from the hashed set.
        let mut other = request(ServiceTier::Flex);
        other.contents = vec![ContentBlock::user("sheet"), ContentBlock::user("DIFFERENT")];
        assert_ne!(cache_key(&request(ServiceTier::Flex)), cache_key(&other));
    }

    #[test]
    fn cache_key_differs_when_model_differs() {
        // AC: M3-AC3 — the model is part of the hashed set. Would fail if `model`
        // were dropped from the hashed set.
        let mut other = request(ServiceTier::Flex);
        other.model = "gemini-pro-latest".to_string();
        assert_ne!(cache_key(&request(ServiceTier::Flex)), cache_key(&other));
    }

    #[tokio::test]
    async fn request_differing_in_contents_is_a_miss() {
        // AC: M3-AC3 — committing one request must not satisfy a different one; a
        // batch whose contents differ is a genuine miss that reaches the transport.
        let fake = Fake::new()
            .push_response(response("first"))
            .push_response(response("second"));
        let cached = Cached::new(fake.clone(), MemCache::default());

        let first = cached
            .generate(request(ServiceTier::Flex))
            .await
            .expect("miss");
        cached.commit(first.commit.clone()).await.expect("commit");

        let mut other = request(ServiceTier::Flex);
        other.contents = vec![ContentBlock::user("sheet"), ContentBlock::user("DIFFERENT")];
        let second = cached.generate(other).await.expect("second");

        assert_eq!(fake.call_count(), 2, "a different batch must not hit");
        assert!(!second.usage.cache_hit_local);
        assert_eq!(second.text, "second");
    }

    #[test]
    fn cache_key_is_a_64_char_hex_sha256() {
        let key = cache_key(&request(ServiceTier::Flex));
        assert_eq!(key.len(), 64);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn hit_avoids_the_transport() {
        // AC: M3-AC3 — a hit returns from cache without calling the inner client.
        let fake = Fake::new().push_response(response("cached text"));
        let cached = Cached::new(fake.clone(), MemCache::default());
        let req = request(ServiceTier::Flex);

        let first = cached.generate(req.clone()).await.expect("miss ok");
        assert_eq!(fake.call_count(), 1);
        cached.commit(first.commit.clone()).await.expect("commit");

        let second = cached.generate(req).await.expect("hit ok");
        assert_eq!(fake.call_count(), 1, "a hit must not reach the transport");
        assert_eq!(second.text, "cached text");
        assert!(second.usage.cache_hit_local);
    }

    #[tokio::test]
    async fn invalid_response_is_not_cached() {
        // AC: M3-AC3 — without a commit (validation failed), the same request is
        // still a miss and reaches the transport again.
        let fake = Fake::new()
            .push_response(response("first"))
            .push_response(response("second"));
        let cached = Cached::new(fake.clone(), MemCache::default());
        let req = request(ServiceTier::Flex);

        let _ = cached.generate(req.clone()).await.expect("miss ok");
        // Deliberately do NOT commit — simulating a validation failure.
        let second = cached.generate(req).await.expect("still a miss");
        assert_eq!(
            fake.call_count(),
            2,
            "an uncommitted response is not cached"
        );
        assert_eq!(second.text, "second");
        assert!(!second.usage.cache_hit_local);
    }

    #[tokio::test]
    async fn hit_ignores_service_tier() {
        // AC: M3-AC3 — a flex request committed, then the identical standard
        // request, is a hit (the tier is excluded from the key).
        let fake = Fake::new().push_response(response("tier-independent"));
        let cached = Cached::new(fake.clone(), MemCache::default());

        let first = cached
            .generate(request(ServiceTier::Flex))
            .await
            .expect("miss");
        cached.commit(first.commit.clone()).await.expect("commit");

        let second = cached
            .generate(request(ServiceTier::Standard))
            .await
            .expect("hit");
        assert_eq!(fake.call_count(), 1);
        assert!(second.usage.cache_hit_local);
    }

    #[tokio::test]
    async fn miss_carries_a_committable_payload() {
        let fake = Fake::new().push_response(response("payload text"));
        let cached = Cached::new(fake, MemCache::default());
        let req = request(ServiceTier::Flex);

        let resp = cached.generate(req.clone()).await.expect("miss");
        let payload = resp.commit.payload().expect("miss carries a payload");
        assert_eq!(payload.key, cache_key(&req));
        assert_eq!(payload.model, "gemini-flash-latest");
        assert_eq!(payload.prompt_version, PROMPT_VERSION);
        assert_eq!(payload.response, "payload text");
    }
}
