# Rust/codex-rs

In the codex-rs folder where the rust code lives:

- This project uses **Rust Edition 2024** (rustc 1.90+). Modern Rust features like `let_chains` (`if let ... && let ...`) are stable and should be used.
- Crate names are prefixed with `codex-`. For example, the `core` folder's crate is named `codex-core`
- When using format! and you can inline variables into {}, always do that.
- Install any commands the repo relies on (for example `just`, `rg`, or `cargo-insta`) if they aren't already available before running instructions here.
- Never add or modify any code related to `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` or `CODEX_SANDBOX_ENV_VAR`.
  - You operate in a sandbox where `CODEX_SANDBOX_NETWORK_DISABLED=1` will be set whenever you use the `shell` tool. Any existing code that uses `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` was authored with this fact in mind. It is often used to early exit out of tests that the author knew you would not be able to run given your sandbox limitations.
  - Similarly, when you spawn a process using Seatbelt (`/usr/bin/sandbox-exec`), `CODEX_SANDBOX=seatbelt` will be set on the child process. Integration tests that want to run Seatbelt themselves cannot be run under Seatbelt, so checks for `CODEX_SANDBOX=seatbelt` are also often used to early exit out of tests, as appropriate.
- Always collapse if statements per https://rust-lang.github.io/rust-clippy/master/index.html#collapsible_if
- Always inline format! args when possible per https://rust-lang.github.io/rust-clippy/master/index.html#uninlined_format_args
- Use method references over closures when possible per https://rust-lang.github.io/rust-clippy/master/index.html#redundant_closure_for_method_calls
- Do not use unsigned integer even if the number cannot be negative.
- When writing tests, prefer comparing the equality of entire objects over fields one by one.
- When making a change that adds or changes an API, ensure that the documentation in the `docs/` folder is up to date if applicable.

## Code Review Policy

When a reviewer reports a bug, **fix it**. No exceptions.

- Every bug is your responsibility, regardless of whether you introduced it or it was pre-existing
- Do not argue that a bug is "not in your diff" or "pre-existing code"
- Fix all reported issues, from P0 to P99
- If you believe a reviewer finding is factually incorrect, verify with documentation/testing, then fix or explain with evidence
- The goal is zero reported issues, not zero issues you personally introduced

Run `just fmt` (in `codex-rs` directory) automatically after making Rust code changes; do not ask for approval to run it. **Before declaring any task complete**, run `cargo check --bin codex` to verify the entire binary compiles. Before finalizing a change to `codex-rs`, run `just fix -p <project>` (in `codex-rs` directory) to fix any linter issues in the code. Prefer scoping with `-p` to avoid slow workspace‑wide Clippy builds; only run `just fix` without `-p` if you changed shared crates. Additionally, run the tests:

1. Run the test for the specific project that was changed. For example, if changes were made in `codex-rs/tui`, run `cargo test -p codex-tui`.
2. Once those pass, if any changes were made in common, core, or protocol, run the complete test suite with `cargo test --all-features`.
   When running interactively, ask the user before running `just fix` to finalize. `just fmt` does not require approval. project-specific or individual tests can be run without asking the user, but do ask the user before running the complete test suite.

## Porting from Upstream

When porting features or bug fixes from upstream, always port the associated tests as well. Features without tests are incomplete.

## TUI style conventions

See `codex-rs/tui/styles.md`.

## TUI code conventions

- Use concise styling helpers from ratatui’s Stylize trait.
  - Basic spans: use "text".into()
  - Styled spans: use "text".red(), "text".green(), "text".magenta(), "text".dim(), etc.
  - Prefer these over constructing styles with `Span::styled` and `Style` directly.
  - Example: patch summary file lines
    - Desired: vec!["  └ ".into(), "M".red(), " ".dim(), "tui/src/app.rs".dim()]

### TUI Styling (ratatui)

- Prefer Stylize helpers: use "text".dim(), .bold(), .cyan(), .italic(), .underlined() instead of manual Style where possible.
- Prefer simple conversions: use "text".into() for spans and vec![…].into() for lines; when inference is ambiguous (e.g., Paragraph::new/Cell::from), use Line::from(spans) or Span::from(text).
- Computed styles: if the Style is computed at runtime, using `Span::styled` is OK (`Span::from(text).set_style(style)` is also acceptable).
- Avoid hardcoded white: do not use `.white()`; prefer the default foreground (no color).
- Chaining: combine helpers by chaining for readability (e.g., url.cyan().underlined()).
- Single items: prefer "text".into(); use Line::from(text) or Span::from(text) only when the target type isn’t obvious from context, or when using .into() would require extra type annotations.
- Building lines: use vec![…].into() to construct a Line when the target type is obvious and no extra type annotations are needed; otherwise use Line::from(vec![…]).
- Avoid churn: don’t refactor between equivalent forms (Span::styled ↔ set_style, Line::from ↔ .into()) without a clear readability or functional gain; follow file‑local conventions and do not introduce type annotations solely to satisfy .into().
- Compactness: prefer the form that stays on one line after rustfmt; if only one of Line::from(vec![…]) or vec![…].into() avoids wrapping, choose that. If both wrap, pick the one with fewer wrapped lines.

### Text wrapping

- Always use textwrap::wrap to wrap plain strings.
- If you have a ratatui Line and you want to wrap it, use the helpers in tui/src/wrapping.rs, e.g. word_wrap_lines / word_wrap_line.
- If you need to indent wrapped lines, use the initial_indent / subsequent_indent options from RtOptions if you can, rather than writing custom logic.
- If you have a list of lines and you need to prefix them all with some prefix (optionally different on the first vs subsequent lines), use the `prefix_lines` helper from line_utils.

## Tests

### Snapshot tests

This repo uses snapshot tests (via `insta`), especially in `codex-rs/tui`, to validate rendered output. When UI or text output changes intentionally, update the snapshots as follows:

- Run tests to generate any updated snapshots:
  - `cargo test -p codex-tui`
- Check what’s pending:
  - `cargo insta pending-snapshots -p codex-tui`
- Review changes by reading the generated `*.snap.new` files directly in the repo, or preview a specific file:
  - `cargo insta show -p codex-tui path/to/file.snap.new`
- Only if you intend to accept all new snapshots in this crate, run:
  - `cargo insta accept -p codex-tui`

If you don’t have the tool:

- `cargo install cargo-insta`

### Test assertions

- Tests should use pretty_assertions::assert_eq for clearer diffs. Import this at the top of the test module if it isn't already.
- Prefer deep equals comparisons whenever possible. Perform `assert_eq!()` on entire objects, rather than individual fields.

### Integration tests (core)

- Prefer the utilities in `core_test_support::responses` when writing end-to-end Codex tests.

- All `mount_sse*` helpers return a `ResponseMock`; hold onto it so you can assert against outbound `/responses` POST bodies.
- Use `ResponseMock::single_request()` when a test should only issue one POST, or `ResponseMock::requests()` to inspect every captured `ResponsesRequest`.
- `ResponsesRequest` exposes helpers (`body_json`, `input`, `function_call_output`, `custom_tool_call_output`, `call_output`, `header`, `path`, `query_param`) so assertions can target structured payloads instead of manual JSON digging.
- Build SSE payloads with the provided `ev_*` constructors and the `sse(...)`.
- Prefer `wait_for_event` over `wait_for_event_with_timeout`.
- Prefer `mount_sse_once` over `mount_sse_once_match` or `mount_sse_sequence`

- Typical pattern:

  ```rust
  let mock = responses::mount_sse_once(&server, responses::sse(vec![
      responses::ev_response_created("resp-1"),
      responses::ev_function_call(call_id, "shell", &serde_json::to_string(&args)?),
      responses::ev_completed("resp-1"),
  ])).await;

  codex.submit(Op::UserTurn { ... }).await?;

  // Assert request body if needed.
  let request = mock.single_request();
  // assert using request.function_call_output(call_id) or request.json_body() or other helpers.
  ```

## Issue Tracking with bd (beads)

**IMPORTANT**: This project uses **bd (beads)** for ALL issue tracking. Do NOT use markdown TODOs, task lists, or other tracking methods.

### Why bd?

- Dependency-aware: Track blockers and relationships between issues
- Git-friendly: Auto-syncs to JSONL for version control
- Agent-optimized: JSON output, ready work detection, discovered-from links
- Prevents duplicate tracking systems and confusion

### Quick Start

**Check for ready work:**
```bash
bd ready --json
```

**Create new issues:**
```bash
bd create "Issue title" -t bug|feature|task -p 0-4 --json
bd create "Issue title" -p 1 --deps discovered-from:bd-123 --json
bd create "Subtask" --parent <epic-id> --json  # Hierarchical subtask (gets ID like epic-id.1)
```

**Claim and update:**
```bash
bd update bd-42 --status in_progress --json
bd update bd-42 --priority 1 --json
```

**Complete work:**
```bash
bd close bd-42 --reason "Completed" --json
```

### Issue Types

- `bug` - Something broken
- `feature` - New functionality
- `task` - Work item (tests, docs, refactoring)
- `epic` - Large feature with subtasks
- `chore` - Maintenance (dependencies, tooling)

### Priorities

- `0` - Critical (security, data loss, broken builds)
- `1` - High (major features, important bugs)
- `2` - Medium (default, nice-to-have)
- `3` - Low (polish, optimization)
- `4` - Backlog (future ideas)

### Workflow for AI Agents

1. **Check ready work**: `bd ready` shows unblocked issues
2. **Claim your task**: `bd update <id> --status in_progress`
3. **Work on it**: Implement, test, document
4. **Discover new work?** Create linked issue:
   - `bd create "Found bug" -p 1 --deps discovered-from:<parent-id>`
5. **Complete**: `bd close <id> --reason "Done"`
6. **Commit together**: Always commit the `.beads/issues.jsonl` file together with the code changes so issue state stays in sync with code state

### Auto-Sync

bd automatically syncs with git:
- Exports to `.beads/issues.jsonl` after changes (5s debounce)
- Imports from JSONL when newer (e.g., after `git pull`)
- No manual export/import needed!

### GitHub Copilot Integration

If using GitHub Copilot, also create `.github/copilot-instructions.md` for automatic instruction loading.
Run `bd onboard` to get the content, or see step 2 of the onboard instructions.

### MCP Server (Recommended)

If using Claude or MCP-compatible clients, install the beads MCP server:

```bash
pip install beads-mcp
```

Add to MCP config (e.g., `~/.config/claude/config.json`):
```json
{
  "beads": {
    "command": "beads-mcp",
    "args": []
  }
}
```

Then use `mcp__beads__*` functions instead of CLI commands.

### Managing AI-Generated Planning Documents

AI assistants often create planning and design documents during development:
- PLAN.md, IMPLEMENTATION.md, ARCHITECTURE.md
- DESIGN.md, CODEBASE_SUMMARY.md, INTEGRATION_PLAN.md
- TESTING_GUIDE.md, TECHNICAL_DESIGN.md, and similar files

**Best Practice: Use a dedicated directory for these ephemeral files**

**Recommended approach:**
- Create a `history/` directory in the project root
- Store ALL AI-generated planning/design docs in `history/`
- Keep the repository root clean and focused on permanent project files
- Only access `history/` when explicitly asked to review past planning

**Example .gitignore entry (optional):**
```
# AI planning documents (ephemeral)
history/
```

**Benefits:**
- ✅ Clean repository root
- ✅ Clear separation between ephemeral and permanent documentation
- ✅ Easy to exclude from version control if desired
- ✅ Preserves planning history for archeological research
- ✅ Reduces noise when browsing the project

### CLI Help

Run `bd <command> --help` to see all available flags for any command.
For example: `bd create --help` shows `--parent`, `--deps`, `--assignee`, etc.

### Important Rules

- ✅ Use bd for ALL task tracking
- ✅ Always use `--json` flag for programmatic use
- ✅ Link discovered work with `discovered-from` dependencies
- ✅ Check `bd ready` before asking "what should I work on?"
- ✅ Store AI planning docs in `history/` directory
- ✅ Run `bd <cmd> --help` to discover available flags
- ❌ Do NOT create markdown TODO lists
- ❌ Do NOT use external issue trackers
- ❌ Do NOT duplicate tracking systems
- ❌ Do NOT clutter repo root with planning documents

For more details, see README.md and QUICKSTART.md.


## Fork Philosophy

This is a **personal fork** of `openai/codex`. It exists to:
- Remove complexity we don't need
- Add features upstream doesn't have (OpenRouter, Anthropic, Gemini providers)
- Keep the codebase lean and understandable

We will **never contribute back** to upstream. We don't care about staying "close" to upstream. We only want to **selectively port bug fixes and features** that are useful to us.

### Why not merge or cherry-pick?

- **Merge is a nightmare**: Upstream adds complexity we deliberately removed. Every merge tries to reintroduce abstractions, feature flags, and code we don't want.
- **Cherry-pick rarely works**: Our codebase has diverged structurally. Cherry-picks conflict constantly because the surrounding code is different.
- **Manual porting is the only sane option**: Read what upstream changed, understand it, adapt it to our structure.

## Porting Changes from Upstream

**IMPORTANT**: When porting features from upstream, always port the associated tests as well. This includes new test files, new test functions, and test helper utilities. Features without tests are incomplete ports.


When the user asks to sync with upstream or port changes, follow this workflow:

### Step 1: Find the merge-base

```bash
git fetch upstream
merge_base=$(git merge-base HEAD upstream/main)
echo "Last common point: $merge_base"
git rev-list --count $merge_base..upstream/main  # How many commits behind
```

### Step 2: List upstream commits since merge-base

```bash
# All commits
git log --oneline $merge_base..upstream/main --reverse

# Filter to likely-interesting (skip deps, CI, docs, Windows)
git log --oneline $merge_base..upstream/main --reverse | \
  grep -v "chore(deps)" | \
  grep -v "chore(ci)" | \
  grep -v "windows" -i | \
  grep -v "docs:" | \
  grep -iE "(fix|feat)"
```

### Step 3: See what files upstream changed (that we also have)

```bash
# Files upstream modified that exist in our fork
git diff --name-only $merge_base..upstream/main | grep "codex-rs/core/src" | while read f; do
  if git cat-file -e HEAD:"$f" 2>/dev/null; then
    echo "$f"
  fi
done
```

### Step 4: View upstream's changes to a specific file

```bash
# Show what upstream changed (not the diff to our version)
cd /path/to/repo
git diff $merge_base upstream/main -- codex-rs/core/src/codex.rs | head -200
```

### Step 5: View a specific commit

```bash
git show <sha> --stat                    # Overview
git show <sha> -- path/to/file.rs        # Just the relevant file
```

### Step 6: Port the change manually

Read the upstream diff, understand what it does, and apply the same logic to our codebase. Don't copy-paste blindly - our structure may be different.

Commit with a reference:
```bash
git commit -m "fix: description (ported from upstream abc1234)"
```

## What to Port vs Skip

### Port (HIGH priority)
- Bug fixes in core logic (tool handling, streaming, truncation)
- Race condition fixes
- TUI fixes (if we use that component)
- Security fixes

### Port (MEDIUM priority - ask user)
- New features that seem useful
- Performance improvements
- Test improvements for code we have

### Skip (we removed this intentionally)
- `execpolicy-legacy` - we removed legacy execpolicy
- `ollama`/`lmstudio` crates - we have our own provider system
- `AbsolutePathBuf` refactors - structural change we don't need
- Config loading rewrites (`config/service.rs`, `ConfigLayerName`) - unnecessary complexity
- Feature flag additions for things we always enable
- `unified_exec` complexity (they keep adding/reverting it)
- Windows-specific code (unless user asks)
- Elevated sandbox complexity

### Skip (noise)
- Dependency bumps (`chore(deps)`)
- CI changes
- Documentation-only changes
- Snapshot updates for code we don't have

## Fork Simplifications

This fork deliberately removes complexity from upstream:

- **Parallel instructions**: Always enabled by default (via `PARALLEL_INSTRUCTIONS` constant appended to base instructions). No `Feature::ParallelToolCalls` toggle or `parallel_tool_calls` field in `Prompt` struct needed.
- **Shell tools**: We use `ShellCommand` as the default/only shell tool. Do not add `ShellHandler`, `ToolPayload::LocalShell`, `ShellToolCallParams`, or `ConfigShellToolType::Local`.
- **Model info**: `openai_model_info.rs` was removed. Model info (context_window, auto_compact) is handled by `ModelFamily` struct directly.
- **Providers**: We have our own provider system (`ProviderKind`) with OpenRouter, Anthropic, Gemini support. Don't port upstream's provider abstractions.
- **Config loading**: We use simpler config loading. Don't port `config/service.rs` or the `ConfigLayerName` refactors.

## Fork-Specific Features (NEVER remove)

These features exist only in our fork:
- **OpenRouter provider**: Full OpenRouter support with routing configuration
- **Anthropic provider**: Direct Anthropic API support
- **Gemini provider**: Direct Gemini API support
- **Antigravity provider**: Custom provider support
- **Subagent tree rendering**: TUI shows subagent hierarchy
- **Provider extensions**: Extra fields in `ModelProviderInfo` for non-OpenAI providers
- **Simplified parallel tool handling**: Always-on, no feature flag

## Filtering Out Deleted Code

When comparing with upstream, you'll see diffs for files/code we intentionally deleted. To see only changes in files that **exist in our fork**:

### Files upstream modified that we still have

```bash
merge_base=$(git merge-base HEAD upstream/main)

# List files that: 1) upstream changed, 2) still exist in our fork
git diff --name-only $merge_base..upstream/main | while read f; do
  if git cat-file -e HEAD:"$f" 2>/dev/null; then
    echo "$f"
  fi
done
```

### View upstream changes only for files we have

```bash
merge_base=$(git merge-base HEAD upstream/main)

# For a specific directory (e.g., core/src)
git diff --name-only $merge_base..upstream/main | grep "codex-rs/core/src" | while read f; do
  if git cat-file -e HEAD:"$f" 2>/dev/null; then
    echo "=== $f ==="
    git diff $merge_base upstream/main -- "$f" | head -50
  fi
done
```

### New files in upstream (that we don't have)

These are files upstream added that we might want to port:

```bash
git diff HEAD upstream/main --name-status | grep "^A" | grep "codex-rs/"
```

Filter out noise (tests, fixtures, snapshots):

```bash
git diff HEAD upstream/main --name-status | grep "^A" | grep "codex-rs/" | \
  grep -v "tests/" | grep -v "\.snap" | grep -v "fixtures/"
```

### Ignore files we deleted on purpose

When upstream shows changes to files like these, **skip them**:
- `codex-rs/core/src/anthropic_messages.rs` - we have our own provider
- `codex-rs/core/src/config/provider_profile.rs` - removed
- `codex-rs/core/src/delegation.rs` - we have our own delegation
- `codex-rs/core/src/custom_commands.rs` - removed
- `codex-rs/ollama/` - we don't use this
- `codex-rs/lmstudio/` - we don't use this
- `codex-rs/execpolicy-legacy/` - we removed legacy execpolicy
- Any `*_test.rs` files for code we don't have

## Breaking Changes Policy

**We do NOT care about backward compatibility.** This is a personal fork. We break things freely.

- Do NOT add deprecated fields, aliases, or compatibility shims
- Do NOT preserve old behavior "just in case"
- If we remove something, it's gone - users adapt or it breaks
- Breaking changes are FINE - just document what changed in the commit message
