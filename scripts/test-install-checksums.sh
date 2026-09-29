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

_assert_file_contains() {
    local label="$1" path="$2" expected="$3"
    if grep -Fqx -- "$expected" "$path"; then
        echo "  PASS: $label"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $label (missing exact line: $expected)"
        FAIL=$((FAIL + 1))
    fi
}

# ---- build fixture data ----
FIXTURE="$(mktemp -d)"
trap 'rm -rf "$FIXTURE"' EXIT

BINARY_NAME="mini-agent"
CARGO_VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${ROOT_DIR}/Cargo.toml" | head -1)"
ASSET_NAME="${BINARY_NAME}-x86_64-unknown-linux-musl"
ARCHIVE="${ASSET_NAME}.tar.gz"

# Create a fake binary
echo '#!/usr/bin/env bash' > "${FIXTURE}/${BINARY_NAME}"
echo "echo \"mini-agent ${CARGO_VERSION}\"" >> "${FIXTURE}/${BINARY_NAME}"
chmod +x "${FIXTURE}/${BINARY_NAME}"

# Create the same exact four-file payload as the release workflow.
python3 "${ROOT_DIR}/scripts/package-release-binary.py" \
    --root "$ROOT_DIR" \
    --binary "${FIXTURE}/${BINARY_NAME}" \
    --archive "${FIXTURE}/${ARCHIVE}" \
    --executable-name "$BINARY_NAME"
if command -v sha256sum >/dev/null 2>&1; then
    GOOD_HASH=$(sha256sum "${FIXTURE}/${ARCHIVE}" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    GOOD_HASH=$(shasum -a 256 "${FIXTURE}/${ARCHIVE}" | awk '{print $1}')
else
    echo "Error: no sha256sum or shasum found; cannot build fixture." >&2
    exit 1
fi

# Create a valid SHA256SUMS
printf '%s  %s\n' "$GOOD_HASH" "$ARCHIVE" > "${FIXTURE}/SHA256SUMS"

# Helper: run install.sh targeting the fixture as a fake release server
run_install() {
    local tmpdir manifest archive install_dir
    tmpdir="$(mktemp -d)"
    install_dir="$(mktemp -d)"
    manifest="$1"
    archive="$2"

    # Exercise the installer's checksum and extraction sequence with local files.
    (
        set +e
        TMPDIR="$tmpdir"
        # shellcheck disable=SC2034 # Consumed by the sourced test body below.
        INSTALL_DIR="$install_dir"
        # shellcheck disable=SC2034 # Consumed by the sourced test body below.
        ARCHIVE_FILE="$ARCHIVE"

        # Copy files into TMPDIR as curl would
        cp "$archive" "${tmpdir}/${ARCHIVE}"
        cp "$manifest" "${tmpdir}/SHA256SUMS"

        # Run just the verify + install portion inline
        # shellcheck disable=SC1091 # The source is the literal heredoc below.
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

# ---- Case 7: execute the checked-in installer and assert canonical URLs ----
STUB_BIN="${FIXTURE}/stub-bin"
REAL_INSTALL_DIR="${FIXTURE}/real-install"
REQUEST_LOG="${FIXTURE}/requested-urls"
mkdir -p "$STUB_BIN" "$REAL_INSTALL_DIR"

cat > "${STUB_BIN}/uname" <<'STUB_UNAME'
#!/usr/bin/env bash
case "$1" in
  -s) echo Linux ;;
  -m) echo x86_64 ;;
  *) exit 2 ;;
esac
STUB_UNAME

cat > "${STUB_BIN}/curl" <<'STUB_CURL'
#!/usr/bin/env bash
set -euo pipefail
out=""
url="${!#}"
for ((i = 1; i <= $#; i++)); do
    if [[ "${!i}" == "-o" ]]; then
        next=$((i + 1))
        out="${!next}"
        break
    fi
done
printf '%s\n' "$url" >> "$INSTALLER_REQUEST_LOG"
case "$url" in
  */SHA256SUMS) cp "$INSTALLER_MANIFEST" "$out" ;;
  */mini-agent-x86_64-unknown-linux-musl.tar.gz) cp "$INSTALLER_ARCHIVE" "$out" ;;
  *) echo "unexpected installer URL: $url" >&2; exit 22 ;;
esac
STUB_CURL
chmod +x "${STUB_BIN}/uname" "${STUB_BIN}/curl"

# Keep a developer's real GITHUB_TOKEN and gh login away from the installer.
if env -u GITHUB_TOKEN -u GH_TOKEN \
    MINI_AGENT_INSTALL_NO_GH=1 \
    PATH="${STUB_BIN}:$PATH" \
    INSTALLER_REQUEST_LOG="$REQUEST_LOG" \
    INSTALLER_MANIFEST="${FIXTURE}/SHA256SUMS" \
    INSTALLER_ARCHIVE="${FIXTURE}/${ARCHIVE}" \
    bash "${ROOT_DIR}/install.sh" --release "$CARGO_VERSION" --dir "$REAL_INSTALL_DIR" >/dev/null; then
    if [[ -x "${REAL_INSTALL_DIR}/${BINARY_NAME}" ]] \
        && [[ "$("${REAL_INSTALL_DIR}/${BINARY_NAME}")" == "mini-agent ${CARGO_VERSION}" ]]; then
        echo "  PASS: checked-in installer executes canonical archive end to end"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: checked-in installer did not install a working canonical binary"
        FAIL=$((FAIL + 1))
    fi
else
    echo "  FAIL: checked-in installer failed against canonical release fixture"
    FAIL=$((FAIL + 1))
fi

CANONICAL_BASE="https://github.com/sebahrens/mini-agent/releases/download/v${CARGO_VERSION}"
_assert_file_contains \
    "installer requests canonical archive origin" \
    "$REQUEST_LOG" \
    "${CANONICAL_BASE}/${ARCHIVE}"
_assert_file_contains \
    "installer requests canonical checksum origin" \
    "$REQUEST_LOG" \
    "${CANONICAL_BASE}/SHA256SUMS"

# ---- Cases 8-10: build provenance (gh attestation verify) ----
# A signed-in gh whose attestation check is controlled by GH_TEST_ATTESTATION.
GH_STUB_BIN="${FIXTURE}/gh-stub-bin"
GH_LOG="${FIXTURE}/gh.log"
mkdir -p "$GH_STUB_BIN"
cat > "${GH_STUB_BIN}/gh" <<'STUB_GH'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$GH_TEST_LOG"
case "$1 $2" in
  "auth status") exit 0 ;;
  "attestation verify")
    [[ "${3:-}" == "--help" ]] && exit 0
    [[ "$GH_TEST_ATTESTATION" == pass ]] && exit 0
    echo "Error: no attestations matched the artifact digest" >&2
    exit 1
    ;;
esac
exit 2
STUB_GH
chmod +x "${GH_STUB_BIN}/gh"

# A PATH with only what the installer and stubs need, so a host gh is absent.
TOOLS_BIN="${FIXTURE}/tools-bin"
mkdir -p "$TOOLS_BIN"
for tool in awk basename bash cat chmod cp dirname grep gzip head mkdir mktemp \
    mv od rm sed sh sha256sum shasum tar tr; do
    if resolved="$(command -v "$tool")"; then
        ln -s "$resolved" "${TOOLS_BIN}/${tool}"
    fi
done

run_real_installer() {
    local install_dir="$1" path="$2" attestation="$3"
    env -u GITHUB_TOKEN -u GH_TOKEN -u MINI_AGENT_INSTALL_NO_GH -u MINI_AGENT_SKIP_ATTESTATION \
        PATH="$path" \
        GH_TEST_LOG="$GH_LOG" \
        GH_TEST_ATTESTATION="$attestation" \
        INSTALLER_REQUEST_LOG="$REQUEST_LOG" \
        INSTALLER_MANIFEST="${FIXTURE}/SHA256SUMS" \
        INSTALLER_ARCHIVE="${FIXTURE}/${ARCHIVE}" \
        bash "${ROOT_DIR}/install.sh" --release "$CARGO_VERSION" --dir "$install_dir"
}

# Case 8: an attestation that does not verify aborts before installing.
ATTEST_FAIL_DIR="${FIXTURE}/attest-fail-install"
if output=$(run_real_installer "$ATTEST_FAIL_DIR" "${GH_STUB_BIN}:${STUB_BIN}:$PATH" fail 2>&1); then
    echo "  FAIL: failed attestation should abort the install"
    FAIL=$((FAIL + 1))
elif [[ "$output" == *"build provenance verification failed"* ]]; then
    echo "  PASS: failed attestation aborts the install"
    PASS=$((PASS + 1))
else
    echo "  FAIL: failed attestation aborted without naming the cause: $output"
    FAIL=$((FAIL + 1))
fi
_assert_file_absent "no binary installed after failed attestation" "${ATTEST_FAIL_DIR}/${BINARY_NAME}"
if grep -Eq "^attestation verify /.*/${ARCHIVE} --repo sebahrens/mini-agent\$" "$GH_LOG"; then
    echo "  PASS: installer verifies the downloaded archive against sebahrens/mini-agent"
    PASS=$((PASS + 1))
else
    echo "  FAIL: installer did not run gh attestation verify on the archive"
    FAIL=$((FAIL + 1))
fi

# Case 9: a verified attestation installs.
ATTEST_PASS_DIR="${FIXTURE}/attest-pass-install"
if output=$(run_real_installer "$ATTEST_PASS_DIR" "${GH_STUB_BIN}:${STUB_BIN}:$PATH" pass 2>&1) \
    && [[ -x "${ATTEST_PASS_DIR}/${BINARY_NAME}" ]] \
    && [[ "$output" == *"Verified build provenance"* ]]; then
    echo "  PASS: verified attestation installs"
    PASS=$((PASS + 1))
else
    echo "  FAIL: verified attestation should install: $output"
    FAIL=$((FAIL + 1))
fi

# Case 10: without gh the install proceeds and prints the manual command.
NO_GH_DIR="${FIXTURE}/no-gh-install"
if [[ -e "${STUB_BIN}/gh" || -e "${TOOLS_BIN}/gh" ]]; then
    echo "  FAIL: test PATH unexpectedly contains gh"
    FAIL=$((FAIL + 1))
elif output=$(run_real_installer "$NO_GH_DIR" "${STUB_BIN}:${TOOLS_BIN}" fail 2>&1) \
    && [[ -x "${NO_GH_DIR}/${BINARY_NAME}" ]] \
    && [[ "$output" == *"Warning: build provenance was not verified"* ]] \
    && [[ "$output" == *"gh attestation verify ${ARCHIVE} --repo sebahrens/mini-agent"* ]]; then
    echo "  PASS: missing gh installs with a manual verification warning"
    PASS=$((PASS + 1))
else
    echo "  FAIL: missing gh should install with a warning: $output"
    FAIL=$((FAIL + 1))
fi

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ "$FAIL" -eq 0 ]]
