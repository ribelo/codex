# QA

## 2026-03-10

### How should Tau's Deep agent live in this Codex repo?

Keep it as a project-defined agent in `.codex/config.toml`, not a Rust built-in.

Implementation decisions:

- expose it as `deep` through the project agent registry
- give it its own prompt file under `.codex/agents/prompts/deep.md`
- set `model_reasoning_effort = "high"` to preserve the Deep mode behavior
- do not hard-wire a provider or model in the project role so the agent still works with the
  caller's current provider/model selection

## 2026-03-09

### How should restored non-Responses providers fit upstream config?

Keep upstream’s canonical config shape: `model_provider` selects the provider and `model` stays a
raw model slug. Restore additional protocols additively through `wire_api = "chat" |
"anthropic" | "gemini" | "bedrock"`.

Built-in native providers are `anthropic` and `gemini`. Custom providers can use `chat`,
`responses`, `anthropic`, `gemini`, or `bedrock` depending on the endpoint they expose.

Non-Responses providers do not use remote `/models` discovery. They require an explicit `model`
name and only use `model_catalog` metadata when the user provides it.

Native compatibility knobs stay small and protocol-specific:

- `version` and `beta` for Anthropic-style headers
- `use_bearer_auth = true` when Anthropic-compatible or Gemini-compatible gateways expect
  `Authorization: Bearer ...` instead of the provider-specific API key header

Responses-only features stay explicit. Conversation compaction, memory summarization, realtime,
and websocket transport are only supported when `wire_api = "responses"`.

### How should Bedrock fit the restored provider architecture?

Bedrock stays custom-provider-only in v1. Do not add a built-in `model_provider = "bedrock"`
entry.

Use the native AWS Rust SDK `ConverseStream` path with `wire_api = "bedrock"`. Bedrock auth uses
the AWS credential chain plus optional `aws_region` and `aws_profile` provider fields; it does not
use `env_key`, bearer tokens, query params, or custom auth headers.

Bedrock-specific behavior:

- `base_url` is only an endpoint override for LocalStack or private gateways
- explicit `model` is required and `/models` discovery is skipped
- Claude-on-Bedrock model ids get Anthropic-like fallback metadata
- `ServiceTier::Fast` maps to Bedrock `priority`, and `ServiceTier::Flex` maps to Bedrock `flex`
- structured output uses Bedrock Converse `output_config` with a JSON schema string

### How should Gemini OAuth and Antigravity fit the restored fork?

Treat both as first-class fork features on top of the current additive provider architecture.
Do not revive the old global `provider_kind`, provider-profile, canonical `provider/model`, or
shared auth-mode design.

Provider/runtime decisions:

- `gemini` stays a built-in provider and supports fork-specific Google OAuth in addition to
  `GEMINI_API_KEY`
- built-in `antigravity` is restored as a fork-specific native provider with `wire_api =
  "antigravity"`
- Gemini credential precedence is `experimental_bearer_token` -> stored Gemini OAuth ->
  `GEMINI_API_KEY`
- Antigravity uses provider-scoped OAuth credentials and its native endpoint fallback order
- Gemini OAuth uses the restored Google Code Assist transport and request shape

Auth/CLI decisions:

- store provider-scoped Google OAuth credentials in `auth.json` under `GEMINI_ACCOUNTS` and
  `ANTIGRAVITY_ACCOUNTS`
- do not embed Google OAuth client credentials in the repo; read provider client IDs from env and
  allow optional env-provided client secrets
- provider-only OAuth buckets must not be treated as primary OpenAI auth
- restore `codex login gemini`, `codex login antigravity`, `codex logout gemini`, and
  `codex logout antigravity`
- `login status` reports Gemini and Antigravity OAuth availability even when no OpenAI auth exists

Wire/runtime decisions:

- use `/home/ribelo/projects/ribelo/pi-mono` as the current fork reference for Gemini Code Assist
  and Antigravity behavior instead of the stale backup branch alone
- Gemini Code Assist uses the newer cloud-sdk header set plus top-level `userAgent` and
  `requestId` request fields
- Gemini 3 models use `thinkingLevel`; older Gemini models keep token-budget-based thinking
- Gemini tool declarations default to `parametersJsonSchema`, but Claude-on-Code-Assist keeps the
  legacy `parameters` field
- Antigravity uses a plain `userAgent = "antigravity"` in the request body, a versioned transport
  `User-Agent` header controlled by `PI_AI_ANTIGRAVITY_VERSION`, and the dual system-instruction
  prefix used in `pi-mono`
- Google project resolution for Gemini and Antigravity should honor
  `GOOGLE_CLOUD_PROJECT` / `GOOGLE_CLOUD_PROJECT_ID`, with `ANTIGRAVITY_PROJECT_ID` as an
  Antigravity-specific override

### How should custom-model metadata work in the fork?

Do not require hardcoded model entries or JSON catalogs for normal custom-model use.

Custom models should resolve from provider-aware inferred defaults plus config overrides. Missing an
exact catalog entry is not a warning-worthy scenario by itself.

Config decisions:

- add a typed `model_metadata` block at the root config level and inside profiles
- let `model_metadata` mirror the public `ModelInfo` surface, excluding the internal fallback flag
- keep `model_catalog_json` optional for exact catalog/picker metadata, not as the normal fix for
  custom models
- keep provider transport/auth settings out of `model_metadata`

Compatibility/runtime decisions:

- profile-level `model_context_window`, `model_auto_compact_token_limit`, and
  `model_supports_reasoning_summaries` are accepted as compatibility aliases and feed into the
  same effective metadata path
- inferred metadata remains internal fallback state for code that cares, but Codex must not emit
  the old scary per-turn warning just because a slug is custom
- explicit config instruction overrides still win over `model_metadata.base_instructions`

### How should the built-in footer and `/statusline` relate?

The built-in idle footer is the richer layout:

- left: colored sandbox dot, then `project:branch +A/-D` when git data is available
- center: `provider • model • reasoning`, plus `• plan` in Plan mode
- right: context text such as `50% left`

`/statusline` remains a backward-compatible flat ordered override. It does not get left/center/right slots.
The sandbox dot belongs to the built-in footer only, not to custom `/statusline` overrides.

### What should `/statusline` expose?

Keep all existing item ids working and add additive items for:

- `project-name`
- `provider`
- `reasoning-effort`
- `mode`
- `git-changes`
- `git-summary`

The `/statusline` preview must show the real built-in footer when no override is set.
