#!/usr/bin/env bash
# Sync the version from Cargo.toml to all packaging files.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

sed_in_place() {
    local expression="$1"
    local path="$2"
    sed -i.bak "$expression" "$path"
    rm -f "${path}.bak"
}

VERSION=$(grep '^version' "${ROOT_DIR}/Cargo.toml" | head -1 | cut -d'"' -f2)
PACKAGING_VERSION=$(sed -n 's/^pkgver=//p' "${ROOT_DIR}/packaging/aur/PKGBUILD" | head -1)

if [ -z "$VERSION" ]; then
    echo "Error: Could not read version from Cargo.toml" >&2
    exit 1
fi

echo "Syncing version ${VERSION} across packaging files..."

VERSION_CHANGED=false
if [[ "$PACKAGING_VERSION" != "$VERSION" ]]; then
    VERSION_CHANGED=true
fi

# PKGBUILD
sed_in_place "s/^pkgver=.*/pkgver=${VERSION}/" "${ROOT_DIR}/packaging/aur/PKGBUILD"

# Checked-in AUR metadata (regenerated with makepkg after release checksums land)
SRCINFO="${ROOT_DIR}/packaging/aur/.SRCINFO"
sed_in_place "s/^[[:space:]]*pkgver = .*/	pkgver = ${VERSION}/" "$SRCINFO"
sed_in_place "s|/download/v[^/]*/|/download/v${VERSION}/|g" "$SRCINFO"
sed_in_place "s|/mini-agent/v[^/]*/LICENSE|/mini-agent/v${VERSION}/LICENSE|g" "$SRCINFO"
sed_in_place "s/zerostack-bin-[0-9][^-]*-/zerostack-bin-${VERSION}-/g" "$SRCINFO"

# conda meta.yaml files (plain YAML format: "version: X.Y.Z")
for meta in "${ROOT_DIR}/packaging/conda/"*/meta.yaml; do
    sed_in_place "s/^  version: .*/  version: ${VERSION}/" "$meta"
done

# conda source URLs
sed_in_place "s|/download/v[^/]*/mini-agent-v[^/]*-source.tar.gz|/download/v${VERSION}/mini-agent-v${VERSION}-source.tar.gz|" \
    "${ROOT_DIR}/packaging/conda/zerostack/meta.yaml"
sed_in_place "s|/download/v[0-9][^/]*/|/download/v${VERSION}/|g" \
    "${ROOT_DIR}/packaging/conda/zerostack-bin/meta.yaml"
sed_in_place "s|/mini-agent/v[0-9][^/]*/LICENSE|/mini-agent/v${VERSION}/LICENSE|g" \
    "${ROOT_DIR}/packaging/conda/zerostack-bin/meta.yaml"

# Homebrew formula
HB_FORMULA="${ROOT_DIR}/packaging/homebrew/zerostack.rb"
if [ -f "$HB_FORMULA" ]; then
    sed_in_place "s/^  version \".*\"/  version \"${VERSION}\"/" "$HB_FORMULA"
    sed_in_place "s|/download/v[^/]*/|/download/v${VERSION}/|g" "$HB_FORMULA"
fi

if [[ "$VERSION_CHANGED" == true ]]; then
    sed_in_place "s/^pkgrel=.*/pkgrel=1/" "${ROOT_DIR}/packaging/aur/PKGBUILD"
    sed_in_place "s/^[[:space:]]*pkgrel = .*/\tpkgrel = 1/" "$SRCINFO"
    for meta in "${ROOT_DIR}/packaging/conda/"*/meta.yaml; do
        sed_in_place "s/^  number: .*/  number: 0/" "$meta"
    done
    sed_in_place "/^  revision [0-9][0-9]*$/d" "$HB_FORMULA"
fi

echo ""
echo "Next steps:"
echo "  just add-tag          # push tag, trigger GitHub release"
echo "  just post-release     # download artifacts, update all checksums, regen .SRCINFO"
