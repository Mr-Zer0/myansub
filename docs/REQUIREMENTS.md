# Product Requirements — subtrans v1

## 1. Purpose

A command-line tool that translates a subtitle file into Burmese using Google Gemini.
Before translating, it builds a context sheet (premise, characters, how characters address
each other, glossary, scenes). A human can review and correct both the context sheet and
the translation. Every step is crash-safe and resumable, and LLM cost is kept to a minimum.

## 2. Users

A single operator running the CLI locally (Linux, macOS, Windows). A native Burmese reviewer
may edit translations using exported review files.

## 3. Scope

- One project = one source subtitle file = one Burmese translation.
- Target language is fixed to Burmese (`my`), but stored per project for future expansion.
- Source language is given by the user at `init`.

## 4. Functional requirements

### FR-1 Input
- Read SRT, WebVTT, ASS, SSA, and SBV.
- Detect the format from content; use the file extension only as a tie-breaker.
- Accept any text encoding. Auto-detect (BOM → strict UTF-8 → `chardetng`) and allow an
  `--encoding` override. Warn and list affected cues if decoding produced replacement characters.
- Record a SHA-256 of the source file. Refuse to continue if the source changes, unless `--force`.

### FR-2 Project
- `init` creates a project directory containing `project.db` (single source of truth).
- All later commands operate on a project directory (default: current directory).

### FR-3 Analysis
- Produce a context sheet matching the schema in `DATA_MODEL.md` §3.
- Default: a single LLM call over the whole transcript. Fall back to map-reduce above a
  configurable token threshold.
- Store each result as a new, versioned analysis with provenance (source digest, prompt
  version, model).

### FR-4 Analysis review (gate)
- Show, edit (`$EDITOR`), export, import, and approve the analysis.
- Every accepted edit creates a new version. Invalid edits are rejected with field locations.
- `unresolved_ambiguities` are shown first.
- Translation cannot start until the current analysis version is approved.

### FR-5 Translation
- Translate cues in batches using the approved analysis.
- Validate every response (`LLM.md` §8). Cues failing checks are flagged, not dropped.
- Failed batches never block other batches.
- After an analysis edit, only affected batches become stale (`LLM.md` §7).

### FR-6 Translation review (gate)
- List with filters (suspect, failed, stale, ambiguity), set a single cue, edit in `$EDITOR`,
  export/import a review file, approve.
- User-edited cues become `locked` and are never overwritten by re-translation.
- Show counts of suspect / fixed / untouched cues.

### FR-7 Output
- Write SRT, WebVTT, ASS, SSA, or SBV, containing the translation only.
- Output is UTF-8 (optional BOM). Writes are atomic.
- Same-format output preserves original styles and headers; cross-format conversion warns
  about lost information.
- ASS/SSA output applies a configurable Myanmar-capable font to all styles. In practice the
  font is applied to every style in the output's style table, which exists only when the
  output carries stored `FormatMeta` — i.e. a same-format ASS/SSA export (ADR-020). A
  cross-format export *to* ASS/SSA, or a source that was recovered via the fallback parser,
  has no style table, so no font is written and a lost-styling warning is emitted instead
  (consistent with ADR-020/ADR-024).
- Export refuses if any translatable cue lacks a translation, unless `--allow-incomplete`
  (then source text is used and a warning lists the cues).

### FR-8 Configuration
- Models, thinking level, and service tier are configurable per stage, via `config.toml`,
  environment variables, and per-run CLI flags.
- API key via `.env` / environment only.

### FR-9 Cost control
- Local response cache: an identical request is never paid for twice.
- `--dry-run` on `analyze` and `translate` reports estimated tokens and cost without calling the API.
- `status` reports actual tokens (input, cached, output, thinking) and estimated cost.

### FR-10 Status
- `status` shows stage states, batch progress, failures, flags, and usage.

### FR-11 Live progress
- `analyze`, `translate`, and `run` show live progress while the analysis chunks / translation batches
  run, so the operator can see work is advancing and how far along it is (`CLI.md` §8, `ARCHITECTURE.md`
  §11).
- Progress is written to stderr; the final outcome line, dry-run estimates, and `status` stay on stdout.
- On by default; an interactive terminal updates a single line in place, non-TTY/raised-logging prints
  periodic plain lines, and `--no-progress` (or `--dry-run`) suppresses it. Progress never changes exit
  codes or crash-safety.

## 5. Non-functional requirements

| ID | Requirement |
|---|---|
| NFR-1 Crash safety | Killing the process at any moment (including `kill -9`) never corrupts the database or an output file. At most the in-flight API calls are lost. |
| NFR-2 Resumability | Re-running any command continues from where it stopped. Completed work is never redone. |
| NFR-3 Graceful stop | Ctrl-C stops dispatching new work, waits a short grace period, and exits with code 130. |
| NFR-4 Cost | Defaults minimize LLM spend (see `LLM.md` §5). |
| NFR-5 Observability | Structured `tracing` logs, controllable via `RUST_LOG`. Sensitive text only at `trace`. Live progress (FR-11) is a separate human-facing channel on stderr, independent of the log level. |
| NFR-6 Portability | Builds and runs on Linux, macOS, and Windows. No native TLS (rustls only). |
| NFR-7 Testability | Full pipeline testable offline with a fake LLM transport. |

## 6. Out of scope (v1)

- TXT input and output.
- Bilingual output.
- Target languages other than Burmese.
- Series / multi-episode projects and cross-episode memory.
- Gemini Batch API mode.
- Budget caps / hard spend limits.
- Per-cue speaker attribution by the LLM (speakers are inferred during translation).
- GUI, video input, embedded subtitle extraction.

## 7. Glossary

| Term | Meaning |
|---|---|
| Cue | One subtitle event: timing + text. |
| Translatable cue | A cue containing at least one letter after tag removal. |
| Context sheet / analysis | Structured JSON describing the work (`DATA_MODEL.md` §3). |
| Address matrix | For a speaker pair: self-reference, addressee reference, register, particle. |
| Batch | A contiguous group of cues translated in one LLM call. |
| Gate | A review stage that must be approved before the next stage runs. |
| Locked cue | A cue whose translation was edited by the user. |
| Stale cue | A cue translated with an older analysis version that the change affects. |
