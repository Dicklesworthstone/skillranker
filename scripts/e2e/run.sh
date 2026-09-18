#!/bin/sh
set -eu
set +x
if [ "${1-}" = "--suite" ] && [ "${2-}" = "roster" ]; then
    shift 2
    exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/roster.sh" "$@"
fi
exec /usr/bin/python3 -I -B "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/runner.py" "$@"
