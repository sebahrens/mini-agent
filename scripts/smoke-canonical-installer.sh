#!/usr/bin/env bash
# Exercise the checked-in installer against the exact Cargo-pinned release.
# This is intentionally networked and must pass before release coordinate
# changes are closed or published.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
INSTALL_ROOT="$(mktemp -d)"
trap 'rm -rf "$INSTALL_ROOT"' EXIT
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${ROOT_DIR}/Cargo.toml" | head -1)"
if [[ -z "$VERSION" ]]; then
    echo "Error: could not read the Cargo package version" >&2
    exit 1
fi

bash "${ROOT_DIR}/install.sh" --release "$VERSION" --dir "${INSTALL_ROOT}/bin"
VERSION_OUTPUT=$("${INSTALL_ROOT}/bin/mini-agent" --version)
EXPECTED_OUTPUT="mini-agent ${VERSION}"
if [[ "$VERSION_OUTPUT" != "$EXPECTED_OUTPUT" ]]; then
    echo "Error: canonical installer produced ${VERSION_OUTPUT}; expected ${EXPECTED_OUTPUT}" >&2
    exit 1
fi

echo "canonical installer smoke: PASS (${VERSION_OUTPUT})"
