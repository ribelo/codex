# QA

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
