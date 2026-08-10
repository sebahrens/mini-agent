#!/usr/bin/env bash
# Build a tag-named GPL Corresponding Source archive with locked Cargo sources.
set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "Usage: package-corresponding-source.sh <vX.Y.Z> <output-dir> [git-ref] [--allow-untagged-label] [--compliance-docs <dir>]" >&2
    exit 2
fi

RELEASE_TAG="$1"
OUTPUT_DIR="$2"
shift 2
SOURCE_REF=""
ALLOW_UNTAGGED_LABEL=false
COMPLIANCE_DOCS=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --allow-untagged-label)
            ALLOW_UNTAGGED_LABEL=true
            shift
            ;;
        --compliance-docs)
            if [[ $# -lt 2 ]]; then
                echo "Error: --compliance-docs requires a directory" >&2
                exit 2
            fi
            COMPLIANCE_DOCS="$2"
            shift 2
            ;;
        --*)
            echo "Error: unknown option: $1" >&2
            exit 2
            ;;
        *)
            if [[ -n "$SOURCE_REF" ]]; then
                echo "Error: multiple git refs supplied" >&2
                exit 2
            fi
            SOURCE_REF="$1"
            shift
            ;;
    esac
done
SOURCE_REF="${SOURCE_REF:-$RELEASE_TAG}"
BINARY_NAME="mini-agent"
CANONICAL_GPL3_LICENSE_SHA256="3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986"

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        echo "Error: no SHA-256 utility is available" >&2
        return 1
    fi
}

if [[ ! "$RELEASE_TAG" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$ ]]; then
    echo "Error: invalid release tag: ${RELEASE_TAG}" >&2
    exit 2
fi
if [[ "$ALLOW_UNTAGGED_LABEL" == true && ! "$RELEASE_TAG" =~ -ci$ ]]; then
    echo "Error: --allow-untagged-label is restricted to labels ending in -ci" >&2
    exit 2
fi

SOURCE_COMMIT=$(git rev-parse --verify "${SOURCE_REF}^{commit}")
if TAG_COMMIT=$(git rev-parse --verify "refs/tags/${RELEASE_TAG}^{commit}" 2>/dev/null); then
    if [[ "$SOURCE_COMMIT" != "$TAG_COMMIT" ]]; then
        echo "Error: ${SOURCE_REF} does not resolve to release tag ${RELEASE_TAG}" >&2
        exit 2
    fi
elif [[ "$ALLOW_UNTAGGED_LABEL" != true ]]; then
    echo "Error: release tag does not exist: ${RELEASE_TAG}" >&2
    exit 2
fi
SOURCE_ROOT="${BINARY_NAME}-${RELEASE_TAG}-source"
ARCHIVE_NAME="${SOURCE_ROOT}.tar.gz"
STAGING_DIR=$(mktemp -d)
trap 'rm -rf "$STAGING_DIR"' EXIT

mkdir -p "$OUTPUT_DIR" "$STAGING_DIR/$SOURCE_ROOT"
git archive --format=tar "$SOURCE_COMMIT" | tar xf - -C "$STAGING_DIR/$SOURCE_ROOT"
if [[ -n "$COMPLIANCE_DOCS" ]]; then
    for document in NOTICE SOURCE.md; do
        if [[ ! -f "$STAGING_DIR/$SOURCE_ROOT/$document" ]]; then
            if [[ ! -f "$COMPLIANCE_DOCS/$document" ]]; then
                echo "Error: legacy compliance document is missing: $COMPLIANCE_DOCS/$document" >&2
                exit 2
            fi
            cp "$COMPLIANCE_DOCS/$document" "$STAGING_DIR/$SOURCE_ROOT/$document"
        fi
    done
fi
if [[ ! -f "$STAGING_DIR/$SOURCE_ROOT/LICENSE" ]] \
    || [[ "$(sha256_file "$STAGING_DIR/$SOURCE_ROOT/LICENSE")" != "$CANONICAL_GPL3_LICENSE_SHA256" ]]; then
    echo "Error: LICENSE is not the canonical GPL-3.0-only text" >&2
    exit 2
fi
mkdir -p "$STAGING_DIR/$SOURCE_ROOT/.cargo"
(
    cd "$STAGING_DIR/$SOURCE_ROOT"
    cargo vendor --locked --versioned-dirs vendor > .cargo/config.toml
    cargo metadata --locked --offline --format-version 1 > /dev/null
)

tar czf "$STAGING_DIR/$ARCHIVE_NAME" -C "$STAGING_DIR" "$SOURCE_ROOT"
ARCHIVE_LISTING="$STAGING_DIR/archive-contents.txt"
tar tzf "$STAGING_DIR/$ARCHIVE_NAME" > "$ARCHIVE_LISTING"
for required in LICENSE NOTICE SOURCE.md Cargo.toml Cargo.lock rust-toolchain.toml Cross.toml .cargo/config.toml; do
    grep -Fxq -- "$SOURCE_ROOT/$required" "$ARCHIVE_LISTING"
done
ESCAPED_SOURCE_ROOT=${SOURCE_ROOT//./\\.}
grep -Eq -- "^$ESCAPED_SOURCE_ROOT/vendor/[^/]+/Cargo\\.toml$" "$ARCHIVE_LISTING"
mv "$STAGING_DIR/$ARCHIVE_NAME" "$OUTPUT_DIR/$ARCHIVE_NAME"

echo "$OUTPUT_DIR/$ARCHIVE_NAME"
