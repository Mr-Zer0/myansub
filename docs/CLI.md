# CLI Reference

Binary name: `myansub`.

## 1. Global options

| Option | Meaning |
|---|---|
| `-p, --project <DIR>` | Project directory (overrides project resolution, §2) |
| `-c, --config <FILE>` | Extra config file, highest file precedence |
| `-v` / `-vv` | Raise log level (overridden by `RUST_LOG`) |
| `--yes` | Answer yes to cost and destructive-action confirmations |
| `--no-progress` | Suppress the live progress display (§8) |

## 2. Project resolution

There are no job IDs. The project directory is the job, and every command resumes it.

1. `--project DIR` given → use `DIR`.
2. Command takes a source `FILE` (`init`, `run FILE`) → `<FILE>.myansub/` next to the source
   (e.g. `movie.ja.srt` → `movie.ja.srt.myansub/`).
3. Otherwise → the current directory if it contains `project.db`; else the single
   `*.myansub/` project in the current directory; else exit 3 listing the candidates.

`run FILE` creates the project if it does not exist (same as `init`, so `--from` is then
required) and resumes it if it does. If the project exists and `--from` disagrees with the
stored source language, exit 3. If the source file changed, exit 3 and point to `init --force`.

## 3. Output options

Accepted by `init`, `run`, and `export`.

| Option | Meaning |
|---|---|
| `--output-format <srt\|vtt\|ass\|ssa\|sbv>` | Output format. Default: same as input. |
| `-o, --output <PATH>` | Output path. Default: `<source-stem>.my.<ext>` next to the source. |

- Precedence: flag on the current command > value stored in the project > default.
- `init` (and `run FILE` when it creates the project) store the given values in the project.
  On later commands, flags apply to that invocation only.
- If `--output` has a known subtitle extension and `--output-format` is absent, the format is
  taken from the extension. If both are given and disagree, exit 2.
- Cross-format output warns about lost styling.

## 4. Approval options

Accepted by `run`.

| Option | Meaning |
|---|---|
| `--auto-approve <analysis\|translation\|all>` | Approve the named gate(s) automatically when reached |
| `--allow-incomplete` | Permit auto-approving translation and exporting with failed units |

- Auto-approving translation is refused (exit 3) if any batch is `failed`, unless
  `--allow-incomplete`. Suspect cues do not block it; their count is printed.
- Approvals are recorded with origin `auto` or `user`; `status` shows the origin.
- `--auto-approve` never implies `--yes`. Cost confirmations still apply.

## 5. Non-interactive behavior

If a confirmation would be shown and stdin is not a TTY and `--yes` was not given, exit 3
with a message naming the flag needed. Never block waiting for input.

## 6. Commands

```
myansub init <FILE> --from <LANG> [--encoding <ENC>] [--output-format F] [-o PATH] [--force]
    Create the project, decode, parse, mask, store cues, store output preferences.
    --force: re-initialize when the source changed (destroys project data after confirmation).

myansub run [FILE] [--from <LANG>] [--encoding <ENC>] [--output-format F] [-o PATH]
            [--auto-approve analysis|translation|all] [--allow-incomplete] [--dry-run]
    Create or resume the project and advance stages. Stops at the first gate that is
    neither approved nor auto-approved and prints the next action. With both gates passed,
    it ends by exporting the output.

myansub list [DIR]
    List projects in DIR (default: current directory): source, stage states, approval origin,
    failed/suspect counts.

myansub status
    Stage states with approval origin, unit progress, failures, flag counts, token usage,
    and estimated cost.

myansub analyze [--model M] [--tier T] [--redo] [--retry-failed] [--dry-run]
    --redo: produce a new analysis version even if a current one exists.

myansub analysis show [--json]
myansub analysis edit
myansub analysis export <PATH>
myansub analysis import <PATH>
myansub analysis approve

myansub translate [--model M] [--tier T] [--concurrency N]
                  [--stale] [--redo] [--retry-failed] [--dry-run]
    --stale: re-translate stale, unlocked cues.
    --redo:  re-translate all unlocked cues.

myansub review list [--suspect] [--failed] [--stale] [--flag F] [--range A-B]
myansub review show <ID>
myansub review set <ID> <TEXT>
myansub review unlock <ID>
myansub review edit [--suspect | --range A-B]
myansub review export <PATH> [--suspect | --range A-B]
myansub review import <PATH>
myansub review approve

myansub export [--output-format F] [-o PATH] [--allow-incomplete]

myansub cache prune [--older-than DAYS]

myansub models list [--provider gemini|openrouter] [FILTER]
    List available models with id, input/output price per 1M tokens, context length, and
    structured-output support. --provider filters to one provider (default: both). FILTER is a
    case-insensitive substring match on the id. Gemini entries come from the [pricing] table;
    OpenRouter entries from its cached /models catalog (LLM.md §10.9).
```

**Model references.** `--model M` (on `analyze` / `translate`) and the `[llm.models]` config keys
accept `[provider:]model` (providers `gemini`, `openrouter`). A reference with no prefix uses
`llm.default_provider` (default `gemini`). Example: `--model openrouter:anthropic/claude-sonnet-5.5`.
An unknown provider prefix, or (for OpenRouter) an unknown model id, exits 2 — the id error lists up
to 5 closest ids (`LLM.md` §10.9).

Commands that spend tokens print an estimate and ask for confirmation when the estimate
exceeds `confirm_above_usd` (config), unless `--yes`.

### Typical unattended use

```bash
myansub run movie.ja.srt --from ja --auto-approve all --yes
# If interrupted, the identical command resumes.
```

## 7. Exit codes

| Code | Meaning |
|---|---|
| 0 | Success (including "nothing to do") |
| 1 | Error |
| 2 | Usage error (clap, conflicting flags) |
| 3 | Precondition not met (gate not approved, source changed, ambiguous project, confirmation needed without TTY) |
| 4 | Completed with failed units (see `status`) |
| 130 | Interrupted (Ctrl-C) |

## 8. Progress output

`analyze`, `translate`, and `run` show live progress while the analysis chunks / translation batches run
(ADR-046, `ARCHITECTURE.md` §11). It answers "is it working, and how far along?" during the minutes a stage
can take.

- **Where.** Progress is written to **stderr**. The final outcome line, `--dry-run` estimates, and the
  `status` report stay on **stdout**, so piping stdout to a file or a parser is unaffected by progress.
- **On by default.** On an interactive terminal a single line updates in place, e.g.
  `translating  8/12 batches · 67% · 0m42s`. For `run` the stage is named (analysis → translation → export).
  On a resume, already-done units are not recounted, so the total reflects remaining work.
- **Non-TTY / raised logging.** When stderr is not a terminal (pipe, CI) or `-v`/`-vv`/`RUST_LOG` raise the
  log level, progress degrades to throttled plain lines (`translation: 8/12 batches done`) instead of an
  in-place line.
- **Off.** `--no-progress` suppresses it entirely. `--dry-run` shows no progress (it runs no units).
- Progress never changes exit codes, and Ctrl-C still stops promptly and exits 130.
