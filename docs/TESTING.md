# Testing

## 1. Principles

- No test touches the network. Use `llm::fake` (scripted responses) or `wiremock`.
- Pure modules (`quality`, `llm::retry` policy, `llm::pricing`, `formats::markup`,
  canonical JSON) get dense unit tests.
- Every crash-safety rule in `ARCHITECTURE.md` §6 has at least one test.
- Use `tempfile` for project directories. Never write into the repo during tests.

## 2. Layout

```
tests/
  fixtures/
    formats/        # srt, vtt, ass, ssa, sbv samples incl. tags, multiline, edge timings
    encodings/      # same content in shift_jis, gbk, cp1252, utf-16le/be, utf-8-bom
    burmese/        # valid Unicode, Zawgyi samples, mixed-script lines
    llm/            # scripted Gemini responses (valid, malformed, partial ids)
  formats_roundtrip.rs
  encoding_detect.rs
  store_crash.rs
  llm_transport.rs   # wiremock
  pipeline_e2e.rs    # fake transport, full flow
  resume.rs          # interruption and rerun scenarios
```

## 3. Fake transport

`Fake` holds a script: a queue of `(matcher, response | error)` and a call log.
Tests assert both outputs and the exact sequence of requests (e.g. "batch 3 sent once").

## 3a. Recording progress sink

Progress (`ARCHITECTURE.md` §11, FR-11) is tested with a recording `Progress` double, not by scraping
stderr: a `RecordingProgress` behind a `Mutex`/atomics captures the ordered events (`stage_start`,
`stage_units`, per-unit `unit_finished` outcomes, `stage_finish`). Tests assert the event sequence and
counts against the scheduler's `Summary` (total = runnable, tick count = total, Done/Failed split, and the
remaining-only total on a resume). Production code paths pass `&NoopProgress` where a real renderer is not
under test; `main`'s `StderrProgress` rendering (TTY vs plain, throttling) is not asserted here.

## 4. Crash and resume tests

Simulate interruption by:
- cancelling the token after N completed units;
- injecting an error inside the unit commit closure (transaction must roll back);
- dropping the runtime mid-run, then reopening the project.

Then rerun and assert no duplicate transport calls for completed units and identical final output.

## 5. Fixtures

Keep fixtures small (≤ 50 cues) except one ~1,500-cue synthetic file for batching and
map-reduce tests. Fixture text must be original or public-domain; never commit copyrighted
subtitles.

## 6. Manual checks (per release)

- Real Gemini smoke test on a 50-cue file with `standard` and `flex`.
- Open exported ASS/SRT in a player and confirm Burmese renders with the configured font.
