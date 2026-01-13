# Subagents

Codex supports delegating work to **subagents**: specialized agents that run in their own session/context.

## Loading Logic

1. **Built-in agents**: Embedded in the binary and immutable. Files in `$CODEX_HOME/agents/` that match a built-in slug (e.g., `rush`, `finder`, `oracle`) are **ignored**.
2. **Custom agents**: Loaded from `$CODEX_HOME/agents/*.md` only if the slug does not conflict with a built-in.
3. **Config overrides**: Metadata for any loaded agent (built-in or custom) can be patched via `config.toml`.

## Configuration Overrides (`config.toml`)

You can override agent metadata in your `config.toml`. Overrides only patch runtime metadata (model, reasoning_effort, enabled); they do **not** modify agent prompts or system instructions.

```toml
[[agent]]
name = "rush"
model = "anthropic/claude-3-5-haiku-latest"
reasoning_effort = "none"
enabled = true
```

### Override fields

- `name`: The slug of the agent to override (required).
- `model`: Canonical model ID (`{provider}/{model}`) or `inherit`.
- `reasoning_effort`: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `inherit`.
- `enabled`: Boolean to enable/disable the agent.

## Migration Note

Built-in agents are no longer copied to `$CODEX_HOME/agents/`. If you previously modified built-in agents by editing files in that directory, those changes are now ignored. Use the `[[agent]]` override mechanism in `config.toml` for metadata changes. Prompt changes for built-in agents are no longer supported.

## Where subagents live

Custom subagents are Markdown files in `$CODEX_HOME/agents/` (by default `~/.codex/agents/`).

## File format

Each subagent file is a Markdown document with **YAML frontmatter** and a body:

```md
---
name: Code Finder
description: "Fast code search agent."
model: inherit
sandbox_policy: read-only
approval_policy: never
allowed_subagents: []
---

You are a code search agent. Use rg, list files, and return paths.
```

### Frontmatter fields

- `name` (optional): display name.
- `description` (optional): shown in UIs/tooling.
- `model` (optional, default `inherit`):
  - `inherit` uses the parent session’s model.
  - `{provider}/{model}` uses an explicit canonical model id (for example `openai/gpt-5.1-codex-mini`).
- `reasoning_effort` (optional, default `inherit`):
  - `inherit` uses the parent session's reasoning effort.
  - `none`, `minimal`, `low`, `medium`, `high`, `xhigh`.
- `sandbox_policy` (required): `read-only`, `workspace-write`, `danger-full-access`, or `inherit`.
- `approval_policy` (required): `untrusted`, `on-failure`, `on-request`, `never`, or `inherit`.
- `allowed_subagents` (optional):
  - omit to allow delegating to any subagent (default for root sessions is full access; default for subagents is none unless specified),
  - set to `[]` to forbid further delegation,
  - or set to a list of subagent slugs.
- `internal` (optional, default `false`): if `true`, the agent is hidden from the main `task` tool unless explicitly listed in the parent's `allowed_subagents`.

### Removed: `profile`

The `profile` frontmatter field is no longer supported for subagents. Use `model: inherit` or `model: {provider}/{model}` instead.
