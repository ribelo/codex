# Task Handlers (Developer Guide)

This document explains how to extend or modify the Codex task system.

## Architecture Overview

The task system is managed by the `TaskRegistry` in `core/src/task_registry.rs`. It handles:
1. Registering built-in task types.
2. Loading and merging `[[task]]` overrides from `config.toml`.
3. Resolving the execution strategy (model, reasoning) based on task type and difficulty.

The execution of a task is handled by `TaskHandler` in `core/src/tools/handlers/task.rs`.

## Adding a Built-in Task Type

Built-in tasks are defined in `BUILTIN_TASKS` within `core/src/task_registry.rs`.

```rust
BuiltinTask {
    task_type: "my-task",
    description: "My specialized task handler",
    sandbox_policy: SubagentSandboxPolicy::ReadOnly,
    system_prompt: Some(MY_TASK_SYSTEM_PROMPT),
}
```

### System Prompts

Detailed system prompts for built-in tasks are usually stored as Markdown files in `core/src/subagents/` and included using `include_str!`.

## Extension Points

### 1. Configuration (`config.toml`)
The easiest way to add new tasks or modify existing ones is via `[[task]]` entries. This does not require recompiling Codex.

### 2. Built-in Templates
Modify or add Markdown files in `core/src/subagents/` (legacy directory name) to update the base instructions for built-in task types.

### 3. Custom Rust Handlers
If a task requires specialized Rust logic during the tool call (before spawning the worker session), you must modify `TaskHandler::handle` in `core/src/tools/handlers/task.rs` and rebuild Codex.

There is currently no plugin API for third-party task handlers.

### 4. Custom Tools
Worker sessions currently inherit the same tool set as their parent session; tasks do not gate tool access.

The `skills` system only injects additional instructions into the worker’s system prompt.

## Workflow for New Handlers

1. Define the `task_type` slug.
2. Create a system prompt Markdown file in `core/src/subagents/` if needed.
3. Register the task in `core/src/task_registry.rs`.
4. (Optional) Define default difficulty overrides in the registry or config.
5. Verify by calling the task through the `task` tool or via the CLI with appropriate routing.
