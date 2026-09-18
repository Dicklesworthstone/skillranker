#!/bin/sh
# Run a product e2e suite: every Rust integration target in its catalog,
# remotely through RCH, then evaluate each contract-matrix case from the actual
# log. Never runner-child scenarios. See docs/product-e2e-cases.md.
set -eu
set +x
usage() {
    echo 'Usage: scripts/e2e/product.sh --suite NAME --artifacts EXISTING_DIRECTORY' >&2
    exit 2
}
if [ "$#" -ne 4 ] || [ "$1" != "--suite" ] || [ "$3" != "--artifacts" ] || [ ! -d "$4" ]; then
    usage
fi
suite=$2
case "$suite" in '' | *[!a-z0-9-]*) usage ;; esac
project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
catalog="scripts/e2e/product/$suite.json"
[ -f "$project_dir/$catalog" ] || usage
artifact_parent=$(CDPATH='' cd -- "$4" && pwd)
artifact_dir=$(mktemp -d "$artifact_parent/sr-$suite-XXXXXXXX")
cd "$project_dir"
# A stale or incomplete mapping fails before anything runs.
python3 -I -B scripts/e2e/product_cases.py check "$catalog"
set --
for target in $(python3 -I -B scripts/e2e/product_cases.py targets "$catalog"); do
    set -- "$@" --test "$target"
done
printf '{"schema_version":1,"suite":"%s","tier":"rust-product-integration","stage":"started"}\n' "$suite"
# No test-name filter: every positive and negative test in these targets runs.
# RCH keeps its job/source/worker diagnostics in the files below. Exit 103 is an
# infrastructure refusal, never a local fallback or a successful product test.
result=0
RCH_REQUIRE_REMOTE=1 rch --json exec -- cargo test --locked -j 2 "$@" -- --nocapture \
    > "$artifact_dir/remote-result.json" 2> "$artifact_dir/tests.log" || result=$?
cat "$artifact_dir/remote-result.json"
cat "$artifact_dir/tests.log" >&2
if [ "$result" -ne 0 ]; then
    printf '{"schema_version":1,"suite":"%s","status":"failed","product_gate":"not-accepted"}\n' "$suite"
    exit "$result"
fi
# Each case passes only if every real test mapped to it ran and passed in this
# log, and the whole run was complete.
cases=0
python3 -I -B scripts/e2e/product_cases.py evaluate "$catalog" "$artifact_dir/tests.log" \
    > "$artifact_dir/cases.jsonl" || cases=$?
cat "$artifact_dir/cases.jsonl"
if [ "$cases" -ne 0 ]; then
    printf '{"schema_version":1,"suite":"%s","status":"failed","product_gate":"not-accepted","reason":"case-evidence"}\n' "$suite"
    exit 1
fi
printf '{"schema_version":1,"suite":"%s","status":"passed","live_provider":false,"native_harness":false}\n' "$suite"
