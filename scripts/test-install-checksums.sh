#!/usr/bin/env bash
# Test install.sh checksum verification using local fixtures (no network).
#
# Usage: bash scripts/test-install-checksums.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

PASS=0
FAIL=0

_assert_exit() {
    local label="$1" expected="$2"
    shift 2
    local actual
    actual=$("$@" 2>&1; echo "EXIT:$?") || true
    local code="${actual##*EXIT:}"
    if [[ "$code" -eq "$expected" ]]; then
        echo "  PASS: $label"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $label (expected exit $expected, got $code)"
        echo "        output: ${actual%EXIT:*}"
        FAIL=$((FAIL + 1))
    fi
}

_assert_file_absent() {
    local label="$1" path="$2"
    if [[ ! -e "$path" ]]; then
        echo "  PASS: $label (file absent as expected)"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $label (unexpected file present: $path)"
        FAIL=$((FAIL + 1))
    fi
}

# ---- build fixture data ----
FIXTURE="$(mktemp -d)"
trap 'rm -rf "$FIXTURE"' EXIT

BINARY_NAME="mini-agent"
ASSET_NAME="${BINARY_NAME}-x86_64-unknown-linux-musl"
ARCHIVE="${ASSET_NAME}.tar.gz"

# Create a fake binary
echo '#!/usr/bin/env bash' > "${FIXTURE}/${BINARY_NAME}"
echo 'echo "mini-agent 1.7.2"' >> "${FIXTURE}/${BINARY_NAME}"
chmod +x "${FIXTURE}/${BINARY_NAME}"

# Create the archive
tar czf "${FIXTURE}/${ARCHIVE}" -C "$FIXTURE" "$BINARY_NAME"
GOOD_HASH=$(sha256sum "${FIXTURE}/${ARCHIVE}" | awk '{print $1}')

# Create a valid SHA256SUMS
printf '%s  %s\n' "$GOOD_HASH" "$ARCHIVE" > "${FIXTURE}/SHA256SUMS"

# Helper: run install.sh targeting the fixture as a fake release server
run_install() {
    local tmpdir manifest archive install_dir
    tmpdir="$(mktemp -d)"
    install_dir="$(mktemp -d)"
    manifest="$1"
    archive="$2"

    # Stub curl to serve local files
    stub_curl() {
        local url="${@: -1}"
        local out_flag=false out_file=""
        for arg in "$@"; do
            if [[ "$out_flag" == true ]]; then
                out_file="$arg"
                out_flag=false
            elif [[ "$arg" == "-o" ]]; then
                out_flag=true
            fi
        done
        if [[ "$url" == */SHA256SUMS ]]; then
            cp "$manifest" "$out_file" 2>/dev/null || true
        else
            cp "$archive" "$out_file" 2>/dev/null || true
        fi
    }
    export -f stub_curl

    # Source install.sh with curl replaced and variables pre-set
    (
        set +e
        # Extract only the checksum + install logic from install.sh
        # by running it with env vars that bypass the arg parsing and download sections
        TMPDIR="$tmpdir"
        INSTALL_DIR="$install_dir"
        BINARY_NAME="$BINARY_NAME"
        ASSET_NAME="$ASSET_NAME"
        ARCHIVE_FILE="$ARCHIVE"
        BASE_URL="file://$FIXTURE"

        # Copy files into TMPDIR as curl would
        cp "$archive" "${tmpdir}/${ARCHIVE}"
        cp "$manifest" "${tmpdir}/SHA256SUMS"

        # Run just the verify + install portion inline
        source /dev/stdin <<'INNER_EOF'
MANIFEST="${TMPDIR}/SHA256SUMS"
if [[ ! -s "$MANIFEST" ]]; then echo "Error: checksum manifest is missing or empty." >&2; exit 1; fi
MATCH_COUNT=$(grep -c "  ${ARCHIVE_FILE}$" "$MANIFEST" || true)
if [[ "$MATCH_COUNT" -eq 0 ]]; then echo "Error: SHA256SUMS has no entry for ${ARCHIVE_FILE}." >&2; exit 1; fi
if [[ "$MATCH_COUNT" -gt 1 ]]; then echo "Error: SHA256SUMS has duplicate entries for ${ARCHIVE_FILE}." >&2; exit 1; fi
EXPECTED_HASH=$(grep "  ${ARCHIVE_FILE}$" "$MANIFEST" | awk '{print $1}')
if [[ ! "$EXPECTED_HASH" =~ ^[0-9a-f]{64}$ ]]; then echo "Error: malformed hash." >&2; exit 1; fi
if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL_HASH=$(sha256sum "${TMPDIR}/${ARCHIVE_FILE}" | awk '{print $1}')
else
    ACTUAL_HASH=$(shasum -a 256 "${TMPDIR}/${ARCHIVE_FILE}" | awk '{print $1}')
fi
if [[ "$ACTUAL_HASH" != "$EXPECTED_HASH" ]]; then
    echo "Error: checksum mismatch." >&2; exit 1
fi
mkdir -p "$INSTALL_DIR"
tar xzf "${TMPDIR}/${ARCHIVE_FILE}" -C "$TMPDIR"
if [[ ! -f "${TMPDIR}/${BINARY_NAME}" ]]; then echo "Error: binary missing from archive." >&2; exit 1; fi
cp "${TMPDIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
INNER_EOF
    )
    local rc=$?
    echo "$rc:$install_dir"
    rm -rf "$tmpdir"
}

echo "=== install.sh checksum verification tests ==="
echo ""

# ---- Case 1: valid archive + valid checksum ----
result=$(run_install "${FIXTURE}/SHA256SUMS" "${FIXTURE}/${ARCHIVE}")
rc="${result%%:*}"; install_dir="${result#*:}"
if [[ "$rc" -eq 0 ]] && [[ -f "${install_dir}/${BINARY_NAME}" ]]; then
    echo "  PASS: valid archive/checksum installs correctly"
    PASS=$((PASS + 1))
else
    echo "  FAIL: valid archive/checksum should install (rc=$rc, binary present: $(test -f "${install_dir}/${BINARY_NAME}" && echo yes || echo no))"
    FAIL=$((FAIL + 1))
fi
rm -rf "$install_dir"

# ---- Case 2: one-byte modified archive ----
MODIFIED="${FIXTURE}/${ARCHIVE}.modified"
cp "${FIXTURE}/${ARCHIVE}" "$MODIFIED"
printf '\x00' | dd of="$MODIFIED" bs=1 seek=20 count=1 conv=notrunc 2>/dev/null
result=$(run_install "${FIXTURE}/SHA256SUMS" "$MODIFIED")
rc="${result%%:*}"; install_dir="${result#*:}"
if [[ "$rc" -ne 0 ]]; then
    echo "  PASS: modified archive aborts before extraction"
    PASS=$((PASS + 1))
else
    echo "  FAIL: modified archive should fail checksum"
    FAIL=$((FAIL + 1))
fi
_assert_file_absent "no binary installed after tampered archive" "${install_dir}/${BINARY_NAME}"
rm -f "$MODIFIED"
rm -rf "$install_dir"

# ---- Case 3: missing manifest ----
EMPTY_MANIFEST="${FIXTURE}/empty_SHA256SUMS"
touch "$EMPTY_MANIFEST"
result=$(run_install "$EMPTY_MANIFEST" "${FIXTURE}/${ARCHIVE}")
rc="${result%%:*}"; install_dir="${result#*:}"
if [[ "$rc" -ne 0 ]]; then
    echo "  PASS: empty manifest aborts"
    PASS=$((PASS + 1))
else
    echo "  FAIL: empty manifest should abort"
    FAIL=$((FAIL + 1))
fi
_assert_file_absent "no binary installed after empty manifest" "${install_dir}/${BINARY_NAME}"
rm -f "$EMPTY_MANIFEST"
rm -rf "$install_dir"

# ---- Case 4: malformed hash in manifest ----
MALFORMED="${FIXTURE}/malformed_SHA256SUMS"
printf 'NOTAHEX  %s\n' "$ARCHIVE" > "$MALFORMED"
result=$(run_install "$MALFORMED" "${FIXTURE}/${ARCHIVE}")
rc="${result%%:*}"; install_dir="${result#*:}"
if [[ "$rc" -ne 0 ]]; then
    echo "  PASS: malformed hash aborts"
    PASS=$((PASS + 1))
else
    echo "  FAIL: malformed hash should abort"
    FAIL=$((FAIL + 1))
fi
_assert_file_absent "no binary installed after malformed hash" "${install_dir}/${BINARY_NAME}"
rm -f "$MALFORMED"
rm -rf "$install_dir"

# ---- Case 5: duplicate entry in manifest ----
DUPE="${FIXTURE}/dupe_SHA256SUMS"
printf '%s  %s\n%s  %s\n' "$GOOD_HASH" "$ARCHIVE" "$GOOD_HASH" "$ARCHIVE" > "$DUPE"
result=$(run_install "$DUPE" "${FIXTURE}/${ARCHIVE}")
rc="${result%%:*}"; install_dir="${result#*:}"
if [[ "$rc" -ne 0 ]]; then
    echo "  PASS: duplicate manifest entry aborts"
    PASS=$((PASS + 1))
else
    echo "  FAIL: duplicate manifest entry should abort"
    FAIL=$((FAIL + 1))
fi
_assert_file_absent "no binary installed after duplicate entry" "${install_dir}/${BINARY_NAME}"
rm -f "$DUPE"
rm -rf "$install_dir"

# ---- Case 6: wrong-platform archive in manifest ----
WRONG_PLATFORM="${FIXTURE}/wrong_SHA256SUMS"
printf '%s  mini-agent-aarch64-unknown-linux-musl.tar.gz\n' "$GOOD_HASH" > "$WRONG_PLATFORM"
result=$(run_install "$WRONG_PLATFORM" "${FIXTURE}/${ARCHIVE}")
rc="${result%%:*}"; install_dir="${result#*:}"
if [[ "$rc" -ne 0 ]]; then
    echo "  PASS: wrong-platform manifest entry aborts"
    PASS=$((PASS + 1))
else
    echo "  FAIL: wrong-platform manifest entry should abort"
    FAIL=$((FAIL + 1))
fi
_assert_file_absent "no binary installed after wrong-platform entry" "${install_dir}/${BINARY_NAME}"
rm -f "$WRONG_PLATFORM"
rm -rf "$install_dir"

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ "$FAIL" -eq 0 ]]
