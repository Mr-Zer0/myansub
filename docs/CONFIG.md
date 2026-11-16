# Configuration

## 1. Resolution order (later wins)

1. Built-in defaults
2. Global file: `<config_dir>/myansub/config.toml` (platform config dir)
3. Project file: `<project>/myansub.toml`
4. File given with `--config`
5. Environment variables `MYANSUB_*`
6. CLI flags

`.env` is loaded with `dotenvy` from the current directory at startup (missing file is not an error).

## 2. Secrets

| Variable | Required | Meaning |
|---|---|---|
| `GEMINI_API_KEY` | yes (not for `--dry-run`) | Gemini API key. Never written to config, DB, or logs. |

## 3. Environment overrides

Names follow `MYANSUB_<SECTION>_<KEY>` (ADR-017). The currently supported variables and the
config keys they map to:

| Variable | Config key |
|---|---|
| `MYANSUB_ANALYSIS_MODEL` | `llm.models.analysis` |
| `MYANSUB_TRANSLATION_MODEL` | `llm.models.translation` |
| `MYANSUB_SERVICE_TIER` | `llm.service_tier` |
| `MYANSUB_CONCURRENCY` | `translation.concurrency` |

Add more only when needed; keep the `MYANSUB_<SECTION>_<KEY>` convention. Unknown
`MYANSUB_*` variables are ignored.

## 4. Full reference

See `docs/config.example` in the repository. Every key there is documented inline and
shows its default. Unknown keys are an error (`#[serde(deny_unknown_fields)]`).

## 5. Validation

Config is validated at startup:
- `concurrency ≥ 1`, `batch_size` between 5 and 300, `max_retries ≤ 20`.
- Models must be non-empty strings.
- `service_tier ∈ {flex, standard}`, `cache.provider ∈ {off, implicit, explicit, auto}`.
- Missing price entries for a configured model produce a warning, not an error.
