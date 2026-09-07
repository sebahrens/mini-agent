#!/usr/bin/env bash
set -euo pipefail

step=initialization
trap 'printf "gym setup failed during %s\n" "$step" >&2' ERR

step=workspace_location
# Resolve before deciding: `$(pwd)`, `$1` and MINI_AGENT_GYM_ROOT can all reach
# the temp root through a symlink (macOS `/tmp` -> `/private/tmp`), and the
# unresolved spelling walks straight past a literal prefix test. Resolves the
# deepest existing ancestor so a gym root that does not exist yet still gets a
# real path.
resolve_path() {
  target=$1
  if [ -d "$target" ]; then
    (cd "$target" && pwd -P)
    return
  fi
  parent=$(dirname "$target")
  if [ -d "$parent" ]; then
    parent=$(cd "$parent" && pwd -P)
    printf '%s/%s\n' "${parent%/}" "$(basename "$target")"
  else
    printf '%s\n' "$target"
  fi
}

repo=${1:-$(pwd)}
if [ ! -d "$repo" ]; then
  printf 'gym setup: %s is not a directory\n' "$repo" >&2
  exit 2
fi
repo=$(resolve_path "$repo")
gym_root=$(resolve_path "${MINI_AGENT_GYM_ROOT:-"$repo/.gym"}")
# Both trees must stay out of the system temp root. macOS Seatbelt write-allows
# /private/tmp wholesale, and Linux bwrap replaces /tmp with a fresh tmpfs, so a
# workspace or a per-arm data/state/cache tree living there is either outside
# the boundary under test or invisible to the sandboxed child.
for candidate in "$repo" "$gym_root"; do
  case "$candidate" in
    /tmp|/tmp/*|/private/tmp|/private/tmp/*)
      printf 'gym setup refuses workspaces under the system temp root (sandbox boundary): %s\n' \
        "$candidate" >&2
      exit 2
      ;;
  esac
done

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
# train.py materializes each episode's worktree under worktrees/ and its AppPaths
# tree under runs/; nothing else in the root is consumed by the runner.
mkdir -p "$gym_root" "$gym_root/worktrees" "$gym_root/runs"
cd "$repo"

step=debug_install
# The learned-skill operator commands the library arm calls are gated behind the
# non-default `skills` feature, so a default install cannot run them. --locked
# keeps this build's lockfile identical to the preflight below, and --root keeps
# the gym binary out of the operator's ~/.cargo/bin. `cargo install --root DIR`
# installs into DIR/bin, so the gym binary lands at $gym_root/bin/mini-agent.
cargo install --path . --debug --locked --features skills --root "$gym_root"
binary="$gym_root/bin/mini-agent"
test -x "$binary"
# No pipeline here: `grep -q` can SIGPIPE the writer, which `set -o pipefail`
# would report as an install failure.
help=$("$binary" --help 2>&1)
case "$help" in
  *--install-learned-skill-seeds*) ;;
  *)
    printf 'installed binary does not advertise --install-learned-skill-seeds: the gym install lost the skills feature\n' >&2
    exit 2
    ;;
esac

step=worker_containment_preflight
# Same feature set as the install above, so the preflight exercises the build
# the episodes actually run.
case "$(uname -s)" in
  Linux)
    cargo test --locked --features skills \
      linux_js_worker_containment -- --ignored --nocapture
    ;;
  Darwin)
    cargo test --locked --features skills \
      macos_js_worker_containment -- --ignored --nocapture
    ;;
  *)
    printf 'gym setup supports Linux and macOS hosts only\n' >&2
    exit 2
    ;;
esac

trap - ERR
printf 'gym host ready: %s\n' "$gym_root"
printf 'run episodes with: scripts/gym/train.sh --binary %s --tasks <tasks.json> --output <outcomes.jsonl>\n' "$binary"
