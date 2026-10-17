#!/bin/sh
set -eu
set +x
if [ "${1-}" = "--suite" ] && [ "${2-}" = "roster" ]; then
    shift 2
    exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/roster.sh" "$@"
fi
if [ "${1-}" = "--suite" ] && [ "${2-}" = "context" ]; then
    shift 2
    exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/context.sh" "$@"
fi
if [ "${1-}" = "--suite" ] && [ "${2-}" = "core-cli" ]; then
    shift 2
    exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/core-cli.sh" "$@"
fi
if [ "${1-}" = "--suite" ] && [ "${2-}" = "cache" ]; then
    shift 2
    exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/cache.sh" "$@"
fi
if [ "${1-}" = "--suite" ] && [ "${2-}" = "parser-corpus" ]; then
    shift 2
    exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/parser-corpus.sh" "$@"
fi
if [ "${1-}" = "--suite" ] && { [ "${2-}" = "replay" ] || [ "${2-}" = "evaluation" ]; }; then
    shift 2
    exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/replay.sh" "$@"
fi
if [ "${1-}" = "--suite" ] && [ "${2-}" = "shadow-hook" ]; then
    shift 2
    exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/shadow-hook.sh" "$@"
fi
if [ "${1-}" = "--suite" ] && { [ "${2-}" = "rollout" ] || [ "${2-}" = "advisory-trial" ] || [ "${2-}" = "relevance-promotion" ] || [ "${2-}" = "availability-promotion" ] || [ "${2-}" = "usability-trial" ]; }; then
    shift 2
    exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/rollout.sh" "$@"
fi
if [ "${1-}" = "--suite" ] && [ "${2-}" = "learning" ]; then
    shift 2
    exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/learning.sh" "$@"
fi
if [ "${1-}" = "--suite" ] && { [ "${2-}" = "passages" ] || [ "${2-}" = "tui" ] || [ "${2-}" = "experiments" ]; }; then
    suite_name="$2"
    shift 2
    exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/p9_suite.sh" "$suite_name" "$@"
fi
exec /usr/bin/python3 -I -B "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/runner.py" "$@"
