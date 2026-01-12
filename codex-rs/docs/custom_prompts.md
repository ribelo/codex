# Custom Prompts

Codex supports custom prompts that can be invoked via slash commands (e.g., `/prompts:myprompt`).
Prompts are Markdown files stored in `$CODEX_HOME/prompts/` (e.g., `~/.codex/prompts/`).

## Frontmatter

Prompts can include YAML frontmatter to configure their behavior.

```yaml
---
description: "Review current changes"
argument-hint: "[optional extra args]"
---
Your prompt content here...
```
