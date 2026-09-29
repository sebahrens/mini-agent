#!/usr/bin/env bash
# Download canonical release inputs, verify them, then update package hashes.
#
# Every release asset is checked against the release's SHA256SUMS manifest and,
# when an authenticated GitHub CLI is available, against its SLSA build
# provenance (`gh attestation verify --repo sebahrens/mini-agent`). Any
# disagreement fails before a recipe is changed. MINI_AGENT_SKIP_ATTESTATION=1
# skips only the provenance check; the manifest comparison always runs.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
TARGET="${1:-all}"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${ROOT_DIR}/Cargo.toml" | head -1)"

case "$TARGET" in
    all|aur|conda-source|conda-bin|homebrew) ;;
    *)
        echo "Usage: $0 {all|aur|conda-source|conda-bin|homebrew}" >&2
        exit 2
        ;;
esac

if [[ -z "$VERSION" ]]; then
    echo "Error: could not read the Cargo package version" >&2
    exit 1
fi

DOWNLOAD_DIR="$(mktemp -d)"
trap 'rm -rf "$DOWNLOAD_DIR"' EXIT
REPO="sebahrens/mini-agent"
RELEASE_BASE="https://github.com/sebahrens/mini-agent/releases/download/v${VERSION}"
MANIFEST="${DOWNLOAD_DIR}/SHA256SUMS"

download() {
    local name="$1" url="$2"
    curl -fsSL --max-time 300 -o "${DOWNLOAD_DIR}/${name}" "$url"
    if [[ ! -s "${DOWNLOAD_DIR}/${name}" ]]; then
        echo "Error: downloaded artifact is empty: ${url}" >&2
        exit 1
    fi
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        echo "Error: no sha256sum or shasum found" >&2
        exit 1
    fi
}

# Print the SHA256SUMS digest from exactly one well-formed "<hex>  <name>" line.
manifest_digest() {
    local name="$1" count line digest
    count="$(awk -v target="$name" '$2 == target { count++ } END { print count + 0 }' "$MANIFEST")"
    if [[ "$count" -ne 1 ]]; then
        echo "Error: SHA256SUMS for v${VERSION} has ${count} entries for ${name}; expected exactly one" >&2
        return 1
    fi
    line="$(awk -v target="$name" '$2 == target { print }' "$MANIFEST")"
    digest="${line%%  *}"
    if [[ ! "$digest" =~ ^[0-9a-f]{64}$ || "$line" != "${digest}  ${name}" ]]; then
        echo "Error: SHA256SUMS for v${VERSION} has a malformed entry for ${name}" >&2
        return 1
    fi
    printf '%s\n' "$digest"
}

# Decide once whether build provenance can be checked. A missing or signed-out
# gh is a warning (the manifest comparison still runs); a failed verification
# is always fatal.
ATTESTATION_MODE=""
select_attestation_mode() {
    local reason=""
    if [[ "${MINI_AGENT_SKIP_ATTESTATION:-}" == "1" ]]; then
        reason="MINI_AGENT_SKIP_ATTESTATION=1"
    elif ! command -v gh >/dev/null 2>&1; then
        reason="the GitHub CLI ('gh') is not installed"
    elif ! gh auth status >/dev/null 2>&1; then
        reason="the GitHub CLI is not signed in"
    elif ! gh attestation verify --help >/dev/null 2>&1; then
        reason="this GitHub CLI does not support 'gh attestation verify'"
    fi
    if [[ -n "$reason" ]]; then
        echo "Warning: build provenance is not verified (${reason}); digests are checked against SHA256SUMS only." >&2
        echo "  Verify each asset with: gh attestation verify <asset> --repo ${REPO}" >&2
        ATTESTATION_MODE="skip"
    else
        ATTESTATION_MODE="verify"
    fi
}

# Download one release asset, then require its digest to equal its SHA256SUMS
# entry and, when possible, its build provenance to verify. Sets ASSET_SHA.
ASSET_SHA=""
download_release_asset() {
    local name="$1" path actual expected
    path="${DOWNLOAD_DIR}/${name}"
    download "$name" "${RELEASE_BASE}/${name}"
    actual="$(sha256_file "$path")"
    expected="$(manifest_digest "$name")" || exit 1
    if [[ "$actual" != "$expected" ]]; then
        echo "Error: ${name} does not match SHA256SUMS for v${VERSION}; no recipe was changed." >&2
        echo "  SHA256SUMS: ${expected}" >&2
        echo "  Downloaded: ${actual}" >&2
        exit 1
    fi
    if [[ "$ATTESTATION_MODE" == verify ]] \
        && ! gh attestation verify "$path" --repo "$REPO" >&2; then
        echo "Error: gh attestation verify --repo ${REPO} rejected ${name}; no recipe was changed." >&2
        exit 1
    fi
    ASSET_SHA="$actual"
}

portable_sed() {
    local expression="$1" file="$2"
    sed -i.bak "$expression" "$file"
    rm -f "${file}.bak"
}

need_linux=false
need_license=false
need_source=false
need_darwin=false
case "$TARGET" in
    all)
        need_linux=true
        need_license=true
        need_source=true
        need_darwin=true
        ;;
    aur|conda-bin)
        need_linux=true
        need_license=true
        ;;
    conda-source) need_source=true ;;
    homebrew)
        need_linux=true
        need_darwin=true
        ;;
esac

# Every target needs at least one release asset, so the manifest is always
# downloaded before any asset.
download SHA256SUMS "${RELEASE_BASE}/SHA256SUMS"
select_attestation_mode

if [[ "$need_linux" == true ]]; then
    download_release_asset mini-agent-x86_64-unknown-linux-musl.tar.gz
    SHA_LINUX_X86="$ASSET_SHA"
    download_release_asset mini-agent-aarch64-unknown-linux-musl.tar.gz
    SHA_LINUX_ARM="$ASSET_SHA"
fi
if [[ "$need_darwin" == true ]]; then
    download_release_asset mini-agent-x86_64-apple-darwin.tar.gz
    SHA_DARWIN_X86="$ASSET_SHA"
    download_release_asset mini-agent-aarch64-apple-darwin.tar.gz
    SHA_DARWIN_ARM="$ASSET_SHA"
fi
if [[ "$need_license" == true ]]; then
    # LICENSE comes from the tagged tree, not the release assets, so it has no
    # SHA256SUMS entry or attestation of its own.
    download LICENSE "https://raw.githubusercontent.com/${REPO}/v${VERSION}/LICENSE"
    SHA_LICENSE="$(sha256_file "${DOWNLOAD_DIR}/LICENSE")"
fi
if [[ "$need_source" == true ]]; then
    download_release_asset "mini-agent-v${VERSION}-source.tar.gz"
    SHA_SOURCE="$ASSET_SHA"
fi

# All required downloads and verifications have succeeded before any recipe is changed.
if [[ "$TARGET" == all || "$TARGET" == aur ]]; then
    portable_sed "s/sha256sums_x86_64=('.*' '.*')/sha256sums_x86_64=('${SHA_LINUX_X86}' '${SHA_LICENSE}')/" "${ROOT_DIR}/packaging/aur/PKGBUILD"
    portable_sed "s/sha256sums_aarch64=('.*' '.*')/sha256sums_aarch64=('${SHA_LINUX_ARM}' '${SHA_LICENSE}')/" "${ROOT_DIR}/packaging/aur/PKGBUILD"
fi
if [[ "$TARGET" == all || "$TARGET" == conda-source ]]; then
    portable_sed "/^  url:.*mini-agent-v.*-source.tar.gz/{n;s/sha256: .*/sha256: ${SHA_SOURCE}/;}" "${ROOT_DIR}/packaging/conda/zerostack/meta.yaml"
fi
if [[ "$TARGET" == all || "$TARGET" == conda-bin ]]; then
    portable_sed "/mini-agent-x86_64-unknown-linux-musl.tar.gz/{n;s/sha256: .*/sha256: ${SHA_LINUX_X86}/;}" "${ROOT_DIR}/packaging/conda/zerostack-bin/meta.yaml"
    portable_sed "/mini-agent-aarch64-unknown-linux-musl.tar.gz/{n;s/sha256: .*/sha256: ${SHA_LINUX_ARM}/;}" "${ROOT_DIR}/packaging/conda/zerostack-bin/meta.yaml"
    portable_sed "/raw.githubusercontent.com.*LICENSE/{n;s/sha256: .*/sha256: ${SHA_LICENSE}/;}" "${ROOT_DIR}/packaging/conda/zerostack-bin/meta.yaml"
fi
if [[ "$TARGET" == all || "$TARGET" == homebrew ]]; then
    portable_sed "/mini-agent-x86_64-apple-darwin.tar.gz/{n;s/sha256 \".*\"/sha256 \"${SHA_DARWIN_X86}\"/;}" "${ROOT_DIR}/packaging/homebrew/zerostack.rb"
    portable_sed "/mini-agent-aarch64-apple-darwin.tar.gz/{n;s/sha256 \".*\"/sha256 \"${SHA_DARWIN_ARM}\"/;}" "${ROOT_DIR}/packaging/homebrew/zerostack.rb"
    portable_sed "/mini-agent-x86_64-unknown-linux-musl.tar.gz/{n;s/sha256 \".*\"/sha256 \"${SHA_LINUX_X86}\"/;}" "${ROOT_DIR}/packaging/homebrew/zerostack.rb"
    portable_sed "/mini-agent-aarch64-unknown-linux-musl.tar.gz/{n;s/sha256 \".*\"/sha256 \"${SHA_LINUX_ARM}\"/;}" "${ROOT_DIR}/packaging/homebrew/zerostack.rb"
fi

echo "Updated ${TARGET} release checksums for v${VERSION}"
