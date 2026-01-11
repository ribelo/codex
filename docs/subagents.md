# Subagents

Codex supports delegating work to **subagents**: specialized agents that run in their own session/context.

## Where subagents live

Subagents are Markdown files in `$CODEX_HOME/agents/` (by default `~/.codex/agents/`).

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
  - omit to allow delegating to any subagent,
  - set to `[]` to forbid further delegation,
  - or set to a list of subagent slugs.

### Removed: `profile`

The `profile` frontmatter field is no longer supported for subagents. Use `model: inherit` or `model: {provider}/{model}` instead.
