# Custom Prompts

Codex supports custom prompts that can be invoked via slash commands (e.g., `/prompts:myprompt`).
Prompts are Markdown files stored in `$CODEX_HOME/prompts/` (e.g., `~/.codex/prompts/`).

If you want a command that runs in an isolated, one-shot session (optionally under a specific
agent or profile) and only records the final response back into the main chat history, use
custom commands instead: see `docs/custom_commands.md`.

## Frontmatter

Prompts can include YAML frontmatter to configure their behavior.

```yaml
---
description: "Review current changes"
argument-hint: "[optional extra args]"
---
Your prompt content here...
```
