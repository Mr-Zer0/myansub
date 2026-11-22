# Architecture

## 1. Overview

```
            ┌────────┐   ┌──────────┐   ┌────────────────┐   ┌─────────────┐   ┌───────────────────┐   ┌────────┐
 file  ───► │ Input  │──►│ Analysis │──►│ AnalysisReview │──►│ Translation │──►│ TranslationReview │──►│ Output │──► file
            └────────┘   └──────────┘   └────────────────┘   └─────────────┘   └───────────────────┘   └────────┘
                                          gate: approve                          gate: approve
```

- `project.db` (SQLite) is the single source of truth. Stages read from it and write to it.
  No stage keeps state only in memory.
- LLM work is split into small **units** (analysis chunks, translation batches). Each unit
  commits its result and its status change in one transaction.
- Any command can be killed and re-run. The orchestrator works out what is left to do.

## 2. Stage state machine

Stage status values: `not_started → running → completed → approved` (`approved` only for
the two review stages).

| Stage | May run when | Notes |
|---|---|---|
| input | always (`init`) | Re-init requires `--force` if the source hash changed |
| analysis | input completed | |
| analysis_review | analysis completed | Approve = mark current analysis version approved |
| translation | current analysis version approved | |
| translation_review | translation completed (no pending units) | Failed units allowed; they are listed |
| output | translation_review approved | |

Editing the analysis creates a new, unapproved version. The analysis_review stage then
returns to `completed`, and translation is blocked until the new version is approved.

`run` advances through stages until it reaches a gate that is not approved, then stops and
prints the next action for the user.

## 3. Module layout

```
src/
  main.rs               # anyhow, tracing init, dotenvy, clap dispatch, exit codes
  lib.rs
  cli.rs                # clap derive definitions only
  config.rs             # config.toml + env + CLI override resolution
  error.rs              # re-exports module error enums
  domain/               # pure types: Cue, Document, Analysis, ProjectState, StageStatus
  formats/
    codec.rs            # trait SubtitleCodec, codec selection chain
    subtitler.rs        # primary codec adapter
    rsubs.rs            # fallback codec adapter (srt, vtt, ass/ssa)
    encoding.rs         # byte decoding (BOM, utf-8, chardetng, encoding_rs)
    markup.rs           # mask / unmask tags; lead-tag extraction
  store/
    db.rs               # connection, pragmas, async bridge
    migrations.rs       # versioned migrations via PRAGMA user_version
    repo_project.rs  repo_cues.rs  repo_analysis.rs  repo_units.rs
    repo_translations.rs  repo_llm.rs
  llm/
    client.rs           # trait LlmClient, LlmRequest, LlmResponse, Usage (provider-neutral)
    router.rs           # Router: LlmClient that dispatches by model-ref provider (M13, ADR-051c)
    gemini.rs           # Gemini reqwest transport: request/response mapping only
    openrouter.rs       # OpenRouter reqwest transport: request/response mapping only (M13)
    catalog.rs          # trait ModelCatalog + GeminiCatalog / OpenRouterCatalog (M13, ADR-051f)
    retry.rs            # pure transport retry policy + Retrying layer
    cache.rs            # local response cache layer (provider in the key)
    context_cache.rs    # Gemini explicit cache lifecycle
    metered.rs          # usage recording layer
    dry_run.rs          # bottom layer that records and estimates instead of sending
    fake.rs             # scripted transport for tests (cfg(test) or feature "testing")
    pricing.rs          # pure cost functions (prices read through ModelCatalog)
    prompts.rs          # prompt builders, PROMPT_VERSION
    schemas.rs          # response schemas (shared by both providers)
  quality/
    checks.rs           # deterministic checks; used by pipeline and review
  pipeline/
    orchestrator.rs     # stage state machine, gates, `run`
    scheduler.rs        # generic bounded WorkUnit runner
    progress.rs         # `trait Progress` sink + `NoopProgress` (rendered by main, §11)
    input.rs  analysis.rs  translation.rs  output.rs
  review/
    editor.rs           # export → $EDITOR → validate → import
    analysis.rs
    translation.rs
```

### Dependency direction

```
main/cli ─► pipeline, review ─► llm, quality, store, formats ─► domain
```

- `domain` depends on nothing in the crate.
- Nothing depends on `pipeline` or `review`.
- The `Progress` sink trait (`pipeline/progress.rs`, §11) is defined in `pipeline`; `main` implements it
  (`StderrProgress`). So progress keeps the `main ─► pipeline` edge and never reverses it: `pipeline` and
  the scheduler depend only on the trait, not on `main`/`cli`.
- `quality` must not depend on `llm` or `store` (pure functions over domain types).
- `llm` must not depend on `pipeline`. It may depend on `store` only inside `cache.rs` and
  `metered.rs`, through a narrow trait (`CacheStore`, `UsageSink`) defined in `llm`, and inside
  `catalog.rs`, which reuses the leaf `store::atomic` write helper for the OpenRouter catalog
  cache (ADR-052b — a crash-safe file write, not a DB coupling; no new crate).

## 4. Key responsibilities

**scheduler.rs** — Generic over `trait WorkUnit`. Knows nothing about stages.
- Resets units left `in_flight` to `pending` at start.
- Runs units with a `tokio::sync::Semaphore` bound (`concurrency`).
- Optionally runs the first unit alone (cache warm-up), then fans out.
- Honors a `CancellationToken`: stops dispatching, waits `grace_secs`, returns.
- Reports a summary (done / failed / cancelled).
- Reports **live progress** to an injected `&dyn Progress` (§11): the runnable `total` once known,
  then one tick per finished unit. Progress is observe-only and never affects the `Summary` or
  cancellation (ADR-046).

**orchestrator.rs** — Owns stage transitions and gate checks. Builds the units for a stage
and hands them to the scheduler. Implements `run`. Brackets each executable stage with
`Progress` stage-start/finish events (§11) so a `run` shows analysis → translation → export distinctly.

**retry.rs** — Transport retries only (see `LLM.md` §6). Content retries (validation
failures) live in `pipeline/translation.rs` and `pipeline/analysis.rs`. Never share a counter.

**pricing.rs** — Pure: `(model, tier, Usage, &PriceTable) -> Cost`. No I/O.

## 5. LLM client layer stack

All layers implement the same `LlmClient` trait.

```
pipeline ──► Metered ──► Cached ──► Retrying ──► Transport
                                                  ├─ Router   (normal; dispatches by provider)
                                                  │    ├─ Gemini
                                                  │    └─ OpenRouter
                                                  ├─ DryRun   (--dry-run)
                                                  └─ Fake     (tests)
```

- **Metered** (outermost): writes one `llm_calls` row per logical request, including local
  cache hits (cost 0). Records the `provider` (M13).
- **Cached**: looks up / stores in `llm_cache`. Stores only responses that the caller marked
  valid (see `LLM.md` §4). The full `provider:model` is in the key, so two providers never
  collide. Sits outside Retrying so a hit never triggers retries.
- **Retrying**: applies the transport retry policy. Operates on `ErrorClass` only, so it is
  provider-agnostic; each transport maps its own statuses via `classify` (`LLM.md` §6/§10).
- **Transport**: there is no `enum Transport` — each transport is a struct implementing
  `LlmClient` (static dispatch, ADR-008/051c). A real run wraps a single **`Router`** (which holds
  the per-provider transports it has keys for and delegates `generate` by the request's model-ref
  provider) so both providers share one `Metered<Cached<Retrying<_>>>` stack. `DryRun` (`--dry-run`)
  and `Fake` (tests) are swapped in whole in place of the `Router`.

Static dispatch: layers are generic structs, e.g. `Metered<Cached<Retrying<Transport>>>`.
Define the trait method as
`fn generate(&self, req: LlmRequest) -> impl Future<Output = Result<LlmResponse, LlmError>> + Send`
so futures can be spawned on the multi-threaded runtime.

## 6. Crash-safety rules

1. **One unit, one transaction.** A unit's result and its `done` status are written in the
   same transaction. Never write a result outside that transaction.
2. **Recover on start.** `in_flight` units are reset to `pending`.
3. **SQLite settings:** `journal_mode=WAL`, `synchronous=FULL`, `foreign_keys=ON`,
   `busy_timeout=5000`. Migrations are versioned with `PRAGMA user_version` and run in a transaction.
4. **Atomic file writes.** Write `<path>.tmp`, `fsync`, rename over target, `fsync` the
   directory where supported. Applies to exports, review files, and outputs.
5. **Source hash check** on every command that reads cues.
6. **Cancellation.** Ctrl-C triggers the `CancellationToken`. Backoff sleeps and waits use
   `tokio::select!` against it.
7. **Imports are all-or-nothing.** A review file import validates fully, then applies in one transaction.

## 7. Concurrency and the database

- `rusqlite` is synchronous. `store::db::Store` holds one `rusqlite::Connection` behind
  `Arc<std::sync::Mutex<_>>` and exposes `async fn call<F, T>(&self, f: F)` that runs `f`
  in `tokio::task::spawn_blocking`.
- Keep closures short: no network I/O inside a DB closure.
- One writer is sufficient; parallelism comes from concurrent LLM calls, not DB access.

## 8. Errors

- One `thiserror` enum per module: `FormatError`, `EncodingError`, `StoreError`, `LlmError`,
  `QualityError`, `PipelineError`, `ReviewError`, `ConfigError`.
- `LlmError` exposes `fn class(&self) -> ErrorClass { Retryable, Shed, RateLimited { retry_after }, Fatal }`.
- `PipelineError` and `ReviewError` wrap lower errors with `#[from]`.
- `main.rs` converts to `anyhow`, adds context, and maps to exit codes (`CLI.md` §3).

## 9. Logging

- `tracing` with `tracing-subscriber` env-filter. Default `info`; `RUST_LOG=subtrans=debug`.
- Spans: `stage{name}`, `unit{kind,id}`, `llm_call{model,tier}`.
- `trace` only: prompts, responses, subtitle text. Never log API keys or full request headers.

## 10. Dependencies (approved)

| Crate | Purpose |
|---|---|
| `tokio` (full) | Async runtime |
| `tokio-util` | `CancellationToken` |
| `clap` (derive) | CLI |
| `subtitler` 2.x | Primary subtitle codec |
| `rsubs-lib` 0.4.x | Fallback codec (SRT, VTT, SSA/ASS) |
| `reqwest` (`default-features = false`, `json`, `rustls-tls`) | HTTP |
| `serde` (derive), `serde_json`, `toml` | Serialization |
| `rusqlite` (`bundled`) | Persistence |
| `thiserror`, `anyhow` (main only) | Errors |
| `tracing`, `tracing-subscriber` (`env-filter`) | Logging |
| `dotenvy` | `.env` loading |
| `encoding_rs`, `chardetng` | Non-UTF-8 input |
| `unicode-segmentation` | Grapheme-based length checks |
| `sha2` | Hashing (source digest, cache keys) |

Dev-dependencies (approved): `tempfile`, `wiremock`.

Note: `subtitler` versions 0.0.x are an unrelated older codebase. Always use 2.x.
Do not add `native-tls`, `openssl`, `chrono`, `async-trait`, or `rand`. Backoff jitter uses
a small local xorshift seeded from the system clock.

M13 (OpenRouter) adds **no new crate**: the second transport reuses `reqwest`; the OpenRouter
catalog's 24 h disk cache resolves the user cache dir by hand (like `config.rs` resolves the config
dir) and writes via `store::atomic`, judging freshness from the file mtime (no `dirs`, no `chrono`).
API keys stay env-var only — no keychain crate (PBI-008 deferred, ADR-052).

No progress-bar/spinner crate (`indicatif` et al.) is approved: progress (§11) is hand-rolled on `std`
(stderr writes, `std::time::Instant`, `std::io::IsTerminal`).

## 11. Progress reporting (ADR-046)

The long-running stages (`analyze`, `translate`, and both inside `run`) report live progress so an
operator can see work advancing. Logging (§9) stays the structured, `RUST_LOG`-controlled channel;
progress is a separate, human-facing display.

- **Seam.** `pipeline/progress.rs` defines `trait Progress: Send + Sync` and a zero-cost `NoopProgress`.
  The trait carries stage lifecycle + per-unit events, e.g.:
  - `stage_start(stage, planned)` — a stage's unit work is about to run (`planned` = batches/chunks, if known).
  - `stage_units(total)` — the scheduler's runnable total (after skipping already-`done` units on a resume).
  - `unit_finished(outcome)` — one unit completed (`Done`/`Failed`), called from the unit's task.
  - `stage_finish(summary)` — the stage's units finished; carries the `Summary` (done/failed/cancelled).
  The scheduler takes `progress: &dyn Progress`; `run_analysis`/`run_translation`/`orchestrator::run`
  thread it down and emit the stage boundaries. Tests pass `&NoopProgress`.

- **Rendering (main only).** `main` owns `StderrProgress`. It keeps counters + a start `Instant` behind a
  `Mutex`, and writes to **stderr** (never stdout — see the channel split below). It never calls into
  `store`/`llm`, never holds its lock across an `.await`, and ignores stderr write errors, so it cannot
  stall or abort a unit.

- **Default + modes (ADR-046(d)).** On by default; the global `--no-progress` flag selects `NoopProgress`.
  `StderrProgress` renders an in-place carriage-return line (label · `done/total` · percent · elapsed)
  only when `stderr` is a TTY and verbosity is default; under a pipe/CI or raised logging (`-v`/`RUST_LOG`)
  it prints throttled plain lines instead. Renders are throttled; stage finish terminates the line with a
  newline so it never collides with the stdout outcome line. `--dry-run` runs no units, so it is silent.

- **Channel split.** Progress → stderr; the final outcome line, dry-run estimate, and `status` report →
  stdout. Cancellation is unaffected (observe-only; a post-grace tick is dropped), so Ctrl-C still exits 130.
