# Configuration

## 1. Resolution order (later wins)

1. Built-in defaults
2. Global file: `<config_dir>/subtrans/config.toml` (platform config dir)
3. Project file: `<project>/subtrans.toml`
4. File given with `--config`
5. Environment variables `SUBTRANS_*`
6. CLI flags

`.env` is loaded with `dotenvy` from the current directory at startup (missing file is not an error).

## 2. Secrets

| Variable | Required | Meaning |
|---|---|---|
| `GEMINI_API_KEY` | yes (not for `--dry-run`) | Gemini API key. Never written to config, DB, or logs. |

## 3. Environment overrides

`SUBTRANS_ANALYSIS_MODEL`, `SUBTRANS_TRANSLATION_MODEL`, `SUBTRANS_SERVICE_TIER`,
`SUBTRANS_CONCURRENCY`. Add more only when needed; keep names `SUBTRANS_<SECTION>_<KEY>`.

## 4. Full reference

See `config.example.toml` in the repository root. Every key there is documented inline and
shows its default. Unknown keys are an error (`#[serde(deny_unknown_fields)]`).

## 5. Validation

Config is validated at startup:
- `concurrency ≥ 1`, `batch_size` between 5 and 300, `max_retries ≤ 20`.
- Models must be non-empty strings.
- `service_tier ∈ {flex, standard}`, `cache.provider ∈ {off, implicit, explicit, auto}`.
- Missing price entries for a configured model produce a warning, not an error.
