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
# The release ships the documents of its tag, which the working tree may have
# moved past since; compare with the tagged copies when the tag is available.
RELEASE_TAG="v${VERSION}"
for document in LICENSE NOTICE SOURCE.md; do
    if git -C "$ROOT_DIR" rev-parse -q --verify "refs/tags/${RELEASE_TAG}" >/dev/null; then
        git -C "$ROOT_DIR" show "${RELEASE_TAG}:${document}" |
            cmp - "${INSTALL_ROOT}/share/doc/mini-agent/${document}"
    else
        cmp "${ROOT_DIR}/${document}" "${INSTALL_ROOT}/share/doc/mini-agent/${document}"
    fi
done
# THIRD_PARTY_LICENSES is generated per build, so it is checked by format.
# Releases before the installer's FIRST_INVENTORY_RELEASE predate it; the
# installer then warns and installs without it. Drop this tolerance once the
# pinned Cargo version is at least that release.
INVENTORY="${INSTALL_ROOT}/share/doc/mini-agent/THIRD_PARTY_LICENSES"
FIRST_INVENTORY_RELEASE="$(sed -n 's/^FIRST_INVENTORY_RELEASE="\([^"]*\)"$/\1/p' "${ROOT_DIR}/install.sh")"
eval "$(sed -n '/^version_predates()/,/^}/p' "${ROOT_DIR}/install.sh")"
if [[ -f "$INVENTORY" ]]; then
    head -n 1 "$INVENTORY" | grep -Fxq "mini-agent third-party license inventory"
elif [[ -n "$FIRST_INVENTORY_RELEASE" ]] && version_predates "$VERSION" "$FIRST_INVENTORY_RELEASE"; then
    echo "note: ${VERSION} predates the bundled third-party licence inventory (${FIRST_INVENTORY_RELEASE})"
else
    echo "Error: the installer did not install ${INVENTORY}" >&2
    exit 1
fi

echo "canonical installer smoke: PASS (${VERSION_OUTPUT})"
