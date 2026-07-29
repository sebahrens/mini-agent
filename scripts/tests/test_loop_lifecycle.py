#!/usr/bin/env python3
"""Regression tests for the Beads lifecycle managed by scripts/loop.sh."""

from __future__ import annotations

import subprocess
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
LOOP_SCRIPT = REPOSITORY_ROOT / "scripts" / "loop.sh"


def extract_function(source: str, name: str, next_name: str) -> str:
    start = source.index(f"{name}() {{")
    end = source.index(f"\n{next_name}() {{", start)
    return source[start:end]


class LoopLifecycleTests(unittest.TestCase):
    def test_reopened_bead_can_be_claimed_again(self) -> None:
        source = LOOP_SCRIPT.read_text()
        reopen_function = extract_function(
            source,
            "reopen_build_bead",
            "auto_close_build_bead",
        )
        harness = f"""
set -eu

state=open
assignee=platon2001
actor=platon2001

bd() {{
    local command="$1"
    shift
    case "$command" in
        update)
            shift
            while [ "$#" -gt 0 ]; do
                case "$1" in
                    --status=open) state=open ;;
                    --assignee)
                        shift
                        assignee="${{1:-}}"
                        ;;
                    --claim)
                        # Beads 1.0.2 considers a claim by the existing assignee
                        # idempotent, even when the issue itself is still open.
                        if [ "$assignee" != "$actor" ]; then
                            assignee="$actor"
                            state=in_progress
                        fi
                        ;;
                esac
                shift
            done
            ;;
        comments) ;;
        *) return 1 ;;
    esac
}}

bead_enforcement_status() {{
    printf '%s\n' "$state"
}}

CURRENT_ITERATION=1
RED=
NC=

{reopen_function}

reopen_build_bead mini-agent-test 1 unavailable
bd update mini-agent-test --claim

if [ "$state" != in_progress ]; then
    echo "expected a reopened bead to be claimable, got state=$state assignee=$assignee" >&2
    exit 42
fi
"""

        completed = subprocess.run(
            ["bash", "-c", harness],
            cwd=REPOSITORY_ROOT,
            text=True,
            capture_output=True,
            check=False,
        )

        self.assertEqual(0, completed.returncode, completed.stderr)


if __name__ == "__main__":
    unittest.main()
