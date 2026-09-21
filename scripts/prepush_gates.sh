#!/usr/bin/env bash
# Run every gate AGENTS.md lists, report each one's own status, and fail if any fails.
#
# This exists because the gates are not hard to run, they are easy to run incompletely. In a
# single day main was pushed red on compilation (a test file calling a changed signature,
# invisible to anything short of --all-targets), red on `cargo fmt --check` twice, red on
# `clippy -D warnings`, and at least one agent — the author of this script — pushed three
# commits without running `ubs --diff` at all. Four of five gates plus a green claim looks
# exactly like five of five.
#
# It reports each gate separately and never infers a gate's result from a pipeline's exit
# status. That specific mistake produced a "clippy exit 0" receipt on this project while clippy
# was failing with four errors, because the last command in the pipeline was an `echo`.
#
# Retire this script when DSR runs these gates on push. It gates nothing by itself; it is a
# way to run what AGENTS.md already requires without forgetting a quarter of it.
#
# Usage:
#   scripts/prepush_gates.sh            # all gates
#   scripts/prepush_gates.sh --fast     # skip the full test run (still fmt/check/clippy/ubs)
#   PREPUSH_GATES_LOCAL=1 scripts/prepush_gates.sh   # never use rch, even if present
#
# The variable is deliberately NOT named SR_*: src/config.rs treats that prefix as a strict
# namespace, so an unrecognised SR_ variable makes every `sr` invocation fail with
# "unknown setting" — including the ones the test suite spawns. This script found that by
# failing its own `cargo test` gate when the variable was called SR_GATES_LOCAL.
#
# Honors RCH when available, per AGENTS.md: "In standalone environments without RCH, the
# underlying Cargo commands are the gates."

set -uo pipefail

cd "$(dirname "$0")/.."
ROOT=$(pwd)

FAST=0
for arg in "$@"; do
  case "$arg" in
    --fast) FAST=1 ;;
    -h|--help) sed -n '1,29p' "$0"; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

# rch is for expensive builds on the shared fleet. fmt, ubs and the Python validators are
# local by nature and are never wrapped.
if [ "${PREPUSH_GATES_LOCAL:-0}" = "1" ] || ! command -v rch >/dev/null 2>&1; then
  CARGO_PREFIX=()
  LANE="local cargo"
else
  CARGO_PREFIX=(rch exec --)
  LANE="rch exec"
fi

LAST_LOG=""
FAILED=()
PASSED=()
SKIPPED=()
REVIEW=()

# Runs one gate, prints its verdict, and records it. The exit status captured is the command's
# own, never a pipeline's: output goes to a file and is shown only on failure, so a gate cannot
# be reported green because something downstream of it succeeded.
gate() {
  local name="$1"; shift
  local log
  log=$(mktemp "${TMPDIR:-/tmp}/sr-gate-XXXXXX.log")
  LAST_LOG="$log"
  printf '\n=== %s ===\n' "$name"
  if "$@" >"$log" 2>&1; then
    # Kept on success, not only on failure. A passing gate is the one you quote, and a receipt
    # that says "tests passed" without the counts is the kind of claim this script exists to
    # discourage.
    printf '  PASS  %s  (output: %s)\n' "$name" "$log"
    PASSED+=("$name")
  else
    local status=$?
    printf '  FAIL  %s (exit %d)\n' "$name" "$status"
    printf -- '  --- last 40 lines of output ---\n'
    tail -40 "$log" | sed 's/^/  /'
    printf -- '  --- full output: %s ---\n' "$log"
    FAILED+=("$name")
    return 0
  fi
}

printf 'Gates for %s\n' "$ROOT"
printf 'Build lane: %s\n' "$LANE"

gate "cargo fmt --check" cargo fmt --check
gate "cargo check --locked --all-targets" "${CARGO_PREFIX[@]}" cargo check --locked --all-targets
gate "cargo clippy --locked --all-targets -- -D warnings" \
  "${CARGO_PREFIX[@]}" cargo clippy --locked --all-targets -- -D warnings

if [ "$FAST" = "1" ]; then
  printf '\n=== cargo test --locked ===\n  SKIP  requested with --fast\n'
  SKIPPED+=("cargo test --locked")
else
  gate "cargo test --locked" "${CARGO_PREFIX[@]}" cargo test --locked
  # The aggregate across every target, printed so a receipt can carry counts rather than a word.
  if [ -f "$LAST_LOG" ]; then
    tr '\r' '\n' <"$LAST_LOG" | awk '
      match($0, /([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored/, m) {
        n++; p += m[1]; f += m[2]; i += m[3]
      }
      END { if (n > 0) printf "  %d report(s) | %d passed | %d failed | %d ignored\n", n, p, f, i }
    '
  fi
fi

if command -v ubs >/dev/null 2>&1; then
  # Reported for triage rather than treated as pass/fail, deliberately. `ubs --diff` scans every
  # file the diff touches, not only the lines it changed, so appending one test to a file makes
  # you the messenger for every pre-existing finding in it. Dogfooding this script produced
  # exactly that: a critical for a `panic!` in a fixture written by someone else months earlier.
  # A gate that fails you for another author's line is a gate people learn to skip, which costs
  # more than it catches. So criticals are printed in full and you attribute them.
  #
  # A critical on a line you wrote is not advisory. Fix it, as 7ebbf82 did.
  printf '\n=== ubs --diff ===\n'
  UBS_LOG=$(mktemp "${TMPDIR:-/tmp}/sr-gate-ubs-XXXXXX.log")
  ubs --diff >"$UBS_LOG" 2>&1
  if grep -qE '^Critical: [1-9]' "$UBS_LOG"; then
    printf '  REVIEW  ubs --diff reports criticals in the files you touched\n'
    grep -E 'CRITICAL|rule:' "$UBS_LOG" | head -30 | sed 's/^/  /'
    printf -- '  --- full output: %s ---\n' "$UBS_LOG"
    printf '  Attribute each one before you quote a result: a finding on a line you wrote is\n'
    printf '  yours to fix, and one that predates you is yours to name, not to suppress.\n'
    REVIEW+=("ubs --diff")
  else
    printf '  PASS  ubs --diff (no criticals in changed files)\n'
    PASSED+=("ubs --diff")
    rm -f "$UBS_LOG"
  fi
else
  printf '\n=== ubs --diff ===\n  SKIP  ubs not on PATH\n'
  SKIPPED+=("ubs --diff")
fi

gate "validate_public_contracts.py" python3 scripts/validate_public_contracts.py

printf '\n========================================\n'
printf 'passed:  %d\n' "${#PASSED[@]}"
printf 'failed:  %d\n' "${#FAILED[@]}"
printf 'review:  %d\n' "${#REVIEW[@]}"
printf 'skipped: %d\n' "${#SKIPPED[@]}"
if [ "${#SKIPPED[@]}" -gt 0 ]; then
  printf '\nSkipped gates are not passed gates. If you report a result from this run, name them:\n'
  for s in "${SKIPPED[@]}"; do printf '  - %s\n' "$s"; done
fi
if [ "${#REVIEW[@]}" -gt 0 ]; then
  printf '\nNeeds triage, and not passed until you have done it:\n'
  for r in "${REVIEW[@]}"; do printf '  - %s\n' "$r"; done
fi
if [ "${#FAILED[@]}" -gt 0 ]; then
  printf '\nFAILED:\n'
  for f in "${FAILED[@]}"; do printf '  - %s\n' "$f"; done
  exit 1
fi
if [ "${#REVIEW[@]}" -gt 0 ] || [ "${#SKIPPED[@]}" -gt 0 ]; then
  printf '\nNothing failed, but this run is not a clean sweep: say what was skipped or left to\n'
  printf 'triage when you report it, and quote the revision you ran it at.\n'
else
  printf '\nAll gates passed. Quote the revision you ran them at, not just the result.\n'
fi
