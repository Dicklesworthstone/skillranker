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
if [ "${1-}" = "--suite" ] && [ "${2-}" = "replay" ]; then
    shift 2
    exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/replay.sh" "$@"
fi
exec /usr/bin/python3 -I -B "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/runner.py" "$@"
