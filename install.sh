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

# ---- download ----
URL="https://github.com/${REPO}/releases/latest/download/${ASSET_NAME}.tar.gz"

echo "Downloading ${BINARY_NAME} latest (${ASSET_NAME})..."
echo "  -> ${URL}"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

curl -fsSL --max-time 300 -o "${TMPDIR}/${ASSET_NAME}.tar.gz" "$URL"

# ---- install ----
mkdir -p "$INSTALL_DIR"

tar xzf "${TMPDIR}/${ASSET_NAME}.tar.gz" -C "$TMPDIR"

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
