#!/usr/bin/env bash
# Sync the version from Cargo.toml to all packaging files.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

VERSION=$(grep '^version' "${ROOT_DIR}/Cargo.toml" | head -1 | cut -d'"' -f2)

if [ -z "$VERSION" ]; then
    echo "Error: Could not read version from Cargo.toml" >&2
    exit 1
fi

echo "Syncing version ${VERSION} across packaging files..."

# PKGBUILD
sed -i "s/^pkgver=.*/pkgver=${VERSION}/" "${ROOT_DIR}/packaging/aur/PKGBUILD"

# conda meta.yaml files (plain YAML format: "version: X.Y.Z")
for meta in "${ROOT_DIR}/packaging/conda/"*/meta.yaml; do
    sed -i "s/^  version: .*/  version: ${VERSION}/" "$meta"
done

# conda source URLs
sed -i "s|/tags/v[0-9][^/]*/|/tags/v${VERSION}/|" \
    "${ROOT_DIR}/packaging/conda/zerostack/meta.yaml"
sed -i "s|/download/v[0-9][^/]*/|/download/v${VERSION}/|g" \
    "${ROOT_DIR}/packaging/conda/zerostack-bin/meta.yaml"
sed -i "s|/zerostack/v[0-9][^/]*/LICENSE|/zerostack/v${VERSION}/LICENSE|g" \
    "${ROOT_DIR}/packaging/conda/zerostack-bin/meta.yaml"

# Homebrew formula
HB_FORMULA="${ROOT_DIR}/packaging/homebrew/zerostack.rb"
if [ -f "$HB_FORMULA" ]; then
    sed -i "s/^  version \".*\"/  version \"${VERSION}\"/" "$HB_FORMULA"
    sed -i "s|/download/v[^/]*/|/download/v${VERSION}/|g" "$HB_FORMULA"
fi

echo ""
echo "Next steps:"
echo "  just add-tag          # push tag, trigger GitHub release"
echo "  just post-release     # download artifacts, update all checksums, regen .SRCINFO"
