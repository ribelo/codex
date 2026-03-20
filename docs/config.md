# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Provider compatibility

This fork keeps upstream’s canonical provider selection shape:

- `model_provider = "openai" | "anthropic" | "gemini" | "antigravity" | "<custom-id>"`
- `model = "<raw-model-slug>"`

Built-in native providers:

- `anthropic`
  - default base URL: `https://api.anthropic.com/v1`
  - default key env var: `ANTHROPIC_API_KEY`
  - native wire protocol: `wire_api = "anthropic"`
- `gemini`
  - default base URL: `https://generativelanguage.googleapis.com/v1beta`
  - default key env var: `GEMINI_API_KEY`
  - also supports fork-specific Google OAuth via `codex login gemini`
  - native wire protocol: `wire_api = "gemini"`
  - credential precedence: `experimental_bearer_token` -> stored Gemini OAuth -> `GEMINI_API_KEY`
- `antigravity`
  - fork-specific Google OAuth provider restored from the pre-reset fork
  - uses `codex login antigravity`
  - native wire protocol: `wire_api = "antigravity"`
  - default endpoint selection falls back across the built-in Antigravity Google endpoints unless
    `base_url` overrides it

Non-Responses providers must set `model` explicitly. Codex does not refresh `/models` for
`wire_api = "chat"`, `wire_api = "anthropic"`, `wire_api = "gemini"`, `wire_api = "antigravity"`, or
`wire_api = "bedrock"`.

Custom providers can opt into the supported wire protocols under `[model_providers.<id>]`:

```toml
model_provider = "anthropic"
model = "claude-3-7-sonnet-latest"
```

```toml
model_provider = "gemini"
model = "gemini-2.5-pro"
```

```toml
model_provider = "antigravity"
model = "claude-opus-4-5-thinking"
```

```toml
model_provider = "my-chat-endpoint"
model = "gpt-4.1-mini"

[model_providers.my-chat-endpoint]
name = "My Chat Endpoint"
base_url = "https://example.com/v1"
wire_api = "chat"
env_key = "MY_API_KEY"
```

Anthropic-compatible and Gemini-compatible endpoints can also use bearer auth instead of their
provider-specific API key headers:

```toml
model_provider = "my-anthropic-gateway"
model = "claude-3-7-sonnet-latest"

[model_providers.my-anthropic-gateway]
name = "Anthropic Gateway"
base_url = "https://gateway.example.com/v1"
wire_api = "anthropic"
env_key = "GATEWAY_TOKEN"
use_bearer_auth = true
version = "2023-06-01"
beta = "tools-2024-05-16"
```

Gemini and Antigravity OAuth are provider-scoped fork features:

- `codex login gemini`
- `codex login antigravity`
- `codex logout gemini`
- `codex logout antigravity`
- `codex login status` reports Gemini and Antigravity OAuth availability even when no OpenAI auth
  is configured
- `codex login gemini` requires `CODEX_GEMINI_OAUTH_CLIENT_ID`
- `codex login antigravity` requires `CODEX_ANTIGRAVITY_OAUTH_CLIENT_ID`
- `CODEX_GEMINI_OAUTH_CLIENT_SECRET` and `CODEX_ANTIGRAVITY_OAUTH_CLIENT_SECRET` are optional
  overrides when the Google client requires a secret; otherwise Codex relies on PKCE without an
  embedded secret
- set `GOOGLE_CLOUD_PROJECT` or `GOOGLE_CLOUD_PROJECT_ID` when Gemini or Antigravity needs a
  specific Cloud Code Assist workspace project
- `ANTIGRAVITY_PROJECT_ID` overrides the Google project env vars for Antigravity only
- `PI_AI_ANTIGRAVITY_VERSION` overrides the Antigravity transport `User-Agent` version when the
  sandbox endpoints expect a different client build

Provider-scoped OAuth credentials live in `auth.json` under `GEMINI_ACCOUNTS` and
`ANTIGRAVITY_ACCOUNTS`. They do not replace the normal OpenAI auth mode and do not restore the old
fork-wide provider auth architecture.

Bedrock is supported as a custom provider using AWS credential-chain auth. It does not use
`env_key` or bearer tokens:

```toml
model_provider = "my-bedrock"
model = "anthropic.claude-3-7-sonnet-20250219-v1:0"

[model_providers.my-bedrock]
name = "AWS Bedrock"
wire_api = "bedrock"
aws_region = "us-east-1"
aws_profile = "sandbox"
```

Optional Bedrock notes:

- `base_url` can override the Bedrock endpoint for LocalStack or other test gateways.
- Claude-on-Bedrock models get Bedrock-aware fallback metadata automatically.
- Bedrock structured output uses the native Converse `output_config` JSON-schema path.

Some features remain Responses-only. Conversation compaction, memory summarization, realtime, and
Responses WebSocket transport are only available with `wire_api = "responses"`.

## Custom model metadata

Custom models are a normal supported case in this fork. If a model slug is not present in a
catalog, Codex now uses provider-inferred defaults instead of treating that as a warning-worthy
failure.

When you know more about a model, set its metadata directly in config or in a profile:

```toml
[model_metadata]
context_window = 262144
supports_parallel_tool_calls = true
shell_type = "shell_command"

[profiles.kimi-k2-5]
model_provider = "kimi"
model = "k2p5"

[profiles.kimi-k2-5.model_metadata]
display_name = "Kimi K2.5"
context_window = 262144
supports_reasoning_summaries = true
```

`model_metadata` mirrors the public model metadata surface used internally by Codex, including
fields like `display_name`, `context_window`, `truncation_policy`, `input_modalities`,
`supports_parallel_tool_calls`, `support_verbosity`, `default_verbosity`, and tool-type defaults.

Legacy compatibility aliases remain supported for the most common metadata overrides:

```toml
model_context_window = 262144
model_auto_compact_token_limit = 200000
model_supports_reasoning_summaries = true
```

The same aliases now work inside profiles as well. `model_catalog_json` remains optional for exact
catalog/picker data, but it is no longer the normal path for custom models.

## Connecting to MCP servers

Codex can connect to MCP servers configured in `~/.codex/config.toml`. See the configuration reference for the latest MCP server options:

- https://developers.openai.com/codex/config-reference

## Apps (Connectors)

Use `$` in the composer to insert a ChatGPT connector; the popover lists accessible
apps. The `/apps` command lists available and installed apps. Connected apps appear first
and are labeled as connected; others are marked as can be installed.

## Notify

Codex can run a notification hook when the agent finishes a turn. See the configuration reference for the latest notification settings:

- https://developers.openai.com/codex/config-reference

When Codex knows which client started the turn, the legacy notify JSON payload also includes a top-level `client` field. The TUI reports `codex-tui`, and the app server reports the `clientInfo.name` value from `initialize`.

## JSON Schema

The generated JSON Schema for `config.toml` lives at `codex-rs/core/config.schema.json`.

## SQLite State DB

Codex stores the SQLite-backed state DB under `sqlite_home` (config key) or the
`CODEX_SQLITE_HOME` environment variable. When unset, WorkspaceWrite sandbox
sessions default to a temp directory; other modes default to `CODEX_HOME`.

## Custom CA Certificates

Codex can trust a custom root CA bundle for outbound HTTPS and secure websocket
connections when enterprise proxies or gateways intercept TLS. This applies to
login flows and to Codex's other external connections, including Codex
components that build reqwest clients or secure websocket clients through the
shared `codex-client` CA-loading path and remote MCP connections that use it.

Set `CODEX_CA_CERTIFICATE` to the path of a PEM file containing one or more
certificate blocks to use a Codex-specific CA bundle. If
`CODEX_CA_CERTIFICATE` is unset, Codex falls back to `SSL_CERT_FILE`. If
neither variable is set, Codex uses the system root certificates.

`CODEX_CA_CERTIFICATE` takes precedence over `SSL_CERT_FILE`. Empty values are
treated as unset.

The PEM file may contain multiple certificates. Codex also tolerates OpenSSL
`TRUSTED CERTIFICATE` labels and ignores well-formed `X509 CRL` sections in the
same bundle. If the file is empty, unreadable, or malformed, the affected Codex
HTTP or secure websocket connection reports a user-facing error that points
back to these environment variables.

## Notices

Codex stores "do not show again" flags for some UI prompts under the `[notice]` table.

## Plan mode defaults

`plan_mode_reasoning_effort` lets you set a Plan-mode-specific default reasoning
effort override. When unset, Plan mode uses the built-in Plan preset default
(currently `medium`). When explicitly set (including `none`), it overrides the
Plan preset. The string value `none` means "no reasoning" (an explicit Plan
override), not "inherit the global default". There is currently no separate
config value for "follow the global default in Plan mode".

## Review defaults

`review_reasoning_effort` lets you set a review-specific default reasoning
effort for `/review` and reviewer-role agent spawns. When unset, review flows
fall back to the global `model_reasoning_effort`, and when that is also unset
they fall back to the selected review model's built-in default. Explicit
per-request review reasoning overrides still win over both config values.

## Realtime start instructions

`experimental_realtime_start_instructions` lets you replace the built-in
developer message Codex inserts when realtime becomes active. It only affects
the realtime start message in prompt history and does not change websocket
backend prompt settings or the realtime end/inactive message.

Ctrl+C/Ctrl+D quitting uses a ~1 second double-press hint (`ctrl + c again to quit`).
