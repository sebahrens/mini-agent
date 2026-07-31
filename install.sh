#!/usr/bin/env bash
#
# Install mini-agent from GitHub Releases.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/gi-dellav/zerostack/main/install.sh | bash
#
#   # Custom install directory:
#   curl -fsSL https://raw.githubusercontent.com/gi-dellav/zerostack/main/install.sh | bash -s -- --dir /usr/local/bin
#
set -euo pipefail

REPO="gi-dellav/zerostack"
BINARY_NAME="mini-agent"
DEFAULT_DIR="${HOME}/.local/bin"

usage() {
    cat <<EOF
Usage: install.sh [--dir <path>]

Options:
  --dir <path>   Install directory (default: ~/.local/bin)
  --help         Show this message
EOF
    exit 0
}

# ---- parse args ----
INSTALL_DIR=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dir)
            INSTALL_DIR="$2"
            shift 2
            ;;
        --help|-h)
            usage
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage
            ;;
    esac
done

# ---- prompt for install path ----
if [[ -z "$INSTALL_DIR" ]] && [[ -t 0 ]]; then
    read -r -p "Install directory [${DEFAULT_DIR}]: " INPUT
    INSTALL_DIR="${INPUT:-${DEFAULT_DIR}}"
else
    INSTALL_DIR="${INSTALL_DIR:-${DEFAULT_DIR}}"
fi

# ---- detect platform ----
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Darwin) OS="apple-darwin" ;;
    Linux)  OS="unknown-linux-musl" ;;
    *)
        echo "Unsupported OS: $OS" >&2
        exit 1
        ;;
esac

case "$ARCH" in
    x86_64|amd64) ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *)
        echo "Unsupported architecture: $ARCH" >&2
        exit 1
        ;;
esac

ASSET_NAME="${BINARY_NAME}-${ARCH}-${OS}"
ARCHIVE_FILE="${ASSET_NAME}.tar.gz"

# ---- download ----
BASE_URL="https://github.com/${REPO}/releases/latest/download"

echo "Downloading ${BINARY_NAME} latest (${ASSET_NAME})..."
echo "  -> ${BASE_URL}/${ARCHIVE_FILE}"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

curl -fsSL --max-time 300 -o "${TMPDIR}/${ARCHIVE_FILE}" "${BASE_URL}/${ARCHIVE_FILE}"
curl -fsSL --max-time 60   -o "${TMPDIR}/SHA256SUMS"     "${BASE_URL}/SHA256SUMS"

# ---- verify checksum before extraction ----
#
# Parse the single line for this exact archive from the manifest.
# Fail closed for: missing manifest, no entry, duplicate entries,
# wrong filename, or hash mismatch.
MANIFEST="${TMPDIR}/SHA256SUMS"

if [[ ! -s "$MANIFEST" ]]; then
    echo "Error: checksum manifest is missing or empty." >&2
    exit 1
fi

# Count entries for this archive (must be exactly 1)
MATCH_COUNT=$(grep -c "  ${ARCHIVE_FILE}$" "$MANIFEST" || true)
if [[ "$MATCH_COUNT" -eq 0 ]]; then
    echo "Error: SHA256SUMS has no entry for ${ARCHIVE_FILE}." >&2
    exit 1
fi
if [[ "$MATCH_COUNT" -gt 1 ]]; then
    echo "Error: SHA256SUMS has duplicate entries for ${ARCHIVE_FILE}." >&2
    exit 1
fi

EXPECTED_HASH=$(grep "  ${ARCHIVE_FILE}$" "$MANIFEST" | awk '{print $1}')

# Validate hash is a 64-character hex string
if [[ ! "$EXPECTED_HASH" =~ ^[0-9a-f]{64}$ ]]; then
    echo "Error: SHA256SUMS contains a malformed hash for ${ARCHIVE_FILE}." >&2
    exit 1
fi

# Compute actual hash using an available SHA-256 implementation
if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL_HASH=$(sha256sum "${TMPDIR}/${ARCHIVE_FILE}" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    ACTUAL_HASH=$(shasum -a 256 "${TMPDIR}/${ARCHIVE_FILE}" | awk '{print $1}')
else
    echo "Error: no sha256sum or shasum found; cannot verify archive." >&2
    exit 1
fi

if [[ "$ACTUAL_HASH" != "$EXPECTED_HASH" ]]; then
    echo "Error: checksum mismatch for ${ARCHIVE_FILE}." >&2
    echo "  Expected: ${EXPECTED_HASH}" >&2
    echo "  Actual:   ${ACTUAL_HASH}" >&2
    exit 1
fi

# ---- install ----
mkdir -p "$INSTALL_DIR"

tar xzf "${TMPDIR}/${ARCHIVE_FILE}" -C "$TMPDIR"

if [[ ! -f "${TMPDIR}/${BINARY_NAME}" ]]; then
    echo "Error: archive does not contain the canonical ${BINARY_NAME} executable." >&2
    exit 1
fi
cp "${TMPDIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"

chmod +x "${INSTALL_DIR}/${BINARY_NAME}"

echo "Installed ${BINARY_NAME} to ${INSTALL_DIR}/${BINARY_NAME}"

# ---- path hint ----
if ! echo "$PATH" | grep -qF "$INSTALL_DIR"; then
    echo
    echo "Note: ${INSTALL_DIR} is not in your PATH."
    echo "Add it with:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo
    echo "To make it permanent, add that line to your shell rc file (~/.bashrc, ~/.zshrc, etc.)."
fi
