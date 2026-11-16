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
