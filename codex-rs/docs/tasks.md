# Task Routing and Difficulty

Codex uses a "WHAT+DIFFICULTY" routing system to determine how to handle a request. This system avoids rigid persona mapping, instead focusing on the nature of the work and its complexity.

## Task Architecture

The `task` tool is the primary mechanism for delegation. When called, it spawns an internal worker session.

- **Built-in Tasks**: Hardcoded in the Rust binary (see `codex-rs/core/src/task_registry.rs`). These often include an embedded system prompt template (e.g., `review.md` or `finder.md`) that defines their specialized behavior.
- **Configured Tasks**: Defined in your `config.toml` via `[[task]]`. These can add new types or override settings for built-in types.

## Task Routing

When a user provides input, Codex classifies the task into a specific **Type** and assigns it a **Difficulty**. This classification guides the selection of the appropriate model and prompt configuration.

### Built-in Task Types

Codex includes several built-in task types:

- `code`: General code changes and implementation work.
- `rust`: Specialized Rust code changes and refactoring.
- `ui`: Frontend and UI work.
- `search`: Finding code, definitions, and references.
- `planning`: Architecture and design decisions.
- `review`: Code review and feedback.

### Difficulty Levels

Difficulty is categorized as:

- `small`: Trivial or routine tasks with clear scope (e.g., fixing a typo, renaming a variable).
- `medium`: Standard tasks requiring some context and multiple steps (default).
- `large`: Complex tasks requiring deep context or multi-file changes.

## Task Tool API

The system can delegate work using the `task` tool, by specifying the type of work and its difficulty.

The `task` tool accepts:
  - `description`: Short description of the task.
  - `prompt`: The full prompt for the worker/handler.
- `task_type`: One of the supported task types.
- `difficulty`: (Optional) The estimated difficulty level (`small`, `medium`, or `large`). Defaults to `medium` when omitted.
- `session_id`: Optional session ID to continue an existing worker session.

### Example Call

```json
{
  "task_type": "rust",
  "difficulty": "medium",
  "description": "Implement a new trait",
  "prompt": "Implement the `Display` trait for the `User` struct in `src/models.rs`."
}
```

## Configuration

You can configure how Codex handles different tasks in your `config.toml`. This includes overriding default models for specific combinations of task type and difficulty.

### Task Customization (`[[task]]`)

Each `[[task]]` entry can specify:
- `type`: The slug (e.g., "rust").
- `description`: (Optional) Overrides the default description.
- `model`: (Optional) The model to use for this task.
- `reasoning_effort`: (Optional) `minimal`, `low`, `medium`, `high`, `xhigh`.
- `sandbox_policy`: (Optional) `read-only`, `workspace-write`, `danger-full-access`.
- `approval_policy`: (Optional) `untrusted`, `on-failure`, `on-request`, `never`.
- `skills`: (Optional) List of skill names to inject.
- `difficulty`: (Optional) Map of overrides for specific difficulties.

```toml
[[task]]
type = "rust"  # Override a built-in
model = "anthropic/claude-3-5-sonnet"
skills = ["tokio", "rust-expert"]

[task.difficulty.small]
model = "openai/gpt-4o-mini"

[task.difficulty.large]
model = "openai/o3-mini"
reasoning_effort = "high"
```

**Note on Prompts**: Built-in tasks use an embedded system prompt template. Providing a `description` in config will be prepended to this template. Config-defined tasks (new types) use the `description` as their entire system prompt.

### Adding New Task Types

To add a custom task type, add a `[[task]]` entry to your `config.toml`:

```toml
[[task]]
type = "docs"
description = "Creating or updating technical docs"
sandbox_policy = "workspace-write"
approval_policy = "on-request"
```

## Developer Extension

To learn how to add new built-in task handlers or modify the task system, see [Task Handlers](./task_handlers.md).

## Skills and Discovery

Codex supports "Skills"

- `./.codex/skills` (project-local)
- `$CODEX_HOME/skills` (user-level)

### SKILL.md

Each skill must have a `SKILL.md` file in its root directory. This file serves as the entry point and contains:
- `name`: The unique identifier for the skill.
- `description`: A clear explanation of what the skill does (used for discovery).
- `instructions`: Detailed guidance for the agent on how to use the skill.

### Discovery Mechanism

At startup, Codex scans the skills directory and reads the `SKILL.md` files. When a user request matches a skill's description or explicitly mentions a skill name, the relevant instructions are injected into the agent's context.

To add a new skill, simply create a new directory in `~/.codex/skills/` with a `SKILL.md` file.
