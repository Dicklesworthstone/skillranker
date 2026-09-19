#!/bin/sh
# The core-cli suite is catalog-driven product evidence: actual production
# Rust/CLI integration, never runner-child scenarios. The targets and the
# case-to-test map live in scripts/e2e/product/core-cli.json.
set -eu
set +x
if [ "$#" -ne 2 ] || [ "$1" != "--artifacts" ] || [ ! -d "$2" ]; then
    echo 'Usage: scripts/e2e/run.sh --suite core-cli --artifacts EXISTING_DIRECTORY' >&2
    exit 2
fi
exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/product.sh" --suite core-cli --artifacts "$2"
