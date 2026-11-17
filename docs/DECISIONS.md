# Decision Log

Append new decisions at the end. Format: context → decision → consequence.

### ADR-001 Subtitle codecs
`subtitler` 2.x is primary (supports SRT, VTT, ASS/SSA, SBV, auto-detection). `rsubs-lib`
0.4.x is the fallback for SRT, VTT, SSA/ASS. SBV has no fallback: if `subtitler` fails on
SBV, return a typed error. No hand-written parsers.

### ADR-002 TXT out of scope
TXT has no timing and creates conversion edge cases. Deferred.

### ADR-003 SQLite as single source of truth
All state lives in `project.db`. Units commit result + status in one transaction.
WAL + `synchronous=FULL`. Enables crash safety and resumability without extra machinery.

### ADR-004 Single-pass analysis by default
A full movie transcript fits easily in Gemini's context. One call is cheaper and more
consistent than map-reduce. Map-reduce remains for very long inputs.

### ADR-005 No rolling per-batch context
Inspired by AI-SubContext but rejected: per-batch state updates add output tokens, force
sequential batches, and change the prompt prefix every call (defeating caching). Upfront
analysis + scenes replace it.

### ADR-006 Full analysis sheet as stable cached prefix
Sending the whole sheet in every call (cached) is cheaper than an uncached per-batch subset
and gives better context. Requires byte-stable canonical JSON.

### ADR-007 Flex tier by default
Batch translation is not latency-sensitive. Flex roughly halves cost; sheds are retryable
with optional fallback to standard. Configurable per stage and per run.

### ADR-008 Layered LLM client with static dispatch
`Metered<Cached<Retrying<Transport>>>`. Dry run and tests swap only the transport, so
estimates and tests exercise real pipeline code. Trait uses
`-> impl Future<Output = …> + Send` instead of `async-trait`.

### ADR-009 Transport vs content retries
Transport retries (429/5xx/sheds) live in `llm/retry.rs`; content retries (invalid output)
live in the pipeline. Separate counters.

### ADR-010 Quality checks are deterministic and standalone
`quality/` is pure and used by both translation and review. No LLM review pass (cost).

### ADR-011 Synchronous rusqlite behind spawn_blocking
One connection, `Arc<Mutex<Connection>>`, `spawn_blocking` bridge. Parallelism comes from
LLM calls, not DB access.

### ADR-012 Burmese address matrix
Burmese pronouns, kinship terms and particles depend on speaker gender and relationship.
The analysis records them per character pair. Prose fields stay in English to save tokens.

### ADR-013 Speaker inference, not attribution
The translation model infers speakers from scene speaker lists and the address matrix
(no extra tokens). Per-cue attribution may be added later as opt-in.

### ADR-014 Reference projects used for ideas only
llm-subs (GPL-3.0) and AI-SubContext (MIT) informed the analysis schema, block hashing,
validation, and review flow. No code is copied from llm-subs.

### ADR-015 Single target language
Target is Burmese (`my`) for v1 but stored per project to allow future languages.

### ADR-016 Encoding detection
BOM → strict UTF-8 → `chardetng` (with source-language hint) → `encoding_rs` decode.
User override via `--encoding`. Output always UTF-8.

### ADR-017 Project name
The project, crate, and binary are named `myansub`. Environment overrides use the
`MYANSUB_` prefix; the project config file is `myansub.toml`. Chosen over `mm-subtrans`
to avoid confusion with the existing `llm-subtrans` project.

### ADR-018 No job IDs; the project directory is the job
A `--resume <JOB_ID>` option was proposed. Rejected: it adds a second identity system and a
global registry that goes stale when folders move. Instead, `run FILE` derives the project
location (`<FILE>.myansub/`) and creates or resumes it, so the resume command is identical
to the start command. `list` shows unfinished projects.

### ADR-019 Auto-approval with guardrails
`run --auto-approve analysis|translation|all` enables unattended runs. Translation
auto-approval is refused when batches failed unless `--allow-incomplete`. Approval origin
(`user`/`auto`) is recorded. Auto-approval never implies `--yes`; without a TTY, a needed
confirmation exits 3 instead of blocking.

### ADR-020 Output format defaults to input format
Same-format output is the only lossless path (ASS styles survive). `--output-format` and
`--output` override; values given at project creation are stored as project defaults.

### ADR-021 Standard Unicode Myanmar only
The product supports only standard Unicode Burmese; Zawgyi is not supported anywhere. LLMs
can still occasionally emit legacy-ordered text, so a deterministic check
(`invalid_myanmar_unicode`) guards the output and triggers a retry. It enforces the
Unicode-only rule; it is not a conversion feature.

### ADR-022 Codec dependency surface and fallback write path
Context: implementing the `formats::codec` chain (M1-B). `subtitler` 2.x defaults pull in
`cli` (clap, tracing-subscriber) and `http` (reqwest); `rsubs-lib` 0.4.x stores timestamps
as `time::Time` and offers no public constructor for them, and `time` is not an approved
dependency. Decisions: (1) depend on `subtitler` with `default-features = false` and only
`srt, vtt, ass, ssa, sbv` to keep the build lean and avoid duplicate HTTP/CLI stacks; (2) the
`rsubs-lib` adapter is a *parse-only* fallback — its `write` delegates to the `subtitler`
codec, since `subtitler` serializes every accepted format and a parsed `Document` is
codec-agnostic. This avoids both adding `time` and hand-writing a serializer. Consequence:
`rsubs-lib` timestamps are read via the `time::Time` component accessors (through a local
macro, without naming the transitive type); ASS/SSA recovered via the fallback carry no
`format_meta` (styles would be in `rsubs-lib`'s shape, not `subtitler`'s), so a same-format
ASS write after a fallback parse uses default styling. This path is only reached when the
primary parser rejects the input, which does not occur for the supported fixtures.

### ADR-023 FormatMeta stores ASS metadata as JSON blobs
Context: `FormatMeta` has `script_info` and `styles` string fields; `DATA_MODEL.md` describes
them as verbatim section text. The `subtitler` model exposes ASS `[Script Info]` as a
`HashMap` and styles as a typed `Vec<AssStyle>`, not verbatim text. Decision: the `subtitler`
adapter serializes those structured values to JSON into the two `FormatMeta` fields and
deserializes them on write. Consequence: same-format ASS/SSA output reproduces the script
info and full style table (M1-AC2) without re-parsing section text; the stored strings are
JSON, not the original section bytes. `[Script Info]` key order is not preserved (HashMap),
which is cosmetic.

### ADR-024 ASS/SSA text-fidelity overlay (amends ADR-001)
Context: probing the crates revealed that `subtitler` 2.13 corrupts ASS/SSA dialogue text at
parse time — it splits each `Dialogue:` line on commas and trims each piece, dropping the
space after every comma (`"Hello, world, and goodbye"` → `"Hello,world,and goodbye"`). This
hits the *source* text the translator and LLM see, for a first-class format. `subtitler`'s
writer is fine, and `rsubs-lib` parses the same dialogue faithfully (commas, spaces, and
inline tags like `{\i1}Wait{\i0}, where…`) provided the ASS has complete V4+ style headers.
Decision (orchestrator-approved; product-owner review deferred): keep `subtitler` as the
primary structural parser *and* writer — timings, styles, speaker, `format_meta`, and
detection all stay `subtitler`'s (so M1-AC2 is unaffected) — and add a deterministic
text-fidelity overlay for ASS/SSA only. `parse_document` parses with `subtitler`, re-parses
the same input with the `rsubs-lib` codec, and, only when both agree on cue count and per-cue
timings, replaces each cue's `text_raw` with the faithful `rsubs-lib` text and re-runs
`markup::mask`. If `rsubs-lib` fails or the parses do not align, the `subtitler` text is kept
and a single `tracing::warn!` is emitted; the parse never fails. The overlay lives in
`formats::codec` (not the `subtitler` adapter) to avoid a `subtitler`↔`rsubs` module cycle and
to keep each adapter a pure single-crate wrapper. Consequence: ASS/SSA source text is faithful
for files with complete V4+ headers; legacy v4 SSA that `rsubs-lib` cannot parse keeps
`subtitler`'s comma-corrupted text (graceful, with a warning). SRT/VTT/SBV are unchanged
(`subtitler` only). No hand-written parsing — both parses go through the crates.

### ADR-025 Gemini structured output uses `responseJsonSchema`
Context: `docs/LLM.md` §1 leaves open ([verify]) whether to send the response schema as
`generationConfig.responseSchema` or `generationConfig.responseJsonSchema`. Verified against the
live Gemini `generateContent` API reference (checked 2026-11, Gemini 3.x Flash era): the reference
documents `responseJsonSchema` (full JSON Schema) as the field to use and marks the older
`responseSchema` as "an internal detail. Use responseJsonSchema rather than this field." Decision:
model the schemas in `llm/schemas.rs` using only keywords valid in both dialects (`type`,
`properties`, `items`, `required`, `enum`, `maxItems`, `maxLength`, `minItems`) and have the
transport (`gemini.rs`, M3-B) send them under `responseJsonSchema`. Consequence: the modeled
`serde_json::Value` is compatible either way; only the wire field name in `gemini.rs` is affected,
and it should be `responseJsonSchema`. Bump `PROMPT_VERSION` if the schema shape later changes.

### ADR-026 Retry policy takes the jitter factor as a parameter
Context: `docs/LLM.md` §6 requires full jitter on backoff but the policy must be a pure function
whose exact delays unit tests can assert (M3-AC2). `docs/LLM.md` §6 permits either passing a drawn
jitter factor into `decide` or returning the ceiling plus a separate `jittered(ceiling, rng)` helper.
Decision: `decide(err, attempt, consecutive_sheds, cfg, jitter: f64)` takes the full-jitter factor
(`[0,1)`) and returns the final `Retry { delay }`; a server-provided `Retry-After`/`RetryInfo` delay
on a 429 is honored exactly and is not jittered. The `Retrying` layer (M3-B) draws the factor from a
local `Xorshift` PRNG (no `rand`, per ARCHITECTURE §10) seeded from the system clock, with the seed
injectable for tests. `RetryConfig` is a plain struct (not the config type) so the policy stays pure
and independent of `config`. Consequence: tests assert exact delays by passing a fixed factor; the
layer owns the one impure step (drawing jitter and sleeping).
