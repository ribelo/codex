# QA

## 2026-03-09

### How should restored non-Responses providers fit upstream config?

Keep upstream’s canonical config shape: `model_provider` selects the provider and `model` stays a
raw model slug. Restore additional protocols additively through `wire_api = "chat" |
"anthropic" | "gemini"`.

Built-in native providers are `anthropic` and `gemini`. Custom providers can use `chat`,
`responses`, `anthropic`, or `gemini` depending on the endpoint they expose.

Non-Responses providers do not use remote `/models` discovery. They require an explicit `model`
name and only use `model_catalog` metadata when the user provides it.

Native compatibility knobs stay small and protocol-specific:

- `version` and `beta` for Anthropic-style headers
- `use_bearer_auth = true` when Anthropic-compatible or Gemini-compatible gateways expect
  `Authorization: Bearer ...` instead of the provider-specific API key header

Responses-only features stay explicit. Conversation compaction, memory summarization, realtime,
and websocket transport are only supported when `wire_api = "responses"`.

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
