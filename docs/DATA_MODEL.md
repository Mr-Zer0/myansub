# Data Model

## 1. Domain types (sketch)

```rust
pub struct Document {
    pub format: SubtitleFormat,          // Srt | Vtt | Ass | Ssa | Sbv
    pub format_meta: Option<FormatMeta>, // e.g. ASS script info + styles, kept for same-format output
    pub cues: Vec<Cue>,
}

pub struct Cue {
    pub idx: u32,                 // 1-based, stable for the life of the project
    pub start_ms: u64,
    pub end_ms: u64,
    pub text_raw: String,         // exactly as parsed
    pub text_masked: String,      // inline tags replaced by ⟦n⟧ placeholders; lead tags removed
    pub lead_tags: Option<String>,// whole-line leading override block, e.g. {\an8\pos(10,20)}
    pub inline_tags: Vec<String>, // placeholder n → original tag
    pub style: Option<String>,
    pub speaker: Option<String>,  // ASS Name/Actor field when present
    pub translatable: bool,       // false if no letters after tag removal
}
```

Placeholders use the form `⟦0⟧`, `⟦1⟧` … numbered per cue.

## 2. SQLite schema (migration 1)

```sql
PRAGMA foreign_keys = ON;

CREATE TABLE project (
  id                  INTEGER PRIMARY KEY CHECK (id = 1),
  source_path         TEXT NOT NULL,
  source_hash         TEXT NOT NULL,           -- sha256 of raw bytes
  source_format       TEXT NOT NULL,           -- srt|vtt|ass|ssa|sbv
  source_encoding     TEXT NOT NULL,
  encoding_confidence REAL,
  source_lang         TEXT NOT NULL,
  target_lang         TEXT NOT NULL DEFAULT 'my',
  format_meta         TEXT,                    -- JSON
  created_at          TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE stage_state (
  stage      TEXT PRIMARY KEY,                 -- input|analysis|analysis_review|translation|translation_review|output
  status     TEXT NOT NULL,                    -- not_started|running|completed|approved
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE cues (
  id           INTEGER PRIMARY KEY,
  idx          INTEGER NOT NULL UNIQUE,
  start_ms     INTEGER NOT NULL,
  end_ms       INTEGER NOT NULL,
  text_raw     TEXT NOT NULL,
  text_masked  TEXT NOT NULL,
  lead_tags    TEXT,
  inline_tags  TEXT NOT NULL DEFAULT '[]',     -- JSON array
  style        TEXT,
  speaker      TEXT,
  translatable INTEGER NOT NULL
);

-- Map phase units (only used when transcript exceeds single_pass_max_tokens)
CREATE TABLE analysis_chunks (
  chunk_no   INTEGER PRIMARY KEY,
  first_cue  INTEGER NOT NULL,
  last_cue   INTEGER NOT NULL,
  status     TEXT NOT NULL,                    -- pending|in_flight|done|failed
  attempts   INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  result     TEXT                              -- JSON partial analysis
);

CREATE TABLE analysis (
  version       INTEGER PRIMARY KEY,           -- highest = current
  doc           TEXT NOT NULL,                 -- JSON, §3
  origin        TEXT NOT NULL CHECK (origin IN ('llm','user')),
  source_digest TEXT NOT NULL,
  signature     TEXT NOT NULL,                 -- "prompt_v{N}|{model}"
  approved      INTEGER NOT NULL DEFAULT 0,
  created_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE translation_batches (
  batch_no         INTEGER PRIMARY KEY,
  first_cue        INTEGER NOT NULL,
  last_cue         INTEGER NOT NULL,
  status           TEXT NOT NULL,              -- pending|in_flight|done|failed
  attempts         INTEGER NOT NULL DEFAULT 0,
  last_error       TEXT,
  input_hash       TEXT,                       -- hash of full batch input incl. context lines
  model            TEXT,
  analysis_version INTEGER,
  updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE translations (
  cue_id           INTEGER PRIMARY KEY REFERENCES cues(id),
  text             TEXT NOT NULL,              -- masked form (placeholders restored on export)
  origin           TEXT NOT NULL CHECK (origin IN ('llm','user')),
  locked           INTEGER NOT NULL DEFAULT 0,
  stale            INTEGER NOT NULL DEFAULT 0,
  flags            TEXT NOT NULL DEFAULT '[]', -- JSON array of QualityFlag
  analysis_version INTEGER NOT NULL,
  model            TEXT,
  updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE translation_history (
  id     INTEGER PRIMARY KEY,
  cue_id INTEGER NOT NULL REFERENCES cues(id),
  text   TEXT NOT NULL,
  origin TEXT NOT NULL,
  at     TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE llm_cache (
  key            TEXT PRIMARY KEY,             -- sha256 of canonical request (LLM.md §4)
  model          TEXT NOT NULL,
  prompt_version INTEGER NOT NULL,
  response       TEXT NOT NULL,
  usage          TEXT NOT NULL,                -- JSON Usage
  created_at     TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE llm_calls (
  id              INTEGER PRIMARY KEY,
  stage           TEXT NOT NULL,
  unit            INTEGER,
  model           TEXT NOT NULL,
  service_tier    TEXT,
  prompt_tokens   INTEGER,
  cached_tokens   INTEGER,
  output_tokens   INTEGER,
  thinking_tokens INTEGER,
  latency_ms      INTEGER,
  cache_hit_local INTEGER NOT NULL DEFAULT 0,
  dry_run         INTEGER NOT NULL DEFAULT 0,
  outcome         TEXT NOT NULL,               -- ok|retryable|fatal|cancelled
  at              TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE provider_cache (
  name             TEXT PRIMARY KEY,           -- Gemini cachedContents/... name
  analysis_version INTEGER NOT NULL,
  model            TEXT NOT NULL,
  token_count      INTEGER NOT NULL,
  expires_at       TEXT NOT NULL
);
```

`stale` is set by selective invalidation (`LLM.md` §7). `locked` cues are never set stale.

## 3. Analysis document (schema_version 1)

Prose fields are written in English. Limits are enforced both in the Gemini response schema
and in serde validation on import.

```json
{
  "schema_version": 1,
  "premise": "string ≤500 — stable whole-title facts",
  "tone": "string ≤160",
  "characters": [
    {
      "id": "c1",
      "name": "string ≤120",
      "aliases": ["string ≤80, max 8"],
      "gender": "m|f|unknown",
      "age_group": "child|young|adult|elder|unknown",
      "role": "string ≤260",
      "speech_style": "string ≤180"
    }
  ],
  "address_matrix": [
    {
      "from": "c1",
      "to": "c2",
      "relation": "string ≤160",
      "self_ref": "Burmese self-reference used by `from`",
      "addressee_ref": "Burmese term `from` uses for `to`",
      "register": "colloquial|polite|formal",
      "particle": "Burmese sentence-final particle, may be empty"
    }
  ],
  "glossary": [
    { "term": "string ≤120", "target": "string ≤120", "keep": false, "note": "string ≤180" }
  ],
  "scenes": [
    { "cue_range": [1, 85], "summary": "string ≤300", "speakers": ["c1", "c2"], "mood": "string ≤80" }
  ],
  "style_rules": ["string ≤180, max 12"],
  "unresolved_ambiguities": ["string ≤220, max 12"]
}
```

Array caps: characters ≤ 40, address_matrix ≤ 80, glossary ≤ 120, scenes ≤ 120.

Validation rules on import:
- `from`, `to`, and `speakers` must reference existing character ids.
- Scene ranges must lie within existing cue indices, ascending, non-overlapping.
- `keep: true` implies `target` equals `term`.
- Unknown fields are rejected on user import (strict), ignored on LLM output (lenient).

Provenance (`source_digest`, `signature`) is stored in table columns, not in the JSON.

## 4. Translation response (from LLM)

```json
{ "lines": [ { "id": 104, "t": "translated text with ⟦0⟧ placeholders" } ] }
```

## 5. Quality flags

```
empty | placeholder_mismatch | untranslated | zawgyi_suspect | boundary_drift
| glossary_miss | too_long | too_fast | failed
```

Serialized as snake_case strings in `translations.flags`.

## 6. Review file formats

### Analysis review file
The analysis document (§3), pretty-printed JSON. The whole document is editable.

### Translation review file
Pretty-printed JSON. Only `target` is editable; other fields are informational and ignored on import.

```json
{
  "project_source_hash": "sha256:…",
  "analysis_version": 3,
  "cues": [
    {
      "id": 104,
      "time": "00:12:03.120 --> 00:12:05.400",
      "speaker": null,
      "source": "Where are you going?",
      "target": "ဘယ်သွားမလို့လဲ",
      "flags": ["too_fast"],
      "locked": false
    }
  ]
}
```

Import rules:
- `project_source_hash` must match the project.
- A cue whose `target` differs from the stored text becomes `origin=user, locked=1`,
  and the previous text is written to `translation_history`.
- Quality checks re-run on changed cues; flags are updated.
- Unknown ids, missing ids, or a parse error reject the whole import.
