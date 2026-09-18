#!/bin/sh
# Execute actual production Rust/CLI context integration, never runner-child scenarios.
set -eu
set +x
if [ "$#" -ne 2 ] || [ "$1" != "--artifacts" ] || [ ! -d "$2" ]; then
    echo 'Usage: scripts/e2e/run.sh --suite context --artifacts EXISTING_DIRECTORY' >&2
    exit 2
fi
artifact_parent=$(CDPATH='' cd -- "$2" && pwd)
artifact_dir=$(mktemp -d "$artifact_parent/sr-context-XXXXXXXX")
project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$project_dir"
printf '%s\n' '{"schema_version":1,"suite":"context","tier":"rust-product-integration","stage":"started"}'
# No test-name filter: all positive/negative tests in these targets must run.
# RCH retains its job/source/worker diagnostics in the files below. Exit 103 is
# infrastructure refusal, never a local fallback or a successful product test.
result=0
RCH_REQUIRE_REMOTE=1 rch --json exec -- cargo test --locked -j 2 \
    --test cass_adapter --test context_contract --test context_failures \
    --test context_overlay_safety --test jsonl_snapshot --test native_identity \
    --test p3_gate --test privacy_contract --test project_signals \
    --test source_selection \
    -- --nocapture > "$artifact_dir/remote-result.json" 2> "$artifact_dir/tests.log" || result=$?
cat "$artifact_dir/remote-result.json"
cat "$artifact_dir/tests.log" >&2
if [ "$result" -ne 0 ]; then
    printf '%s\n' '{"schema_version":1,"suite":"context","status":"failed","product_gate":"not-accepted"}'
    exit "$result"
fi
# Each contract-matrix case passes only if every real test mapped to it ran
# and passed in this log, and the whole run was complete.
cases=0
python3 -I -B scripts/e2e/product_cases.py evaluate scripts/e2e/product/context.json \
    "$artifact_dir/tests.log" > "$artifact_dir/cases.jsonl" || cases=$?
cat "$artifact_dir/cases.jsonl"
if [ "$cases" -ne 0 ]; then
    printf '%s\n' '{"schema_version":1,"suite":"context","status":"failed","product_gate":"not-accepted","reason":"case-evidence"}'
    exit 1
fi
printf '%s\n' '{"schema_version":1,"suite":"context","status":"passed","scope":"exact-session-context-privacy","live_provider":false,"native_harness":false}'
