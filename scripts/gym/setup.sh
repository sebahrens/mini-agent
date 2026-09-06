#!/usr/bin/env bash
set -euo pipefail

step=initialization
trap 'printf "gym setup failed during %s\n" "$step" >&2' ERR

repo=${1:-$(pwd)}
gym_root=${MINI_AGENT_GYM_ROOT:-"$repo/.gym"}
case "$repo" in
  /private/tmp|/private/tmp/*)
    printf 'gym setup refuses workspaces under /private/tmp (Seatbelt test boundary)\n' >&2
    exit 2
    ;;
esac

step=prerequisites
command -v cargo >/dev/null
command -v rustc >/dev/null
command -v python3 >/dev/null
command -v git >/dev/null
command -v jq >/dev/null
python3 - "$repo" <<'PY'
import re
import subprocess
import sys
from pathlib import Path

repo = Path(sys.argv[1])
required = re.search(r'channel\s*=\s*"([^"]+)"', (repo / "rust-toolchain.toml").read_text()).group(1)
actual = subprocess.check_output(["rustc", "--version"], text=True).split()[1]
if actual != required:
    raise SystemExit(f"rustc {actual} does not match rust-toolchain.toml {required}")
git_version = subprocess.check_output(["git", "version"], text=True).strip().split()[-1]
parts = tuple(int(part) for part in re.findall(r"\d+", git_version)[:2])
if parts < (2, 40):
    raise SystemExit(f"git {git_version} is older than required 2.40")
PY

step=isolated_directories
mkdir -p "$gym_root" "$gym_root/tasks" "$gym_root/worktrees" "$gym_root/data" "$gym_root/local" "$gym_root/state" "$gym_root/cache"
cd "$repo"

step=debug_install
cargo install --path . --debug

step=worker_containment_preflight
case "$(uname -s)" in
  Linux)
    cargo test --locked --no-default-features --features js \
      linux_js_worker_containment -- --ignored --nocapture
    ;;
  Darwin)
    cargo test --locked --no-default-features --features js \
      macos_js_worker_containment -- --ignored --nocapture
    ;;
  *)
    printf 'gym setup supports Linux and macOS hosts only\n' >&2
    exit 2
    ;;
esac

step=seed_library
export ZS_DATA_DIR="$gym_root/data"
export ZS_LOCAL_DATA_DIR="$gym_root/local"
export ZS_STATE_DIR="$gym_root/state"
export ZS_CACHE_DIR="$gym_root/cache"
python3 scripts/gym/train.py \
  --repo "$repo" \
  --binary mini-agent \
  --prepare-library seeds \
  --state-root "$gym_root"
trap - ERR
printf 'gym host ready: %s\n' "$gym_root"
