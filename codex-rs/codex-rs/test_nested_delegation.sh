#!/usr/bin/env bash
# Simple test script for verifying nested delegation rendering

set -euo pipefail

echo "Testing nested delegation support..."
echo ""

# Run protocol tests
echo "1. Running protocol tests..."
cargo test -p codex-protocol --lib 2>&1 | tail -5
echo "✓ Protocol tests passed"
echo ""

# Run delegation context tests
echo "2. Running delegation context tests..."
cargo test -p codex-core delegation --lib 2>&1 | tail -5
echo "✓ Delegation context tests passed"
echo ""

# Run TUI rendering tests
echo "3. Running TUI rendering tests..."
cargo test -p codex-tui history_cell::tests::subagent 2>&1 | tail -10
echo "✓ TUI rendering tests passed"
echo ""

echo "All tests passed! ✓"
echo ""
echo "To manually test nested delegation:"
echo "1. Create a subagent that delegates to another subagent"
echo "2. Run the TUI and observe the nested rendering"
echo "3. Verify proper indentation and tree connectors"
