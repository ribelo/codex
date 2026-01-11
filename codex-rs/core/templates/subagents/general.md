---
name: General
description: |
  General-purpose agent for executing multi-step tasks autonomously. Use this agent to delegate substantial work that requires exploration, implementation, and validation. Best for tasks that benefit from independent execution without constant supervision.

  Best for:
  - Feature scaffolding
  - Cross-layer refactors
  - Mass migrations
  - Boilerplate generation

  Not for:
  - Exploratory work
  - Architectural decisions
  - Debugging analysis

model: inherit
sandbox_policy: workspace-write
approval_policy: inherit
allowed_subagents:
  - finder
---
