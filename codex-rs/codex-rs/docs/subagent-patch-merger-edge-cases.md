# Subagent Patch Merger: Edge Cases and Failure Scenarios

This document provides a comprehensive analysis of potential edge cases and failure scenarios
in the subagent worktree isolation and patch merger system.

## Architecture Overview

The system uses git worktrees for subagent isolation:

1. **Worktree Creation** (`WorktreeManager::create_worktree`):
   - Captures dirty state via `git stash create -u` (creates unreferenced commit)
   - Creates detached worktree at that commit
   - Subagent runs in isolated worktree

2. **Diff Generation** (`WorktreeManager::generate_diff`):
   - Stages all changes with `git add -A`
   - Generates diff with `git diff --binary --cached <base_sha>`

3. **Patch Application** (`WorktreeManager::apply_patch`):
   - Try clean `git apply` (fast path, atomic)
   - Fall back to `git apply --3way` (handles concurrent changes)
   - Detect conflicts via `git diff --check`
   - Save patch file on hard failure

4. **Locking**:
   - `repo_lock` mutex prevents parallel merges within the same turn
   - Parallel subagents from the same turn serialize correctly

---

## Confirmed Issues

### 1. Index Lock Contention (HIGH - Likely Cause of Intermittent Failures)

**Scenario**: User runs git commands (commit, pull, etc.) while subagent is merging.

**Mechanism**:
1. Subagent finishes and tries to `git apply`
2. User's terminal holds `.git/index.lock`
3. `git apply` fails immediately with "Unable to create index.lock"
4. Code falls through to `--3way` which also fails
5. `detect_conflict_markers` finds nothing (nothing was written)
6. **Result**: `PatchApplyResult::Conflict` - patch saved, user sees "merge failed"

**Fix**: Implement retry loop with backoff for `git apply` when stderr contains "index.lock".

```rust
// Pseudo-code for fix
for attempt in 0..3 {
    let output = git_apply().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("index.lock") || stderr.contains("Unable to create") {
            tokio::time::sleep(Duration::from_millis(100 * (attempt + 1))).await;
            continue;
        }
    }
    break;
}
```

---

### 2. Filename Parsing Bug (MEDIUM)

**Scenario**: A file named `data:test.json` has a merge conflict.

**Mechanism**:
1. `git diff --check` outputs: `data:test.json:1: leftover conflict marker`
2. `detect_conflict_markers` uses `line.split(':').next()`
3. Returns `"data"` instead of `"data:test.json"`

**Impact**: UI shows wrong filename; actual file still has markers.

**Fix**: Parse from the right side or use regex:
```rust
// Better parsing: find the last two colons
if let Some(pos) = line.rfind(": leftover conflict marker") {
    let prefix = &line[..pos];
    if let Some(colon_pos) = prefix.rfind(':') {
        return Some(prefix[..colon_pos].to_string());
    }
}
```

---

### 3. Binary File Stats Display (LOW)

**Scenario**: Subagent modifies a binary file (image, compiled asset).

**Mechanism**:
1. `git diff --numstat` outputs `-\t-\timage.png` for binary files
2. `parts[0].parse::<i32>().unwrap_or(0)` returns 0
3. UI shows `image.png (+0/-0)` despite actual changes

**Fix**: Check for `-` explicitly:
```rust
let insertions = if parts[0] == "-" { -1 } else { parts[0].parse().unwrap_or(0) };
// Or use Option<i32> to represent "binary file"
```

---

### 4. Modify/Delete Conflicts (MEDIUM)

**Scenario**: User modifies `file.txt`; subagent deletes it.

**Mechanism**:
1. `git apply` fails (deletion vs modification conflict)
2. `git apply --3way` fails but does NOT insert `<<<<<<<` markers
3. `detect_conflict_markers` finds nothing
4. **Result**: Hard failure (patch saved)

**Impact**: This is actually correct behavior - prevents silent data loss. The user gets the patch
file and can decide manually. No fix needed, but could improve messaging.

---

### 5. Partial Patch Application State (MEDIUM)

**Scenario**: 10-file patch, 9 apply cleanly, 1 fails completely (no markers).

**Mechanism**:
1. `git apply --3way` partially succeeds
2. 9 files are modified in working tree
3. 1 file fails without generating markers (e.g., delete conflict)
4. `detect_conflict_markers` returns empty
5. **Result**: "Conflict - patch saved" but 9 files are already modified!

**Impact**: User might manually apply the saved patch, duplicating the 9 changes.

**Potential Fix**: After `--3way` failure, check if any files were modified:
```rust
// If git apply --3way failed without markers, check actual state
let status_output = Command::new("git")
    .args(["status", "--porcelain"])
    .current_dir(target_dir)
    .output().await?;

if !status_output.stdout.is_empty() {
    // Warn user that partial changes were applied
}
```

---

### 6. UTF-8 Encoding Issues (LOW)

**Scenario**: Repository has text files with non-UTF8 encoding (ISO-8859-1, etc.).

**Mechanism**:
1. `git diff --binary` outputs raw bytes
2. `String::from_utf8_lossy` replaces invalid sequences with U+FFFD ()
3. Patch context is corrupted
4. `git apply` fails because context doesn't match file content

**Impact**: Rare (most modern repos use UTF-8), but could cause "patch saved" for international users.

**Fix**: Store patch as `Vec<u8>` instead of `String`:
```rust
// Instead of String::from_utf8_lossy
let patch_bytes: Vec<u8> = diff_output.stdout;
tokio::fs::write(&patch_file, &patch_bytes).await?;
```

---

## Verified Safe Scenarios

### Race Condition: Parent Modifies Files During Subagent Run

**Scenario**: User edits `file.txt` while subagent is working.

**Why it's safe**:
1. base_sha = stash commit with original dirty state
2. Subagent works on worktree copy
3. When merging, `git apply` fails (context mismatch)
4. `git apply --3way` correctly uses base_sha as common ancestor
5. Git performs proper 3-way merge
6. **Result**: Either clean merge or conflict markers

### Parallel Subagents from Same Turn

**Scenario**: Main agent spawns subagent A and B in parallel.

**Why it's safe**:
1. Both share the same `invocation.turn.repo_lock`
2. When A finishes, it acquires lock and merges
3. When B finishes, it waits for lock, then merges
4. B's `git apply --3way` handles A's changes correctly

### Nested Subagents

**Scenario**: Subagent A spawns subagent B.

**Why it's safe**:
1. A's worktree created from main workspace
2. B's worktree created from A's worktree
3. B merges to A's worktree (using A's repo_lock)
4. A merges to main workspace (using main's repo_lock)
5. Changes bubble up correctly

### Stash Commit Garbage Collection

**Scenario**: `git gc` runs while subagent is active.

**Why it's safe**:
1. `git stash create` creates dangling commit
2. Worktree HEAD references this commit
3. Commit is reachable while worktree exists
4. GC won't delete it

### Git Config Inheritance (autocrlf, etc.)

**Scenario**: User has `core.autocrlf=true`.

**Why it's safe**:
1. Worktree inherits git config
2. Both diff generation and apply use same config
3. Line endings are normalized consistently

---

## Known Limitations (By Design)

### Fresh Repositories (No Commits)

Subagents cannot run in repositories with zero commits. `git rev-parse HEAD` fails.
This is documented behavior - users must make at least one commit first.

### Git Submodules

Submodules are NOT initialized in worktrees. Subagents see empty directories
for submodule paths. This could cause build failures in projects that depend on submodules.

**Workaround**: Run `git submodule update --init --recursive` in subagent prompt,
but this is slow and may have side effects.

### Large Diffs (Memory)

The patch is loaded entirely into memory as a `String`. For repositories with
very large binary changes (100MB+), this could cause memory pressure.

### External File Changes

Files modified outside the repository (e.g., `/tmp`, `~/.config`) are applied
immediately and are NOT tracked by the merge system. This is documented in
`WORKTREE_ISOLATION_PROMPT`.

---

## Debugging Tips

### Check for Lock Contention

If seeing "patch saved" errors intermittently:
1. Check if user is running git commands simultaneously
2. Look for `index.lock` in error messages (may be truncated)
3. Retry the operation - if it succeeds, it was lock contention

### Check for Encoding Issues

If patches fail consistently for specific files:
1. Check file encoding: `file <filename>`
2. If not UTF-8, the patch may be corrupted
3. Convert to UTF-8 or accept this limitation

### Inspect Saved Patches

When merge fails, patch is saved to `.codex/patches/<task_id>.diff`:
1. Inspect the patch manually: `cat .codex/patches/*.diff`
2. Try applying manually: `git apply --3way <patch_file>`
3. Check what error git reports

### Check Partial Application

If seeing unexpected file states after "merge failed":
1. Run `git status` to see what was modified
2. Run `git diff` to see actual changes
3. Some hunks may have been applied before failure

---

## Recommended Fixes (Priority Order)

1. **HIGH**: Add retry loop for `index.lock` contention
2. **MEDIUM**: Fix filename parsing in `detect_conflict_markers`
3. **LOW**: Use `Vec<u8>` for patch storage to preserve encoding
4. **LOW**: Add "partial apply" detection and warning
5. **LOW**: Handle binary file stats display
