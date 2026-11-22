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
| `GEMINI_API_KEY` | when a stage uses `gemini` (not for `--dry-run`) | Gemini API key. Never written to config, DB, or logs. |
| `OPENROUTER_API_KEY` | when a stage uses `openrouter` (not for `--dry-run`) | OpenRouter API key. Never written to config, DB, or logs. |

Keys are resolved from the environment only (ADR-052); a key is required only when a stage actually
dispatches to that provider, so a Gemini-only run never needs `OPENROUTER_API_KEY` and vice versa.
(A stored-account store + `auth` command — PBI-008 — is deferred.)

## 3. Environment overrides

Names follow `MYANSUB_<SECTION>_<KEY>` (ADR-017). The currently supported variables and the
config keys they map to:

| Variable | Config key |
|---|---|
| `MYANSUB_ANALYSIS_MODEL` | `llm.models.analysis` |
| `MYANSUB_TRANSLATION_MODEL` | `llm.models.translation` |
| `MYANSUB_DEFAULT_PROVIDER` | `llm.default_provider` |
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
- `burmese.digits ∈ {myanmar, western}`, `burmese.cue_final_stop ∈ {keep, strip}`; an invalid
  value is rejected at startup naming the key (ADR-049).
- `llm.default_provider ∈ {gemini, openrouter}` (default `gemini`); a model reference with an
  unknown `provider:` prefix is a usage error (exit 2, ADR-051).
- `llm.openrouter.tier ∈ {floor, flex, default}` (default `floor`),
  `llm.openrouter.data_collection ∈ {deny, allow}` (default `deny`); invalid values rejected at
  startup naming the key (`LLM.md` §10.7).
- Missing price entries for a configured model produce a warning, not an error.

## 7. OpenRouter (`[llm.openrouter]`, ADR-051)

| Key | Values | Default | Effect |
|---|---|---|---|
| `tier` | `floor` \| `flex` \| `default` | `floor` | Endpoint-selection tier; `floor` = cheapest (incl. flex) with automatic fallback (`LLM.md` §10.7). |
| `data_collection` | `deny` \| `allow` | `deny` | Whether endpoints that may train on data are allowed. Named in the free-model routing error (`LLM.md` §10.8). |
| `zdr` | `true` \| `false` | `false` | Require zero-data-retention endpoints. |
| `attribution` | `true` \| `false` | `false` | When true, send `X-OpenRouter-Title: myansub`. |

`llm.default_provider` (`gemini` \| `openrouter`, default `gemini`) selects the provider for an
unprefixed model reference (`LLM.md` §10.1). The OpenRouter base URL is `llm.openrouter.base_url`
(default `https://openrouter.ai/api/v1`); Gemini keeps `llm.base_url`.

## 6. Burmese output policy (`[burmese]`, ADR-049)

| Key | Values | Default | Effect |
|---|---|---|---|
| `digits` | `myanmar` \| `western` | `myanmar` | Digit style the translation prompt requests. Part of the prompt, so changing it marks unlocked translations stale (`LLM.md` §7). The `digit_style` quality flag fires when a cue uses the other system. |
| `cue_final_stop` | `keep` \| `strip` | `strip` | How a trailing ။ at the end of a cue is handled at export. `strip` removes one trailing ။ (and trailing whitespace) per cue; `keep` leaves the text unchanged. Applied deterministically at export, never by the LLM, so changing it needs only a re-export (no LLM calls, no stale-marking). |
