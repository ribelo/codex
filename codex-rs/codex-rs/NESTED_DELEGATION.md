# Nested Subagent Delegation Support

## Overview

This implementation adds first-class support for nested subagent delegation, allowing the CLI/TUI to display a clear tree structure showing "who delegated to whom, and what each nested agent did."

## Changes Made

### 1. Protocol Extensions (`protocol/src/protocol.rs`)

Extended `SubagentEventPayload` with delegation context fields:

```rust
pub struct SubagentEventPayload {
    // ... existing fields ...
    
    /// Unique identifier for this delegation.
    pub delegation_id: Option<String>,
    
    /// Parent delegation ID if this is a nested delegation.
    pub parent_delegation_id: Option<String>,
    
    /// Nesting depth (0 = top-level, 1 = first nested, etc.)
    pub depth: Option<i32>,
    
    // ...
}
```

All fields are optional for backwards compatibility.

### 2. Delegation Context Module (`core/src/delegation.rs`)

Created a new module for managing delegation hierarchy:

- **`DelegationContext`**: Tracks delegation_id, parent_delegation_id, and depth
  - `new_root()`: Creates a top-level delegation (depth 0)
  - `new_child()`: Creates a nested delegation from a parent

- **`DelegationRegistry`**: Session-wide registry for managing active delegation contexts
  - `enter()`: Creates and tracks a new delegation context
  - `exit()`: Exits the current delegation context
  - `current()`: Returns the current delegation context if any

### 3. Session State Integration (`core/src/state/session.rs`)

### 3. Session Services Integration (`core/src/state/service.rs`)

Added `DelegationRegistry` to `SessionServices` (rather than `SessionState`) so it's publicly accessible to tool handlers. This placement makes sense because:
- `SessionServices` is already public within the core crate
- It sits alongside `subagent_sessions`, which is conceptually related
- Tool handlers can access it via `invocation.session.services.delegation_registry`

### 4. Task Handler Updates (`core/src/tools/handlers/task.rs`)

Updated the task handler to:

1. Create a delegation context when a task tool is invoked
2. Propagate delegation_id, parent_delegation_id, and depth to all wrapped events
3. Exit the delegation context when the task completes or fails

The delegation context is automatically nested: if a subagent calls another subagent, the depth increases and parent links are maintained.

### 5. TUI Rendering (`tui/src/history_cell.rs`)

Updated `SubagentTaskCell` to:

1. Store delegation context (delegation_id, parent_delegation_id, depth)
2. Render with proper indentation based on depth:
   - Root level (depth 0): No indentation
   - Nested levels: 2 spaces per depth level
   - Tree connectors: `└` prefix for nested items

Example rendering:

```
• Delegated to @explorer
  └ Find all test files
  └ • Delegated to @explorer          (nested, depth 1)
      └ Search for API endpoints
      └ • Delegated to @explorer      (nested, depth 2)
          └ List directory contents
              ✓ Run ls -la
```

### 6. ChatWidget Integration (`tui/src/chatwidget.rs`)

Updated `on_subagent_event` to:

1. Extract delegation_id, parent_delegation_id, and depth from event payloads
2. Pass these to the subagent cell constructor

## Testing

### Unit Tests

1. **Delegation Context Tests** (`core/src/delegation_test.rs`):
   - Root context creation
   - Child context creation with proper parent linking
   - Nested context with correct depth tracking
   - Registry enter/exit functionality

2. **TUI Rendering Tests** (`tui/src/history_cell.rs`):
   - Root level subagent rendering
   - Nested subagent rendering with indentation
   - Deeply nested rendering with activities

### Running Tests

```bash
# Run delegation context tests
cargo test -p codex-core delegation

# Run TUI rendering tests
cargo test -p codex-tui history_cell

# Update snapshots if needed
cargo insta review -p codex-tui
```

### Manual Testing

To test nested delegation manually:

1. Create a subagent that delegates to another subagent
2. Observe the TUI rendering showing proper nesting with tree connectors
3. Verify that depth increases correctly for each nested level

## Backwards Compatibility

All delegation context fields are optional in the protocol:

- Events without delegation_id/parent_delegation_id/depth will render as before (flat, depth 0)
- Existing subagent implementations continue to work
- The TUI gracefully handles missing delegation context

## Future Enhancements

Potential improvements for future work:

1. **Folding/Expanding**: Add keybindings to collapse/expand nested delegation trees
2. **Visual Improvements**: More sophisticated tree rendering with │, ├, └ characters
3. **Delegation Summary**: Show completion status summary at each delegation node
4. **Integration Tests**: Add end-to-end tests with actual nested subagent calls
5. **Stack-based Registry**: Maintain a proper delegation stack for complex nesting scenarios

## Architecture Notes

### Design Decisions

1. **Optional Fields**: Made delegation context optional to avoid breaking existing code
2. **Simple Registry**: Used a simple current-context model rather than full stack tracking
3. **2-Space Indentation**: Chosen for readability without consuming too much horizontal space
4. **Depth-based Rendering**: Directly tied to depth field rather than reconstructing from parent links

### Thread Safety

- `DelegationRegistry` uses `Arc<RwLock<>>` for thread-safe access
- Multiple subagents can safely share the same registry
- Read locks are used for current context queries
- Write locks are used only when entering/exiting contexts

## Related Files

- `protocol/src/protocol.rs` - Protocol definitions
- `core/src/delegation.rs` - Delegation context and registry
- `core/src/delegation_test.rs` - Delegation tests
- `core/src/state/session.rs` - Session state with registry
- `core/src/tools/handlers/task.rs` - Task tool handler
- `tui/src/history_cell.rs` - TUI rendering with tests
- `tui/src/chatwidget.rs` - Event handling
