# Justfile
# https://github.com/casey/just

[private]
default:
    @just --list

# ---- Build ----

build:
    cargo build --release

build-all:
    cargo build --release --all-features

run *args:
    cargo run -- {{ args }}

# ---- Quality ----

fmt:
    cargo fmt
    cargo clippy --all-targets --all-features -- -D warnings

check:
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings

test: fmt
    cargo test

# ---- Git hooks ----

install-hook:
    #!/usr/bin/env bash
    cat > .git/hooks/pre-commit << 'EOF'
    #!/bin/sh
    set -e
    echo "Running pre-commit quality checks..."
    just check
    EOF
    chmod +x .git/hooks/pre-commit
    echo "Pre-commit hook installation confirmed."

remove-hook:
    rm .git/hooks/pre-commit
    echo "Pre-commit hook uninstallation confirmed."

# ---- Tags ----

add-tag:
    #!/usr/bin/env bash
    set -euo pipefail
    VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
    python3 scripts/check-package-metadata.py \
        --require-clean \
        --ref-type tag \
        --release-tag "v${VERSION}"
    git push origin HEAD
    git tag -a "v${VERSION}" -m "Release v${VERSION}"
    git push origin "v${VERSION}"
    echo "Created and pushed tag v${VERSION}"

remove-tag VERSION="":
    #!/usr/bin/env bash
    set -e
    tag="{{ VERSION }}"
    if [ -z "$tag" ]; then
        tag=$(git tag | sort -V | fzf --prompt="Select tag to remove: ")
    fi
    if [ -z "$tag" ]; then
        echo "No tag selected"
        exit 1
    fi
    git tag -d "$tag" || {
        echo "Local tag not found"
        exit 1
    }
    git push --delete origin "$tag"
    echo "Removed tag $tag"

# ---- Packaging: version sync ----

# Sync version from Cargo.toml to all packaging files
sync-version:
    bash scripts/sync-version.sh

# ---- Packaging: checksums ----

# Download release artifacts and update AUR PKGBUILD checksums
aur-checksums:
    bash scripts/update-release-checksums.sh aur

# Update the source tarball SHA256 in conda/zerostack/meta.yaml
conda-source-sha256:
    bash scripts/update-release-checksums.sh conda-source

# Download release artifacts and update conda/zerostack-bin/meta.yaml checksums
conda-bin-checksums:
    bash scripts/update-release-checksums.sh conda-bin

# Download release artifacts and update packaging/homebrew/zerostack.rb checksums
homebrew-checksums:
    bash scripts/update-release-checksums.sh homebrew

# ---- Packaging: AUR metadata ----

# Regenerate .SRCINFO from PKGBUILD (requires makepkg)
aur-regen-srcinfo:
    #!/usr/bin/env bash
    set -euo pipefail
    cd packaging/aur
    SRCINFO_TMP=$(mktemp)
    trap 'rm -f "$SRCINFO_TMP"' EXIT
    makepkg --printsrcinfo > "$SRCINFO_TMP"
    mv "$SRCINFO_TMP" .SRCINFO
    echo "Regenerated packaging/aur/.SRCINFO"

# ---- Packaging: release workflow ----

# Full release: bump version, sync, commit, push, and tag for GitHub release CI
release BUMP:
    #!/usr/bin/env bash
    set -euo pipefail

    if ! git diff --quiet || ! git diff --cached --quiet; then
        echo "Error: working tree is dirty. Commit or stash changes first." >&2
        exit 1
    fi

    VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
    IFS='.' read -r MAJOR MINOR PATCH <<< "$VERSION"

    case "{{ BUMP }}" in
        major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
        minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
        patch) PATCH=$((PATCH + 1)) ;;
        *) echo "Error: BUMP must be one of: major, minor, patch" >&2; exit 1 ;;
    esac

    NEW_VERSION="${MAJOR}.${MINOR}.${PATCH}"
    echo "Bumping version: ${VERSION} -> ${NEW_VERSION}"
    sed -i.bak "s/^version = \"${VERSION}\"/version = \"${NEW_VERSION}\"/" Cargo.toml
    rm -f Cargo.toml.bak

    just pre-release

    # Refresh the root package entry before the locked release validation.
    cargo metadata --format-version 1 --no-deps >/dev/null
    python3 scripts/check-package-metadata.py \
        --ref-type tag \
        --release-tag "v${NEW_VERSION}"

    git commit -am "bump to v${NEW_VERSION}"
    python3 scripts/check-package-metadata.py \
        --require-clean \
        --ref-type tag \
        --release-tag "v${NEW_VERSION}"
    git push origin HEAD

    git tag -a "v${NEW_VERSION}" -m "Release v${NEW_VERSION}"
    git push origin "v${NEW_VERSION}"
    echo "Tag v${NEW_VERSION} pushed — CI release triggered."

    echo ""
    echo "=== release v${NEW_VERSION} done ==="
    echo "Next: wait for CI, then run: just post-release"

# Run after bumping Cargo.toml version (syncs version strings, no network needed)
pre-release: sync-version
    @echo "=== pre-release done: version synced across all packaging files ==="
    @echo "Next: just add-tag, wait for GitHub release, then: just post-release"

# Run after the GitHub release has been published (needs tag archive + binaries to be available)
release-checksums:
    bash scripts/update-release-checksums.sh all

canonical-installer-smoke:
    bash scripts/smoke-canonical-installer.sh

post-release: canonical-installer-smoke release-checksums aur-regen-srcinfo
    @echo "=== post-release done: all checksums updated + .SRCINFO regenerated ==="
    @echo "Ready for:"
    @echo "  AUR: cd packaging/aur && pkgctl aur publish zerostack-bin"
    @echo "  conda: submit PR to conda-forge/staged-recipes"
    @echo "  homebrew: push packaging/homebrew/zerostack.rb to homebrew-tap repo"
