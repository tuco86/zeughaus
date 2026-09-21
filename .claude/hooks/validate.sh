#!/bin/bash
# Stop hook: the pre-push gate, printing only what failed so a clean run costs
# no context.

cd "$CLAUDE_PROJECT_DIR" || exit 0

check_output=$(cargo check --workspace --all-targets 2>&1)
check_status=$?

# Only the editor targets the browser; native-only crates are not expected to.
wasm_output=$(cargo check --target wasm32-unknown-unknown -p zeughaus 2>&1)
wasm_status=$?

test_output=$(cargo test --workspace 2>&1)
test_status=$?

if [ $check_status -ne 0 ]; then
    echo "## cargo check (native) failed"
    echo "$check_output" | grep -E "^error" | head -20
    echo ""
fi

if [ $wasm_status -ne 0 ]; then
    echo "## cargo check (wasm) failed"
    echo "$wasm_output" | grep -E "^error" | head -20
    echo ""
fi

if [ $test_status -ne 0 ]; then
    echo "## cargo test failed"
    echo "$test_output" | grep -E "(FAILED|panicked|error\[)" | head -20
    echo "$test_output" | grep -E "^test .* FAILED" | head -10
    echo ""
fi

# Exit 2 surfaces the report to the agent.
if [ $check_status -ne 0 ] || [ $wasm_status -ne 0 ] || [ $test_status -ne 0 ]; then
    exit 2
fi

exit 0
