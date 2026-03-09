# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Provider compatibility

This fork keeps upstream’s canonical provider selection shape:

- `model_provider = "openai" | "anthropic" | "gemini" | "<custom-id>"`
- `model = "<raw-model-slug>"`

Built-in native providers:

- `anthropic`
  - default base URL: `https://api.anthropic.com/v1`
  - default key env var: `ANTHROPIC_API_KEY`
  - native wire protocol: `wire_api = "anthropic"`
- `gemini`
  - default base URL: `https://generativelanguage.googleapis.com/v1beta`
  - default key env var: `GEMINI_API_KEY`
  - native wire protocol: `wire_api = "gemini"`

Non-Responses providers must set `model` explicitly. Codex does not refresh `/models` for
`wire_api = "chat"`, `wire_api = "anthropic"`, or `wire_api = "gemini"`.

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

Some features remain Responses-only. Conversation compaction, memory summarization, realtime, and
Responses WebSocket transport are only available with `wire_api = "responses"`.

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

## Realtime start instructions

`experimental_realtime_start_instructions` lets you replace the built-in
developer message Codex inserts when realtime becomes active. It only affects
the realtime start message in prompt history and does not change websocket
backend prompt settings or the realtime end/inactive message.

Ctrl+C/Ctrl+D quitting uses a ~1 second double-press hint (`ctrl + c again to quit`).
