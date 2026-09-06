#!/usr/bin/env bash
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
exec python3 "$repo/scripts/gym/train.py" --repo "$repo" "$@"
