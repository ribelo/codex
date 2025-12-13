# Custom Commands

Codex supports custom commands that can be invoked via slash commands (e.g., `/commands:review`).
Commands are Markdown files stored in `$CODEX_HOME/commands/` (e.g., `~/.codex/commands/`).

Unlike custom prompts, commands run in a new, isolated one-shot session. The command’s Markdown
body becomes the user message in that sub-session, and only the final assistant response is
recorded back into the parent chat history.

## Frontmatter

Commands can include YAML frontmatter to configure how they run.

```yaml
---
description: "Quick review with a different model"
argument-hint: "[optional args]"
profile: "fast"
---
Review the following changes: $ARGUMENTS
```

Supported keys:

- **`description`**: short description shown in the slash popup
- **`argument-hint`** / **`argument_hint`**: hint shown after the description
- **`agent`**: run under a specific subagent (e.g. `explorer`)
- **`profile`**: run under a specific profile from `config.toml`

`agent` and `profile` are mutually exclusive.

## Examples

### Example: Agent command

File: `~/.codex/commands/explore.md`

```markdown
---
description: "Find files matching a pattern"
agent: explorer
---
Find files matching $1
```

Usage: `/commands:explore *.rs`

### Example: Profile command

File: `~/.codex/commands/fast_review.md`

```markdown
---
description: "Review using the fast profile"
profile: fast
---
Review these files: $ARGUMENTS
```

Usage: `/commands:fast_review *.rs core/src`

