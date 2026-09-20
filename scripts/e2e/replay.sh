#!/bin/sh
# The replay suite is catalog-driven product evidence: actual production
# Rust/CLI integration, never runner-child scenarios. The targets and the
# case-to-test map live in scripts/e2e/product/replay.json.
set -eu
set +x

artifacts_dir=""
binary_path=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --artifacts)
            [ "$#" -ge 2 ] || { echo 'Usage: scripts/e2e/run.sh --suite replay [--binary PATH] --artifacts EXISTING_DIRECTORY' >&2; exit 2; }
            artifacts_dir="$2"
            shift 2
            ;;
        --binary)
            [ "$#" -ge 2 ] || { echo 'Usage: scripts/e2e/run.sh --suite replay [--binary PATH] --artifacts EXISTING_DIRECTORY' >&2; exit 2; }
            binary_path="$2"
            shift 2
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

if [ -z "$artifacts_dir" ] || [ ! -d "$artifacts_dir" ]; then
    echo 'Usage: scripts/e2e/run.sh --suite replay [--binary PATH] --artifacts EXISTING_DIRECTORY' >&2
    exit 2
fi

exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/product.sh" --suite replay --artifacts "$artifacts_dir"
