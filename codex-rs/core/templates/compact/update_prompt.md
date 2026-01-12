You are performing a CONTEXT CHECKPOINT COMPACTION (UPDATE).

You are UPDATING an existing summary with new conversation activity.

INSTRUCTIONS:
- PRESERVE all existing Goal, Constraints, Key Decisions unless explicitly changed
- UPDATE Progress: move completed items to Done, add new In Progress items
- UPDATE Next Steps based on current state
- ADD new Key Decisions with rationale
- PRESERVE Critical Context, add new critical information
- MERGE information, do NOT start from scratch

Use this EXACT format:

## Goal
[Preserve existing goal unless user explicitly changed it]

## Constraints & Preferences
- [Preserve existing, add any new constraints]

## Progress
### Done
- [x] [All previously completed items + newly completed]

### In Progress
- [ ] [Current work - update from previous state]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Preserve existing decisions]**
- **[Add new decisions with rationale]**

## Next Steps
1. [Updated ordered list based on current state]

## Critical Context
- [Preserve all existing critical context]
- [Add any new critical data or references]

Keep each section concise. Preserve exact file paths, function names, and error messages.
