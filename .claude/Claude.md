# myansub

Rust CLI that translates subtitle files into Burmese (`my`) using Gemini, through a
crash-safe, resumable pipeline with two human review gates:

Input → Analysis → AnalysisReview → Translation → TranslationReview → Output

## Read before working

| Doc | When |
|---|---|
| `docs/REQUIREMENTS.md` | What to build, what is out of scope |
| `docs/ARCHITECTURE.md` | Module layout, dependency rules, crash-safety rules |
| `docs/DATA_MODEL.md` | SQLite schema, analysis JSON, review file formats |
| `docs/LLM.md` | Gemini integration, prompts, caching, retries, cost |
| `docs/CLI.md` / `docs/CONFIG.md` | User-facing surface |
| `docs/MILESTONES.md` | Current milestone and its acceptance criteria |
| `docs/TESTING.md` | Test strategy and fixtures |
| `docs/DECISIONS.md` | Why things are the way they are |

Work one milestone at a time. Check acceptance criteria before calling it done.

## Commands

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

All three must pass before any change is complete.

## Non-negotiables

- Rust 2024 edition, latest stable. Only dependencies listed in `docs/ARCHITECTURE.md`.
  Adding any other crate needs product-owner approval — ask, don't add.
- `thiserror` in library code; `anyhow` only in `src/main.rs`.
- No `unwrap()`/`expect()`/`panic!` outside tests, except on proven invariants with a comment.
- Never write a subtitle parser. Use `subtitler`, fall back to `rsubs-lib`.
- Every persisted state change happens inside one SQLite transaction (see crash-safety rules).
- All LLM traffic goes through the `LlmClient` layer stack. No direct HTTP from pipeline code.
- Respect the module dependency direction in `docs/ARCHITECTURE.md`.
- Tests never touch the network. Use the fake transport or `wiremock`.
- Never log API keys. Subtitle text and prompts are logged at `trace` level only.
- Prompt changes bump `PROMPT_VERSION` (it is part of every cache key).

## When docs are unclear or wrong

Stop and ask rather than guess. If you make a deliberate deviation, record it in
`docs/DECISIONS.md` in the same change.

