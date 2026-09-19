#!/bin/sh
# The parser-corpus suite is catalog-driven product evidence: pure parser,
# property, and bounded fuzz regressions across untrusted input boundaries.
# Targets and the case-to-test map live in scripts/e2e/product/parser-corpus.json.
set -eu
set +x
if [ "$#" -ne 2 ] || [ "$1" != "--artifacts" ] || [ ! -d "$2" ]; then
    echo 'Usage: scripts/e2e/run.sh --suite parser-corpus --artifacts EXISTING_DIRECTORY' >&2
    exit 2
fi
exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/product.sh" --suite parser-corpus --artifacts "$2"
