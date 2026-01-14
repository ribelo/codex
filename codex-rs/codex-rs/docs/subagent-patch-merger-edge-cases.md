# Worker Patch Application: Edge Cases and Failure Scenarios

This document provides a comprehensive analysis of potential edge cases and failure scenarios
in the worker patch application system.

## Architecture Overview

Workers apply changes directly to the workspace. Parallel workers are serialized via a `repo_lock`.

1. **Patch Application**:
   - Changes made by workers are captured and applied to the main workspace.
   - Uses `apply_patch` logic to update files.

2. **Locking**:
   - `repo_lock` mutex prevents parallel merges within the same turn.
   - Parallel workers from the same turn serialize correctly.

---

## Confirmed Issues

None currently tracked.

---

## Parallel Workers from Same Turn

**Scenario**: Main agent spawns worker A and B in parallel.

**Why it's safe**:
1. Both share the same `invocation.turn.repo_lock`.
2. When A finishes, it acquires lock and merges its changes.
3. When B finishes, it waits for the lock, then merges its changes.

### Nested Workers

**Scenario**: Worker A spawns worker B.

**Why it's safe**:
1. B merges its changes (using A's `repo_lock`).
2. A merges its changes to the main workspace (using main's `repo_lock`).
3. Changes bubble up correctly.

---

## Known Limitations (By Design)

### Large Diffs (Memory)

The patch is loaded entirely into memory as a `String`. For repositories with
very large binary changes (100MB+), this could cause memory pressure.

---

## Debugging Tips

### Inspect Saved Patches

When merge fails, patch is saved to `.codex/patches/<task_id>.diff`:
1. Inspect the patch manually: `cat .codex/patches/*.diff`
2. Try applying manually: `git apply --3way <patch_file>`
3. Check what error git reports
