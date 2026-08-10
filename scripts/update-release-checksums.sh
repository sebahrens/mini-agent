#!/usr/bin/env bash
# Download canonical release inputs, verify HTTP success, then update package hashes.
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
RELEASE_BASE="https://github.com/sebahrens/mini-agent/releases/download/v${VERSION}"

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

if [[ "$need_linux" == true ]]; then
    download linux-x86.tar.gz "${RELEASE_BASE}/mini-agent-x86_64-unknown-linux-musl.tar.gz"
    download linux-arm.tar.gz "${RELEASE_BASE}/mini-agent-aarch64-unknown-linux-musl.tar.gz"
    SHA_LINUX_X86="$(sha256_file "${DOWNLOAD_DIR}/linux-x86.tar.gz")"
    SHA_LINUX_ARM="$(sha256_file "${DOWNLOAD_DIR}/linux-arm.tar.gz")"
fi
if [[ "$need_darwin" == true ]]; then
    download darwin-x86.tar.gz "${RELEASE_BASE}/mini-agent-x86_64-apple-darwin.tar.gz"
    download darwin-arm.tar.gz "${RELEASE_BASE}/mini-agent-aarch64-apple-darwin.tar.gz"
    SHA_DARWIN_X86="$(sha256_file "${DOWNLOAD_DIR}/darwin-x86.tar.gz")"
    SHA_DARWIN_ARM="$(sha256_file "${DOWNLOAD_DIR}/darwin-arm.tar.gz")"
fi
if [[ "$need_license" == true ]]; then
    download LICENSE "https://raw.githubusercontent.com/sebahrens/mini-agent/v${VERSION}/LICENSE"
    SHA_LICENSE="$(sha256_file "${DOWNLOAD_DIR}/LICENSE")"
fi
if [[ "$need_source" == true ]]; then
    download source.tar.gz "${RELEASE_BASE}/mini-agent-v${VERSION}-source.tar.gz"
    SHA_SOURCE="$(sha256_file "${DOWNLOAD_DIR}/source.tar.gz")"
fi

# All required downloads and hashes have succeeded before any recipe is changed.
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
