#!/usr/bin/env bash
#
# Install mini-agent from GitHub Releases.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/sebahrens/mini-agent/main/install.sh | bash
#
#   # Custom install directory:
#   curl -fsSL https://raw.githubusercontent.com/sebahrens/mini-agent/main/install.sh | bash -s -- --dir /usr/local/bin
#
set -euo pipefail

REPO="sebahrens/mini-agent"
BINARY_NAME="mini-agent"
DEFAULT_DIR="${HOME}/.local/bin"
REQUIRED_DOCUMENTS=("LICENSE" "NOTICE" "SOURCE.md")

usage() {
    local status="${1:-0}"
    cat <<EOF
Usage: install.sh [--dir <path>] [--release <version>]

Options:
  --dir <path>   Install directory (default: ~/.local/bin)
  --release <version>
                 Install an exact release (for example, 1.7.2). Defaults to latest.
  --help         Show this message
EOF
    exit "$status"
}

# ---- parse args ----
INSTALL_DIR=""
RELEASE_VERSION=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dir)
            if [[ $# -lt 2 ]]; then
                echo "Missing value for --dir" >&2
                exit 2
            fi
            INSTALL_DIR="$2"
            shift 2
            ;;
        --release)
            if [[ $# -lt 2 ]]; then
                echo "Missing value for --release" >&2
                exit 2
            fi
            RELEASE_VERSION="$2"
            shift 2
            ;;
        --help|-h)
            usage
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage 2
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

# A typed or quoted "~" is not expanded by the shell; expand the current
# user's home so "~/bin" never becomes a literal "./~/bin" directory.
case "$INSTALL_DIR" in
    \~) INSTALL_DIR="$HOME" ;;
    \~/*) INSTALL_DIR="${HOME}/${INSTALL_DIR#\~/}" ;;
esac

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
if [[ -n "$RELEASE_VERSION" ]]; then
    if [[ ! "$RELEASE_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]]; then
        echo "Invalid release version: ${RELEASE_VERSION}" >&2
        exit 1
    fi
    BASE_URL="https://github.com/${REPO}/releases/download/v${RELEASE_VERSION}"
    RELEASE_LABEL="v${RELEASE_VERSION}"
else
    BASE_URL="https://github.com/${REPO}/releases/latest/download"
    RELEASE_LABEL="latest"
fi

echo "Downloading ${BINARY_NAME} ${RELEASE_LABEL} (${ASSET_NAME})..."
echo "  -> ${BASE_URL}/${ARCHIVE_FILE}"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

curl -fsSL --max-time 300 -o "${TMPDIR}/${ARCHIVE_FILE}" "${BASE_URL}/${ARCHIVE_FILE}"
curl -fsSL --max-time 60   -o "${TMPDIR}/SHA256SUMS"     "${BASE_URL}/SHA256SUMS"

# ---- reject non-release responses before checksum parsing ----
#
# A sign-in or error page served with HTTP 200 (for example when the release
# is private or the request needs authentication) passes curl -f. Detect it
# here so the failure names the real cause instead of a missing checksum.
download_error() {
    local what="$1"
    echo "Error: the downloaded ${what} is not a release asset (received $2)." >&2
    echo "  URL: ${BASE_URL}" >&2
    echo "  The release may not exist for this platform, or the repository may be" >&2
    echo "  private and require authentication. For a private repository, download" >&2
    echo "  the assets with an authenticated client, for example:" >&2
    echo "    gh release download --repo ${REPO} --pattern '${ARCHIVE_FILE}' --pattern SHA256SUMS" >&2
    exit 1
}

is_html() {
    local prefix
    prefix="$(head -c 512 "$1" | tr -d '\000' | tr '[:upper:]' '[:lower:]')"
    [[ "$prefix" == *"<!doctype html"* || "$prefix" == *"<html"* ]]
}

ARCHIVE_MAGIC="$(head -c 2 "${TMPDIR}/${ARCHIVE_FILE}" | od -An -tx1 | tr -d ' \n')"
if [[ "$ARCHIVE_MAGIC" != "1f8b" ]]; then
    if is_html "${TMPDIR}/${ARCHIVE_FILE}"; then
        download_error "archive ${ARCHIVE_FILE}" "an HTML page"
    fi
    download_error "archive ${ARCHIVE_FILE}" "data that is not gzip"
fi
if is_html "${TMPDIR}/SHA256SUMS"; then
    download_error "checksum manifest SHA256SUMS" "an HTML page"
fi

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

# Match the filename as an exact whitespace-delimited field, not as a regular
# expression. Dots and other punctuation in the platform name must stay
# literal, and extra fields make the selected line non-canonical below.
MATCH_COUNT=$(awk -v target="$ARCHIVE_FILE" '$2 == target { count++ } END { print count + 0 }' "$MANIFEST")
if [[ "$MATCH_COUNT" -eq 0 ]]; then
    echo "Error: SHA256SUMS has no entry for ${ARCHIVE_FILE}." >&2
    exit 1
fi
if [[ "$MATCH_COUNT" -gt 1 ]]; then
    echo "Error: SHA256SUMS has duplicate entries for ${ARCHIVE_FILE}." >&2
    exit 1
fi

MATCH_LINE=$(awk -v target="$ARCHIVE_FILE" '$2 == target { print }' "$MANIFEST")
EXPECTED_HASH=$(printf '%s\n' "$MATCH_LINE" | awk '{print $1}')

# Validate hash is a 64-character hex string
if [[ ! "$EXPECTED_HASH" =~ ^[0-9a-f]{64}$ ]]; then
    echo "Error: SHA256SUMS contains a malformed hash for ${ARCHIVE_FILE}." >&2
    exit 1
fi
if [[ "$MATCH_LINE" != "${EXPECTED_HASH}  ${ARCHIVE_FILE}" ]]; then
    echo "Error: SHA256SUMS contains a malformed entry for ${ARCHIVE_FILE}." >&2
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
# Use an absolute path without a trailing slash for the PATH checks below.
INSTALL_DIR="$(cd "$INSTALL_DIR" && pwd)"

tar xzf "${TMPDIR}/${ARCHIVE_FILE}" -C "$TMPDIR"

if [[ ! -f "${TMPDIR}/${BINARY_NAME}" ]]; then
    echo "Error: archive does not contain the canonical ${BINARY_NAME} executable." >&2
    exit 1
fi
for document in "${REQUIRED_DOCUMENTS[@]}"; do
    if [[ ! -f "${TMPDIR}/${document}" ]]; then
        echo "Error: archive does not contain required GPL document ${document}." >&2
        exit 1
    fi
done

if [[ "$(basename "$INSTALL_DIR")" == "bin" ]]; then
    DOC_DIR="$(dirname "$INSTALL_DIR")/share/doc/${BINARY_NAME}"
else
    DOC_DIR="${INSTALL_DIR}/share/doc/${BINARY_NAME}"
fi
mkdir -p "$DOC_DIR"
for document in "${REQUIRED_DOCUMENTS[@]}"; do
    cp "${TMPDIR}/${document}" "${DOC_DIR}/${document}"
done

# Stage the new binary beside the target and rename it into place. Writing
# over the existing file would reuse its inode: Linux refuses that while a
# session is running (ETXTBSY), macOS kills a rewritten signed executable, and
# an interrupted copy would leave a truncated binary. A rename replaces the
# directory entry atomically and leaves running processes on the old inode.
TARGET="${INSTALL_DIR}/${BINARY_NAME}"
STAGED="${INSTALL_DIR}/.${BINARY_NAME}.tmp.$$"
trap 'rm -rf "$TMPDIR"; rm -f "$STAGED"' EXIT
cp "${TMPDIR}/${BINARY_NAME}" "$STAGED"
chmod +x "$STAGED"
mv -f "$STAGED" "$TARGET"

echo "Installed ${BINARY_NAME} to ${TARGET}"
echo "Installed license and source notices to ${DOC_DIR}"

# ---- path hint ----
INSTALL_DIR_ON_PATH=0
IFS=':' read -r -a PATH_ENTRIES <<< "$PATH"
for entry in "${PATH_ENTRIES[@]}"; do
    # Compare whole entries, ignoring a trailing slash, so /usr/local/bin2
    # does not count as /usr/local/bin.
    while [[ "$entry" == */ && "$entry" != "/" ]]; do
        entry="${entry%/}"
    done
    if [[ "$entry" == "$INSTALL_DIR" ]]; then
        INSTALL_DIR_ON_PATH=1
        break
    fi
done

if [[ "$INSTALL_DIR_ON_PATH" -eq 0 ]]; then
    echo
    echo "Note: ${INSTALL_DIR} is not in your PATH."
    echo "Add it with:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo
    echo "To make it permanent, add that line to your shell rc file (~/.bashrc, ~/.zshrc, etc.)."
else
    RESOLVED="$(command -v "$BINARY_NAME" 2>/dev/null || true)"
    if [[ -n "$RESOLVED" && "$RESOLVED" != "$TARGET" ]]; then
        echo
        echo "Warning: '${BINARY_NAME}' on your PATH resolves to a different binary."
        echo "  Resolves to: ${RESOLVED}"
        echo "  Installed:   ${TARGET}"
        echo "Move ${INSTALL_DIR} earlier in PATH (or remove the other binary), then run"
        echo "'hash -r' or open a new shell."
    fi
fi
