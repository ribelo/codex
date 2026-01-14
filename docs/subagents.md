# Workers

Codex supports delegating work to internal worker sessions.

## Task-Based Delegation (Recommended)

Modern delegation uses the `task` tool with "WHAT+DIFFICULTY" routing. Instead of picking a specific persona, the system routes the request based on the `task_type` and `difficulty`.

Users should prefer configuring tasks via `[[task]]` in `config.toml`. See `codex-rs/docs/tasks.md` for the primary documentation.

## Legacy: Custom Worker Definitions

The legacy worker-definition system allows defining custom “worker” prompts via Markdown files.

### Loading Logic

1. **Built-in agents**: Embedded in the binary and immutable (e.g., `rush`, `finder`, `oracle`).
2. **Custom agents**: Loaded from `$CODEX_HOME/agents/*.md`.
3. **Config Overrides**: Metadata for agents can be patched via `[[agent]]` in `config.toml`.

### Configuration Overrides (`[[agent]]`)

Overrides only patch runtime metadata (model, reasoning_effort, enabled); they do **not** modify agent prompts or system instructions.

```toml
[[agent]]
name = "rush"
model = "anthropic/claude-3-5-haiku-latest"
enabled = true
```

### Custom Worker Format

## File format

Each worker file is a Markdown document with **YAML frontmatter** and a body:

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
- `allowed_subagents` (optional; legacy key name):
  - omit to allow delegating to any worker (default for root sessions is full access; default for workers is none unless specified),
  - set to `[]` to forbid further delegation,
  - or set to a list of worker slugs.
- `internal` (optional, default `false`): if `true`, the agent is hidden from the main `task` tool unless explicitly listed in the parent's `allowed_subagents`.

### Removed: `profile`

The `profile` frontmatter field is no longer supported. Use `model: inherit` or `model: {provider}/{model}` instead.
