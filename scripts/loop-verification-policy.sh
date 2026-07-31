#!/bin/bash
# Verification relevance policy for scripts/loop.sh.
#
# loop.sh is intentionally long-lived across agent iterations. Keep these
# predicates in a reloadable file so fixes made by one iteration are used by
# that same loop process during post-agent verification.

path_is_generated_python_bytecode() {
    case "$1" in
        */__pycache__/*|*.pyc|*.pyo) return 0 ;;
        *) return 1 ;;
    esac
}

path_is_github_workflow() {
    case "$1" in
        .github/workflows/*.yml|.github/workflows/*.yaml) return 0 ;;
        *) return 1 ;;
    esac
}

path_is_relevant_for_profile() {
    local path="$1" profile="$2" surfaces="${3:-rust}"
    # Generated Python bytecode is never an implementation surface. It can
    # appear under scripts/ after a local test run, but has no source verifier
    # and must not make an iteration relevant (or trigger "verifier missing").
    path_is_generated_python_bytecode "$path" && return 1
    # CI workflow fixes are complete implementation changes when their YAML
    # syntax and the Cargo gates pass, including under the headless profile.
    path_is_github_workflow "$path" && return 0
    case "$path" in
        src/*|Cargo.toml|Cargo.lock|build.rs|scripts/loop.sh|scripts/loop-verification-policy.sh) return 0 ;;
    esac
    case ",$surfaces," in
        *,script,*) case "$path" in scripts/*) return 0 ;; esac ;;
    esac
    case ",$surfaces," in
        *,data,*) case "$path" in data/*) return 0 ;; esac ;;
    esac
    case ",$surfaces," in
        *,asset,*) case "$path" in assets/*) return 0 ;; esac ;;
    esac
    case ",$surfaces," in
        *,cargo-config,*) case "$path" in .cargo/*) return 0 ;; esac ;;
    esac
    if [ "$profile" = packaged-artifact ] && [[ ",$surfaces," == *,packaging,* ]]; then
        case "$path" in
            packaging/*|nix/*|tap/*|.github/*|scripts/*|install.sh|justfile|default.nix|release.nix|shell.nix) return 0 ;;
        esac
    fi
    return 1
}
