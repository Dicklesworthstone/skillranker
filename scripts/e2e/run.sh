#!/bin/sh
set -eu
set +x
exec /usr/bin/python3 -I -B "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/runner.py" "$@"
