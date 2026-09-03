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

# VS Code extension manifest and lockfile (top-level package version only;
# dependency versions are deeper-indented and untouched)
VSCODE_DIR="${ROOT_DIR}/editors/vscode"
if [ -f "${VSCODE_DIR}/package.json" ]; then
    sed_in_place "s/^  \"version\": \".*\",$/  \"version\": \"${VERSION}\",/" "${VSCODE_DIR}/package.json"
fi
if [ -f "${VSCODE_DIR}/package-lock.json" ]; then
    sed_in_place "s/^  \"version\": \".*\",$/  \"version\": \"${VERSION}\",/" "${VSCODE_DIR}/package-lock.json"
    sed_in_place "/^    \"\": {$/,/^    },$/ s/^      \"version\": \".*\",$/      \"version\": \"${VERSION}\",/" \
        "${VSCODE_DIR}/package-lock.json"
fi

# VSIX Corresponding Source directions
if [ -f "${VSCODE_DIR}/SOURCE.md" ]; then
    sed_in_place "s/for version [0-9][0-9A-Za-z.+-]* is the \`v[0-9][0-9A-Za-z.+-]*\` tree/for version ${VERSION} is the \`v${VERSION}\` tree/" \
        "${VSCODE_DIR}/SOURCE.md"
    sed_in_place "s|/tree/v[0-9][0-9A-Za-z.+-]*>|/tree/v${VERSION}>|g" "${VSCODE_DIR}/SOURCE.md"
    sed_in_place "s/mini-agent-v[0-9][0-9A-Za-z.+-]*-source\.tar\.gz/mini-agent-v${VERSION}-source.tar.gz/g" \
        "${VSCODE_DIR}/SOURCE.md"
fi

# Windows MSI build example
WINDOWS_README="${ROOT_DIR}/packaging/windows/README.md"
if [ -f "$WINDOWS_README" ]; then
    sed_in_place "s/-p:ProductVersion=[0-9][0-9A-Za-z.+-]*/-p:ProductVersion=${VERSION}/" "$WINDOWS_README"
    sed_in_place "s/mini-agent-[0-9][0-9A-Za-z.+-]*-win32-x64\.vsix/mini-agent-${VERSION}-win32-x64.vsix/g" "$WINDOWS_README"
fi

# ACP registry manifest (agent version is the 4-space-indented key; the
# 6-space-indented protocol version must stay untouched)
ACP_REGISTRY="${ROOT_DIR}/docs/acp-registry.json"
if [ -f "$ACP_REGISTRY" ]; then
    sed_in_place "s/^    \"version\": \".*\",$/    \"version\": \"${VERSION}\",/" "$ACP_REGISTRY"
fi

if [[ "$VERSION_CHANGED" == true ]]; then
    sed_in_place "s/^pkgrel=.*/pkgrel=1/" "${ROOT_DIR}/packaging/aur/PKGBUILD"
    sed_in_place "s/^[[:space:]]*pkgrel = .*/\tpkgrel = 1/" "$SRCINFO"
    for meta in "${ROOT_DIR}/packaging/conda/"*/meta.yaml; do
        sed_in_place "s/^  number: .*/  number: 0/" "$meta"
    done
    sed_in_place "/^  revision [0-9][0-9]*$/d" "$HB_FORMULA"

    # The previous release's artifact digests are meaningless under the new
    # version's URLs. Replace them with an obvious placeholder that
    # `just post-release` overwrites with the published checksums. The GPL
    # LICENSE digest is version-independent and is preserved.
    LICENSE_SHA256="3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986"
    PLACEHOLDER_SHA256="0000000000000000000000000000000000000000000000000000000000000000"
    for recipe in \
        "${ROOT_DIR}/packaging/aur/PKGBUILD" \
        "$SRCINFO" \
        "${ROOT_DIR}/packaging/conda/"*/meta.yaml \
        "$HB_FORMULA"; do
        [ -f "$recipe" ] || continue
        sed_in_place "s/${LICENSE_SHA256}/__LICENSE_SHA256__/g" "$recipe"
        sed_in_place "s/[0-9a-f]\{64\}/${PLACEHOLDER_SHA256}/g" "$recipe"
        sed_in_place "s/__LICENSE_SHA256__/${LICENSE_SHA256}/g" "$recipe"
    done
    echo "Release digests reset to the pending placeholder; run 'just post-release' after publishing v${VERSION}."
fi

echo ""
echo "Next steps:"
echo "  just add-tag          # push tag, trigger GitHub release"
echo "  just post-release     # download artifacts, update all checksums, regen .SRCINFO"
