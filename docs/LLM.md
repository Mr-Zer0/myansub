# LLM Integration (Gemini)

> Gemini API details (field names, tier parameter, thinking controls) change over time.
> Keep every API-specific name inside `llm/gemini.rs` and verify against the current
> Gemini API docs when implementing. Items marked **[verify]** must be checked.

## 1. Transport

- Endpoint: `POST {base_url}/models/{model}:generateContent`
  (default `base_url = https://generativelanguage.googleapis.com/v1beta`).
- Auth header: `x-goog-api-key: $GEMINI_API_KEY`.
- Structured output: `generationConfig.responseMimeType = "application/json"` plus a response
  schema from `llm/schemas.rs`. **[verify]** whether to use `responseSchema` or `responseJsonSchema`
  for the chosen models.
- Always set `generationConfig.maxOutputTokens` (computed per request, §5).
- Thinking: per-stage setting (`off|minimal|low|medium|high` or a token budget) mapped to the
  model family's control (`thinkingBudget` vs `thinkingLevel`). **[verify]** per model.
- Service tier: `flex` or `standard`, sent with each request. **[verify]** parameter name and
  placement. If a model rejects the tier, log a warning and retry once on `standard`.
- Read `usageMetadata`: `promptTokenCount`, `cachedContentTokenCount`,
  `candidatesTokenCount`, `thoughtsTokenCount`.
- Timeout per request from config. Use one shared `reqwest::Client`.

## 2. Prompt layout (cache-friendly)

Order is fixed. Everything above the line must be byte-identical across all calls of a stage.

```
systemInstruction   static per PROMPT_VERSION: role, Burmese rules, output contract
contents[0] (user)  analysis sheet, canonical JSON (sorted keys, fixed formatting)
─────────────────── cached prefix ends here ───────────────────
contents[1] (user)  variable: scene summary, context lines, lines to translate
```

- Canonical JSON: serialize via `serde_json::Value` with keys sorted recursively. A unit test
  asserts identical bytes for equal documents.
- `PROMPT_VERSION: u32` lives in `prompts.rs`. Bump it on any prompt or schema change.

### Translation variable block

```
SCENE: <summary of the scene overlapping this batch>
SPEAKERS IN SCENE: c1 (name), c2 (name)

CONTEXT BEFORE (reference only, do not translate):
[101] Aoi: …
[102] …

TRANSLATE:
[104] …
[105] …

CONTEXT AFTER (reference only, do not translate):
[188] …
```

The speaker prefix appears only when the cue has a known speaker (ASS Name field).
Non-translatable cues are never sent.

### System instruction for translation (content requirements)

- Translate into natural, spoken Myanmar (Burmese) in Unicode. Never use Zawgyi.
- Choose pronouns, kinship terms, register, and sentence-final particles from the address
  matrix. Infer the speaker from the scene speakers and the dialogue when not given.
- Keep `keep: true` glossary terms unchanged. Use glossary targets consistently.
- Preserve every `⟦n⟧` placeholder exactly once.
- Translate each line only from its own source. Never move text between lines, even when a
  sentence spans several cues.
- Keep lines short enough to read; subtitles are not prose.
- Return only JSON matching the schema: `{"lines":[{"id":…,"t":"…"}]}`.

### Analysis prompt (content requirements)

- Input: full transcript as `[idx] Speaker: text` (or `[idx] text`), tags removed, no timings.
- Output: analysis document (`DATA_MODEL.md` §3). Prose in English; `self_ref`,
  `addressee_ref`, `particle`, and glossary `target` in Burmese.
- Mark gender `unknown` when the text is insufficient. Do not guess.
- List concrete ambiguities that would change the Burmese output.
- Scenes must cover the cue range contiguously.

### Map-reduce analysis (only above `single_pass_max_tokens`)

- Map: each chunk (≈ `analysis.chunk_cues` cues) produces a partial document.
- Reduce: one call merges partials. Partials are passed in chunk order.
- Each chunk is a scheduler unit.

## 3. Batching (translation)

- Batches of `translation.batch_size` translatable cues, contiguous.
- Context: `context_before` / `context_after` source lines (translatable only).
- `input_hash` = sha256 of the variable block + analysis version + model + PROMPT_VERSION.
  A `done` batch whose `input_hash` is unchanged is never resent.
- First batch runs alone (cache warm-up) when `warm_cache_first = true`; then up to
  `concurrency` batches in parallel.

## 4. Local response cache (`llm/cache.rs`)

- Key = sha256 of canonical JSON of:
  `{ model, prompt_version, systemInstruction, contents, generationConfig }`.
  Excluded: service tier, explicit-cache handle (replaced by the hash of the cached content),
  request ids.
- Lookup before every call. A hit returns the stored response with usage marked
  `cache_hit_local`.
- Store only after the caller confirms validity: `generate()` returns a response plus a
  `CommitToken`; the pipeline calls `cache.commit(token)` after validation succeeds, inside
  the unit's transaction where possible. Invalid responses are never cached.
- `subtrans cache prune [--older-than DAYS]` deletes entries.

## 5. Cost strategy

Defaults minimize cost. Each item is configurable.

1. **Flex tier** (`llm.service_tier = "flex"`): roughly half the standard price, synchronous,
   may be shed under load. Sheds are retryable; after `max_sheds_before_fallback` consecutive
   sheds on a unit, switch that unit to `standard` if `service_tier_fallback = true`.
2. **Stable prefix** (§2) so Gemini implicit caching applies.
3. **Full analysis sheet in every call**, not a per-batch subset (cached prefix is cheaper
   than an uncached subset, and gives better context).
4. **Cache warm-up**: first batch alone, then parallel.
5. **Explicit cache** (`cache.provider`):
   - `off` / `implicit`: never create explicit caches.
   - `explicit`: always create one per analysis version when prefix ≥ `min_cache_tokens`.
   - `auto` (default): create only if estimated saving > 0:
     ```
     saving = (batches - 1) * prefix_tokens * input_price
                * (explicit_discount - implicit_hit_rate * implicit_discount)
              - prefix_tokens * storage_price_per_token_hour * est_hours
     ```
     Prices and discounts come from `[pricing]`; `implicit_hit_rate` from config (default 0.5).
   - Record in `provider_cache`. Reuse on resume if not expired; recreate otherwise.
     Delete at stage end. TTL = estimated duration + margin.
   - **[verify]** the system instruction placement rules when using cached content.
6. **Local response cache** (§4).
7. **Thinking off/minimal for translation**, low for analysis (thinking tokens bill as output).
8. **Small outputs**: `{"id","t"}` only; `maxOutputTokens` ≈ source tokens × `output_ratio`
   (config, default 4.0 for Burmese) + overhead.
9. **Small inputs**: no timings, tags masked, lead tags never sent, integer ids,
   non-translatable cues skipped.
10. **Larger batches** (default 80). On validation failure, retry only the failed lines.
11. **Selective invalidation** after analysis edits (§7).
12. **No LLM review pass**: quality checks are deterministic (§8).

## 6. Retries

### Transport retries (`llm/retry.rs`)

Pure policy: `fn decide(err: &LlmError, attempt: u32, cfg: &RetryConfig) -> Decision`
where `Decision = Retry { delay } | FallbackTier | GiveUp`.

| Error | Decision |
|---|---|
| 429 | Retry; delay from `Retry-After` header or `google.rpc.RetryInfo.retryDelay` in error details, else backoff |
| 5xx, timeout, connection error | Retry with backoff |
| Flex shed / capacity error | Retry; after N consecutive → `FallbackTier` |
| 400 (schema/request), 401, 403, 404 | `GiveUp` (fatal) |

Backoff: `min(max_delay, base * 2^attempt)` with full jitter. Cap: `llm.max_retries`.
Sleeps use `tokio::select!` with the cancellation token.

### Content retries (pipeline)

On a transport-successful but invalid response:
1. Unparseable JSON or schema mismatch → retry the whole batch once with a strict reminder.
2. Wrong id set → keep valid lines, retry only missing ids as a new smaller request.
3. Per-line failures (`placeholder_mismatch`, `untranslated`, `zawgyi_suspect`) → retry those
   lines once, isolated, with a strict reminder. If still failing, store the best result and flag it.
4. After `translation.max_content_attempts`, mark the batch `failed` and continue.

Content and transport attempts are counted separately.

## 7. Selective invalidation

When a new analysis version is approved after translations exist, diff old vs new:

| Change | Affected batches |
|---|---|
| `premise`, `tone`, `style_rules` | all |
| glossary entry added/changed/removed | batches whose source contains the term (case-insensitive) |
| character changed (name, aliases, gender, age_group, speech_style) | batches whose source mentions the name/alias, or whose overlapping scene lists the character |
| address_matrix pair changed | batches whose overlapping scene lists both characters |
| scene changed | batches overlapping old or new range |

Affected, unlocked translations get `stale = 1`. `translate --stale` re-translates them after
printing an estimate and asking for confirmation (`--yes` skips).

## 8. Validation (`quality/checks.rs`)

Pure functions over `(source cue, translated text, analysis, limits)`.

| Check | Flag |
|---|---|
| Empty target for a translatable cue | `empty` |
| Placeholder multiset differs from source | `placeholder_mismatch` |
| No Myanmar code point (U+1000–U+109F, U+AA60–U+AA7F, U+A9E0–U+A9FF), unless the source is a `keep` term or a known name | `untranslated` |
| Zawgyi heuristic (e.g. U+1031 at line start or after whitespace; code points U+1060–U+1097 in otherwise Burmese text) | `zawgyi_suspect` |
| Target/source length ratio far outside the batch median while a neighbor is abnormally short | `boundary_drift` |
| Source contains a glossary term but target lacks its `target` | `glossary_miss` |
| Grapheme clusters per line > `max_graphemes_per_line` or lines > `max_lines` | `too_long` |
| Graphemes per second > `max_gps` | `too_fast` |

Thresholds are configurable and must be tuned after native-speaker review.

## 9. Pricing and dry run

- `llm/pricing.rs`: `fn cost(model, tier, &Usage, &PriceTable) -> Cost`. Price table entries:
  input, cached_input, output (thinking billed as output), storage per 1M tokens per hour,
  tier multipliers, optional long-prompt bracket (threshold + alternative prices).
- Unknown model in price table → cost reported as "unknown", never fails the run.
- `--dry-run` swaps the transport for `DryRun`. The real pipeline runs: real batches,
  real prompts, real local-cache lookups (hits are free). Tokens are estimated as
  `ceil(utf8_bytes / bytes_per_token)` per script (config; Burmese is much denser than Latin),
  and output tokens via `output_ratio`. Nothing is written except `llm_calls` rows marked
  `dry_run = 1`, which are deleted at the end of the dry run after the report is printed.
