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
REQUIRED_DOCUMENTS=("LICENSE" "NOTICE" "SOURCE.md" "THIRD_PARTY_LICENSES")

usage() {
    local status="${1:-0}"
    cat <<EOF
Usage: install.sh [--dir <path>] [--release <version>]

Options:
  --dir <path>   Install directory (default: ~/.local/bin)
  --release <version>
                 Install an exact release (for example, 1.7.2). Defaults to latest.
  --help         Show this message

Private releases: if the anonymous download fails, the installer retries with
GITHUB_TOKEN (GitHub REST API) and then with an authenticated 'gh' CLI. Set
MINI_AGENT_INSTALL_NO_TOKEN=1 or MINI_AGENT_INSTALL_NO_GH=1 to disable either.
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

ARCHIVE_PATH="${TMPDIR}/${ARCHIVE_FILE}"
MANIFEST="${TMPDIR}/SHA256SUMS"

# Authenticated fallbacks for private or access-restricted releases. They run
# only after the anonymous download fails or returns a non-release page, and
# whatever they fetch goes through the same checksum verification below.
#
#   GITHUB_TOKEN                   sent as an Authorization header to the GitHub
#                                  REST API (from a 0600 file, never in argv)
#   gh                             `gh release download`, used only when gh is
#                                  installed and `gh auth status` succeeds
#   MINI_AGENT_INSTALL_NO_TOKEN=1  ignore GITHUB_TOKEN
#   MINI_AGENT_INSTALL_NO_GH=1     never invoke gh
API_BASE="https://api.github.com"
if [[ -n "$RELEASE_VERSION" ]]; then
    API_RELEASE_URL="${API_BASE}/repos/${REPO}/releases/tags/v${RELEASE_VERSION}"
else
    API_RELEASE_URL="${API_BASE}/repos/${REPO}/releases/latest"
fi

is_html() {
    local prefix
    prefix="$(head -c 512 "$1" | tr -d '\000' | tr '[:upper:]' '[:lower:]')"
    [[ "$prefix" == *"<!doctype html"* || "$prefix" == *"<html"* ]]
}

clear_assets() {
    rm -f "$ARCHIVE_PATH" "$MANIFEST"
}

# ---- reject non-release responses before checksum parsing ----
#
# A sign-in or error page served with HTTP 200 (for example when the release
# is private or the request needs authentication) passes curl -f. Detect it
# here so the failure names the real cause instead of a missing checksum.
# Sets ASSET_WHAT and ASSET_PROBLEM and returns non-zero when the downloaded
# files are not release assets.
check_assets() {
    ASSET_WHAT=""
    ASSET_PROBLEM=""
    if [[ ! -f "$ARCHIVE_PATH" ]]; then
        ASSET_WHAT="archive ${ARCHIVE_FILE}"
        ASSET_PROBLEM="missing"
        return 1
    fi
    local magic
    magic="$(head -c 2 "$ARCHIVE_PATH" | od -An -tx1 | tr -d ' \n')"
    if [[ "$magic" != "1f8b" ]]; then
        ASSET_WHAT="archive ${ARCHIVE_FILE}"
        if is_html "$ARCHIVE_PATH"; then
            ASSET_PROBLEM="an HTML page"
        else
            ASSET_PROBLEM="data that is not gzip"
        fi
        return 1
    fi
    ASSET_WHAT="checksum manifest SHA256SUMS"
    if [[ ! -f "$MANIFEST" ]]; then
        ASSET_PROBLEM="missing"
        return 1
    fi
    if is_html "$MANIFEST"; then
        ASSET_PROBLEM="an HTML page"
        return 1
    fi
    ASSET_WHAT=""
    return 0
}

fetch_anonymous() {
    if ! curl -fsSL --max-time 300 -o "$ARCHIVE_PATH" "${BASE_URL}/${ARCHIVE_FILE}"; then
        rm -f "$ARCHIVE_PATH"
        return 1
    fi
    if ! curl -fsSL --max-time 60 -o "$MANIFEST" "${BASE_URL}/SHA256SUMS"; then
        rm -f "$MANIFEST"
        return 1
    fi
}

# Print the REST API download URL of the named asset from a release JSON
# document. Each asset object lists its "url" before its "name"; only URLs of
# this repository's release assets are accepted, so the token is never sent to
# any other endpoint.
release_asset_url() {
    local url
    url="$(grep -oE '"(url|name)"[[:space:]]*:[[:space:]]*"[^"]*"' "$1" | awk -v target="$2" '
        /^"url"/ {
            url = $0
            sub(/^"url"[[:space:]]*:[[:space:]]*"/, "", url)
            sub(/"$/, "", url)
            if (url !~ /\/releases\/assets\/[0-9]+$/) url = ""
            next
        }
        /^"name"/ {
            name = $0
            sub(/^"name"[[:space:]]*:[[:space:]]*"/, "", name)
            sub(/"$/, "", name)
            if (name == target && url != "") { print url; exit }
        }')"
    if [[ "$url" != "${API_BASE}/repos/${REPO}/releases/assets/"* ]]; then
        return 1
    fi
    printf '%s\n' "$url"
}

fetch_with_token() {
    local header="${TMPDIR}/.auth-header" meta="${TMPDIR}/.release.json"
    local archive_url manifest_url status=1
    # Keep the token out of argv (visible in ps) by handing curl a header file.
    (umask 077 && printf 'Authorization: Bearer %s\n' "$GITHUB_TOKEN" > "$header")
    if curl -fsSL --max-time 60 -H "@${header}" \
        -H "Accept: application/vnd.github+json" \
        -o "$meta" "$API_RELEASE_URL" \
        && archive_url="$(release_asset_url "$meta" "$ARCHIVE_FILE")" \
        && manifest_url="$(release_asset_url "$meta" SHA256SUMS)" \
        && curl -fsSL --max-time 300 -H "@${header}" \
            -H "Accept: application/octet-stream" \
            -o "$ARCHIVE_PATH" "$archive_url" \
        && curl -fsSL --max-time 60 -H "@${header}" \
            -H "Accept: application/octet-stream" \
            -o "$MANIFEST" "$manifest_url"; then
        status=0
    fi
    rm -f "$header" "$meta"
    return "$status"
}

gh_usable() {
    [[ "${MINI_AGENT_INSTALL_NO_GH:-}" != "1" ]] \
        && command -v gh >/dev/null 2>&1 \
        && gh auth status >/dev/null 2>&1
}

fetch_with_gh() {
    local dir="${TMPDIR}/gh-download"
    rm -rf "$dir"
    mkdir "$dir"
    if [[ -n "$RELEASE_VERSION" ]]; then
        gh release download "v${RELEASE_VERSION}" --repo "$REPO" \
            --pattern "$ARCHIVE_FILE" --pattern SHA256SUMS --dir "$dir" || return 1
    else
        gh release download --repo "$REPO" \
            --pattern "$ARCHIVE_FILE" --pattern SHA256SUMS --dir "$dir" || return 1
    fi
    [[ -f "${dir}/${ARCHIVE_FILE}" && -f "${dir}/SHA256SUMS" ]] || return 1
    mv -f "${dir}/${ARCHIVE_FILE}" "$ARCHIVE_PATH"
    mv -f "${dir}/SHA256SUMS" "$MANIFEST"
}

download_error() {
    local what="$1" problem="$2" tried="$3"
    if [[ "$problem" == "missing" ]]; then
        echo "Error: could not download the ${what}." >&2
    else
        echo "Error: the downloaded ${what} is not a release asset (received ${problem})." >&2
    fi
    echo "  URL: ${BASE_URL}" >&2
    if [[ -n "$tried" ]]; then
        echo "  Authenticated retries also failed:${tried}." >&2
    fi
    echo "  The release may not exist for this platform, or the repository may be" >&2
    echo "  private and require authentication. For a private repository, set" >&2
    echo "  GITHUB_TOKEN or sign in with 'gh auth login' and rerun the installer," >&2
    echo "  or download the assets with an authenticated client, for example:" >&2
    echo "    gh release download --repo ${REPO} --pattern '${ARCHIVE_FILE}' --pattern SHA256SUMS" >&2
    exit 1
}

# ---- download, with authenticated fallbacks ----
DOWNLOAD_SOURCE=""
fetch_anonymous || true
if check_assets; then
    DOWNLOAD_SOURCE="anonymous"
else
    FIRST_WHAT="$ASSET_WHAT"
    FIRST_PROBLEM="$ASSET_PROBLEM"
    TRIED=""
    if [[ -n "${GITHUB_TOKEN:-}" && "${MINI_AGENT_INSTALL_NO_TOKEN:-}" != "1" ]]; then
        echo "Anonymous download failed; retrying with GITHUB_TOKEN..." >&2
        clear_assets
        if fetch_with_token && check_assets; then
            DOWNLOAD_SOURCE="GITHUB_TOKEN"
        else
            TRIED="${TRIED} GITHUB_TOKEN"
        fi
    fi
    if [[ -z "$DOWNLOAD_SOURCE" ]] && gh_usable; then
        echo "Retrying with the authenticated GitHub CLI (gh release download)..." >&2
        clear_assets
        if fetch_with_gh && check_assets; then
            DOWNLOAD_SOURCE="gh"
        else
            TRIED="${TRIED} gh"
        fi
    fi
    if [[ -z "$DOWNLOAD_SOURCE" ]]; then
        download_error "$FIRST_WHAT" "$FIRST_PROBLEM" "$TRIED"
    fi
    echo "Downloaded release assets with ${DOWNLOAD_SOURCE}."
fi

# ---- verify checksum before extraction ----
#
# Parse the single line for this exact archive from the manifest.
# Fail closed for: missing manifest, no entry, duplicate entries,
# wrong filename, or hash mismatch. This applies equally to assets fetched
# anonymously, with GITHUB_TOKEN, or with gh.

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

# ---- Linux sandbox prerequisite ----
# bubblewrap backs the default Linux subprocess sandbox and the JS runtime but
# is not bundled. Only warn: the binary itself runs without it.
if [[ "$OS" == "unknown-linux-musl" ]] && ! command -v bwrap >/dev/null 2>&1; then
    echo
    echo "Warning: bubblewrap ('bwrap') was not found on your PATH."
    echo "  ${BINARY_NAME} uses it for the default Linux sandbox and the JS runtime;"
    echo "  without it the sandbox cannot be enforced and the js tool is unavailable."
    echo "  Install it with your package manager, for example:"
    echo "    sudo apt install bubblewrap"
    echo "  On Ubuntu 23.10+/24.04, AppArmor must also allow bwrap's user namespaces; see"
    echo "  https://github.com/${REPO}#linux-prerequisites"
fi

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
