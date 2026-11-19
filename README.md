# myansub

A command-line tool that translates subtitle files into **Burmese (`my`)** using Google
Gemini. Before translating, it builds a *context sheet* — premise, characters, how characters
address one another, a glossary, and scene summaries — so the translation stays consistent in
register and terminology. A human can review and correct both the context sheet and the
translation. Every step is **crash-safe and resumable**, and LLM cost is kept to a minimum.

```
Input → Analysis → AnalysisReview → Translation → TranslationReview → Output
                   └─ gate ─┘                      └─ gate ─┘
```

The two gates are human review points: translation can't start until the analysis is approved,
and export won't finish until the translation is approved (unless you auto-approve).

## Features

- **Formats in/out:** SRT, WebVTT, ASS, SSA, SBV. Format detected from content; any text
  encoding auto-detected (with an `--encoding` override).
- **Context-aware translation:** a versioned analysis drives the translation for consistent
  names, terms, and speech register.
- **Human-in-the-loop:** review, edit (`$EDITOR`), export/import, and approve both the analysis
  and the translation. User-edited cues are *locked* and never overwritten.
- **Crash-safe & resumable:** `project.db` (SQLite) is the single source of truth. Kill the
  process at any moment — re-run the same command and it continues where it stopped.
- **Cost control:** a local response cache means an identical request is never paid for twice;
  `--dry-run` estimates tokens/cost without calling the API; `status` reports actual usage.

## Requirements

- Rust (2024 edition, latest stable) to build.
- A Google **Gemini API key** (not needed for `--dry-run`). The free tier works for testing;
  heavier use may require billing.

## Install

Build an optimized binary and put it on your `PATH`:

```bash
cargo install --path .
myansub --help
```

Or build in place:

```bash
cargo build --release
./target/release/myansub --help
```

## Configuration

**API key** (secret) — supplied via the environment only, never written to config, the DB, or
logs:

```bash
cp .env.example .env     # then edit .env and set GEMINI_API_KEY=...
# or:
export GEMINI_API_KEY=your-key-here
```

`.env` is loaded automatically at startup (from the current directory). `.env` is gitignored.

**Settings** (non-secret) — models, thinking level, service tier, caching, cost confirmation,
etc. Resolution order, later wins:

1. Built-in defaults
2. Global: `<config_dir>/myansub/config.toml`
3. Project: `myansub.toml` in the working directory
4. `--config <FILE>`
5. Environment variables (`MYANSUB_*`)
6. CLI flags

Copy the fully-documented template to get started — every key shows its default:

```bash
cp docs/config.example myansub.toml
```

See [`docs/CONFIG.md`](docs/CONFIG.md) for the full reference.

## Quick start

A project *is* its directory — there are no job IDs. `init`/`run FILE` create
`<FILE>.myansub/` next to the source; every later command resumes it.

Unattended, end to end:

```bash
myansub run movie.ja.srt --from ja --auto-approve all --yes
# If interrupted, the identical command resumes.
```

With human review at each gate:

```bash
myansub init movie.ja.srt --from ja   # decode, parse, store cues

myansub analyze                       # build the context sheet
myansub analysis edit                 # review / correct it
myansub analysis approve              # gate 1

myansub translate                     # translate in batches
myansub review list --suspect         # inspect flagged cues
myansub review edit --suspect         # fix them in $EDITOR
myansub review approve                # gate 2

myansub export                        # write movie.ja.my.srt
```

`status` shows stage states, progress, failures, flag counts, token usage, and estimated cost
at any time.

## Commands

| Command | Purpose |
|---|---|
| `init <FILE> --from <LANG>` | Create the project: decode, parse, mask, store cues and output prefs |
| `run [FILE]` | Create or resume the project and advance stages (exports when both gates pass) |
| `list [DIR]` | List projects in a directory |
| `status` | Stage states, progress, failures, usage, estimated cost |
| `analyze` | Build the analysis context sheet |
| `analysis show\|edit\|export\|import\|approve` | Inspect and edit the analysis (gate 1) |
| `translate` | Translate cues in batches |
| `review list\|show\|set\|unlock\|edit\|export\|import\|approve` | Review and edit translations (gate 2) |
| `export` | Write the translated subtitle file |
| `cache prune` | Manage the local response cache |

Run `myansub <command> --help` for full options. Commands that spend tokens print an estimate
and ask for confirmation above `confirm_above_usd` (config), unless `--yes`. Full details in
[`docs/CLI.md`](docs/CLI.md).

### Exit codes

`0` success · `1` error · `2` usage error · `3` precondition not met (e.g. gate not approved,
source changed, confirmation needed without a TTY) · `4` completed with failed units · `130`
interrupted.

## Output

- Writes the translation only, as SRT/VTT/ASS/SSA/SBV. Default path: `<source-stem>.my.<ext>`
  next to the source. Output is UTF-8, written atomically.
- Same-format output preserves original styles/headers; cross-format conversion warns about
  lost styling. ASS/SSA output applies a configurable Myanmar-capable font.

## Development

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

All three must pass before a change is complete. Tests never touch the network (a fake LLM
transport / `wiremock` is used). Design docs live in [`docs/`](docs/) — start with
[`REQUIREMENTS.md`](docs/REQUIREMENTS.md) and [`ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Out of scope (v1)

TXT input/output, bilingual output, target languages other than Burmese, multi-episode
projects, the Gemini Batch API, hard spend caps, and GUI/video/embedded-subtitle extraction.
