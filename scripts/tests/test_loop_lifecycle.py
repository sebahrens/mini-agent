#!/usr/bin/env python3
"""Regression tests for the Beads lifecycle managed by scripts/loop.sh."""

from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
LOOP_SCRIPT = REPOSITORY_ROOT / "scripts" / "loop.sh"
VERIFICATION_POLICY = REPOSITORY_ROOT / "scripts" / "loop-verification-policy.sh"


def extract_function(source: str, name: str, next_name: str) -> str:
    start = source.index(f"{name}() {{")
    end = source.index(f"\n{next_name}() {{", start)
    return source[start:end]


class LoopLifecycleTests(unittest.TestCase):
    def test_production_verifier_never_selects_the_research_spike(self) -> None:
        source = LOOP_SCRIPT.read_text()

        self.assertIn("standalone spike/", source)
        self.assertNotIn('existing_crates+="spike"', source)

    def test_verification_errors_are_appended_with_real_newlines(self) -> None:
        source = LOOP_SCRIPT.read_text()
        append_function = extract_function(
            source,
            "append_verification_error",
            "report_verification_failure",
        )
        harness = f"""
set -eu

errors=

{append_function}

append_verification_error "verifier missing" "No checker for generated.pyc"
append_verification_error "cargo fmt errors" $'first line\\nsecond line'
printf '%s' "$errors"
"""

        completed = subprocess.run(
            ["bash", "-c", harness],
            cwd=REPOSITORY_ROOT,
            text=True,
            capture_output=True,
            check=False,
        )

        self.assertEqual(0, completed.returncode, completed.stderr)
        self.assertEqual(
            "=== verifier missing ===\n"
            "No checker for generated.pyc\n\n"
            "=== cargo fmt errors ===\n"
            "first line\n"
            "second line\n\n",
            completed.stdout,
        )

    def test_verification_failure_description_uses_real_newlines(self) -> None:
        source = LOOP_SCRIPT.read_text()
        report_function = extract_function(
            source,
            "report_verification_failure",
            "run_verification",
        )
        harness = f"""
set -eu

CURRENT_ITERATION=77
PICKED_ID=mini-agent-test
RED=
NC=

bd() {{
    [ "$1" = create ] || return 1
    shift
    while [ "$#" -gt 0 ]; do
        if [ "$1" = --description ]; then
            shift
            printf '%s' "$1" > "$CAPTURE_FILE"
            return 0
        fi
        shift
    done
    return 1
}}

{report_function}

report_verification_failure $'=== verifier missing ===\\npath.pyc\\n'
"""

        with tempfile.TemporaryDirectory() as temp_directory:
            capture_file = Path(temp_directory) / "description.txt"
            completed = subprocess.run(
                ["bash", "-c", harness],
                cwd=REPOSITORY_ROOT,
                env={"CAPTURE_FILE": str(capture_file), "PATH": "/usr/bin:/bin"},
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertEqual(0, completed.returncode, completed.stderr)
            self.assertEqual(
                "Post-agent verification failed. Fix before any other work.\n\n"
                "=== verifier missing ===\npath.pyc\n",
                capture_file.read_text(),
            )

    def test_clean_install_uses_online_resilient_cargo_network_settings(self) -> None:
        source = LOOP_SCRIPT.read_text()
        install_function = extract_function(
            source,
            "run_in_clean_install_environment",
            "cargo_config_free_ancestor_chain",
        )
        harness = f"""
set -eu

install_root=$(mktemp -d)
trap 'rm -rf "$install_root"' EXIT
export CARGO_NET_OFFLINE=true

{install_function}

run_in_clean_install_environment "$install_root" /usr/bin/env /usr/bin/true
"""

        completed = subprocess.run(
            ["bash", "-c", harness],
            cwd=REPOSITORY_ROOT,
            text=True,
            capture_output=True,
            check=False,
        )

        self.assertEqual(0, completed.returncode, completed.stderr)
        environment = dict(
            line.split("=", 1)
            for line in completed.stdout.splitlines()
            if "=" in line
        )
        self.assertEqual("10", environment.get("CARGO_NET_RETRY"))
        self.assertEqual("0", environment.get("CARGO_INCREMENTAL"))
        self.assertEqual("256", environment.get("CARGO_PROFILE_DEV_CODEGEN_UNITS"))
        self.assertNotIn("CARGO_HTTP_MULTIPLEXING", environment)
        self.assertNotIn("CARGO_NET_OFFLINE", environment)

    def test_hard_timeout_runs_shell_functions_when_timeout_binary_exists(self) -> None:
        source = LOOP_SCRIPT.read_text()
        timeout_function = extract_function(
            source,
            "run_with_hard_timeout",
            "hash_file_sha256",
        )
        harness = f"""
set -eu

TIMEOUT_BIN=$(command -v false)

shell_task() {{
    printf 'shell-function-ran\\n'
}}

{timeout_function}

run_with_hard_timeout 5 shell_task
"""

        completed = subprocess.run(
            ["bash", "-c", harness],
            cwd=REPOSITORY_ROOT,
            text=True,
            capture_output=True,
            check=False,
        )

        self.assertEqual(0, completed.returncode, completed.stderr)
        self.assertEqual("shell-function-ran\n", completed.stdout)

    def test_clean_install_is_locked_and_not_subject_to_scenario_file_limit(self) -> None:
        source = LOOP_SCRIPT.read_text()
        replay_function = extract_function(
            source,
            "replay_real_binary_evidence",
            "decide_build_outcome",
        )
        install_start = replay_function.index('if [ "$reason" = replay-failed ] \\\n'
                                              '            && ! (cd "$install_workspace"')
        install_end = replay_function.index(
            'elif [ "$reason" = replay-failed ]; then',
            install_start,
        )
        install_section = replay_function[install_start:install_end]

        self.assertIn("install --path . --debug --locked", install_section)
        self.assertNotIn("ulimit -f", install_section)
        self.assertIn("ulimit -f 4096", replay_function[install_end:])

    def test_checked_in_src_fixtures_use_the_full_cargo_suite(self) -> None:
        source = LOOP_SCRIPT.read_text()
        fixture_function = extract_function(
            source,
            "path_is_cargo_verified_fixture",
            "load_verification_policy",
        )
        harness = f"""
set -eu

{fixture_function}

path_is_cargo_verified_fixture src/extras/skills/fixtures/evidence-skill/SKILL.md
if path_is_cargo_verified_fixture src/extras/skills/fixtures/evidence-skill/data/example.txt; then
    exit 42
fi
if path_is_cargo_verified_fixture src/extras/skills/fixtures/another-skill/SKILL.md; then
    exit 43
fi
if path_is_cargo_verified_fixture docs/fixtures/example.md; then
    exit 44
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

    def test_loop_verifier_is_relevant_to_headless_rust_iterations(self) -> None:
        relevance_function = VERIFICATION_POLICY.read_text()
        harness = f"""
set -eu

{relevance_function}

path_is_relevant_for_profile scripts/loop.sh headless rust
path_is_relevant_for_profile .github/workflows/ci.yml headless rust
path_is_relevant_for_profile .github/workflows/ci.yaml headless rust
if path_is_relevant_for_profile .github/workflows/ci.json headless rust; then
    exit 41
fi
if path_is_relevant_for_profile scripts/tests/test_loop_lifecycle.py headless rust; then
    exit 42
fi
path_is_relevant_for_profile scripts/tests/test_loop_lifecycle.py headless script
if path_is_relevant_for_profile scripts/tests/__pycache__/test_loop_lifecycle.cpython-314.pyc headless script; then
    exit 43
fi
if path_is_relevant_for_profile scripts/tests/test_loop_lifecycle.pyo packaged-artifact packaging; then
    exit 44
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

    def test_automatic_staging_excludes_tracked_python_bytecode(self) -> None:
        source = LOOP_SCRIPT.read_text()
        policy = VERIFICATION_POLICY.read_text()
        bytecode_function = extract_function(
            policy,
            "path_is_generated_python_bytecode",
            "path_is_relevant_for_profile",
        )
        staging_function = extract_function(
            source,
            "stage_non_beads_changes",
            "reopen_build_bead",
        )
        harness = f"""
set -eu

{bytecode_function}

{staging_function}

stage_non_beads_changes
git diff --cached --name-only
"""

        with tempfile.TemporaryDirectory() as temp_directory:
            repository = Path(temp_directory)
            source_file = repository / "src" / "lib.rs"
            beads_file = repository / ".beads" / "issues.jsonl"
            bytecode = (
                repository
                / "scripts"
                / "tests"
                / "__pycache__"
                / "test_loop_lifecycle.cpython-314.pyc"
            )
            source_file.parent.mkdir(parents=True)
            beads_file.parent.mkdir(parents=True)
            bytecode.parent.mkdir(parents=True)
            source_file.write_text("pub fn before() {}\n")
            beads_file.write_text('{"status":"before"}\n')
            bytecode.write_bytes(b"\x00before")
            subprocess.run(
                ["git", "init", "--quiet"],
                cwd=repository,
                check=True,
            )
            subprocess.run(
                ["git", "add", "."],
                cwd=repository,
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "-c",
                    "user.name=Loop Test",
                    "-c",
                    "user.email=loop@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "base",
                ],
                cwd=repository,
                check=True,
            )

            source_file.write_text("pub fn after() {}\n")
            beads_file.write_text('{"status":"after"}\n')
            bytecode.write_bytes(b"\x00after")
            completed = subprocess.run(
                ["bash", "-c", harness],
                cwd=repository,
                text=True,
                capture_output=True,
                check=False,
            )

        self.assertEqual(0, completed.returncode, completed.stderr)
        self.assertEqual("src/lib.rs\n", completed.stdout)

    def test_automatic_staging_keeps_tracked_python_bytecode_deletions(
        self,
    ) -> None:
        source = LOOP_SCRIPT.read_text()
        policy = VERIFICATION_POLICY.read_text()
        bytecode_function = extract_function(
            policy,
            "path_is_generated_python_bytecode",
            "path_is_relevant_for_profile",
        )
        staging_function = extract_function(
            source,
            "stage_non_beads_changes",
            "reopen_build_bead",
        )
        harness = f"""
set -eu

{bytecode_function}

{staging_function}

stage_non_beads_changes
git diff --cached --name-status
"""

        with tempfile.TemporaryDirectory() as temp_directory:
            repository = Path(temp_directory)
            bytecode = (
                repository
                / "scripts"
                / "tests"
                / "__pycache__"
                / "test_loop_lifecycle.cpython-314.pyc"
            )
            bytecode.parent.mkdir(parents=True)
            bytecode.write_bytes(b"\x00tracked")
            subprocess.run(
                ["git", "init", "--quiet"],
                cwd=repository,
                check=True,
            )
            subprocess.run(
                ["git", "add", "."],
                cwd=repository,
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "-c",
                    "user.name=Loop Test",
                    "-c",
                    "user.email=loop@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "base",
                ],
                cwd=repository,
                check=True,
            )

            bytecode.unlink()
            completed = subprocess.run(
                ["bash", "-c", harness],
                cwd=repository,
                text=True,
                capture_output=True,
                check=False,
            )

        self.assertEqual(0, completed.returncode, completed.stderr)
        self.assertEqual(
            "D\tscripts/tests/__pycache__/"
            "test_loop_lifecycle.cpython-314.pyc\n",
            completed.stdout,
        )

    def test_run_verification_accepts_headless_workflow_only_commit(self) -> None:
        source = LOOP_SCRIPT.read_text()
        policy = VERIFICATION_POLICY.read_text()
        append_function = extract_function(
            source,
            "append_verification_error",
            "report_verification_failure",
        )
        relevance_function = policy
        verification_function = extract_function(
            source,
            "run_verification",
            "show_agent_progress",
        )
        harness = f"""
set -eu

MODE=build
CURRENT_ITERATION=1
PICKED_ID=mini-agent-test
BOLD=
CYAN=
DIM=
GREEN=
NC=
RED=
YELLOW=

path_is_cargo_verified_fixture() {{
    return 1
}}

sign_test_binaries() {{
    :
}}

report_verification_failure() {{
    return 1
}}

{append_function}

{relevance_function}

{verification_function}

run_verification HEAD~1 HEAD headless rust
"""

        with tempfile.TemporaryDirectory() as temp_directory:
            repository = Path(temp_directory)
            fake_bin = repository / "fake-bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo"
            fake_cargo.write_text("#!/usr/bin/env bash\nexit 0\n")
            fake_cargo.chmod(0o755)

            subprocess.run(
                ["git", "init", "--quiet"],
                cwd=repository,
                check=True,
            )
            (repository / "Cargo.toml").write_text(
                '[package]\nname = "verification-fixture"\nversion = "0.1.0"\n'
            )
            subprocess.run(
                ["git", "add", "Cargo.toml"],
                cwd=repository,
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "-c",
                    "user.name=Loop Test",
                    "-c",
                    "user.email=loop@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "base",
                ],
                cwd=repository,
                check=True,
            )

            workflow = repository / ".github" / "workflows" / "ci.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text("name: CI\n")
            bytecode = (
                repository
                / "scripts"
                / "tests"
                / "__pycache__"
                / "test_loop_lifecycle.cpython-314.pyc"
            )
            bytecode.parent.mkdir(parents=True)
            bytecode.write_bytes(b"\x00tracked generated bytecode")
            subprocess.run(
                [
                    "git",
                    "add",
                    ".github/workflows/ci.yml",
                    "scripts/tests/__pycache__/test_loop_lifecycle.cpython-314.pyc",
                ],
                cwd=repository,
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "-c",
                    "user.name=Loop Test",
                    "-c",
                    "user.email=loop@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "workflow",
                ],
                cwd=repository,
                check=True,
            )

            completed = subprocess.run(
                ["bash", "-c", harness],
                cwd=repository,
                env={"PATH": f"{fake_bin}:/usr/bin:/bin"},
                text=True,
                capture_output=True,
                check=False,
            )

        self.assertEqual(0, completed.returncode, completed.stderr)
        self.assertIn("All checks passed", completed.stdout)
        self.assertNotIn("relevance allowlist", completed.stderr)
        self.assertNotIn("verifier missing", completed.stderr)

    def test_run_verification_does_not_report_generated_bytecode_as_failure(
        self,
    ) -> None:
        source = LOOP_SCRIPT.read_text()
        policy = VERIFICATION_POLICY.read_text()
        append_function = extract_function(
            source,
            "append_verification_error",
            "report_verification_failure",
        )
        relevance_function = policy
        verification_function = extract_function(
            source,
            "run_verification",
            "show_agent_progress",
        )
        harness = f"""
set -eu

MODE=build
CURRENT_ITERATION=1
PICKED_ID=mini-agent-test
BOLD=
CYAN=
DIM=
GREEN=
NC=
RED=
YELLOW=

path_is_cargo_verified_fixture() {{
    return 1
}}

sign_test_binaries() {{
    :
}}

report_verification_failure() {{
    printf 'P0_REPORTED\\n'
}}

{append_function}

{relevance_function}

{verification_function}

if run_verification HEAD~1 HEAD headless script; then
    exit 42
fi
"""

        with tempfile.TemporaryDirectory() as temp_directory:
            repository = Path(temp_directory)
            fake_bin = repository / "fake-bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo"
            fake_cargo.write_text("#!/usr/bin/env bash\nexit 0\n")
            fake_cargo.chmod(0o755)

            subprocess.run(
                ["git", "init", "--quiet"],
                cwd=repository,
                check=True,
            )
            (repository / "Cargo.toml").write_text(
                '[package]\nname = "verification-fixture"\nversion = "0.1.0"\n'
            )
            subprocess.run(
                ["git", "add", "Cargo.toml"],
                cwd=repository,
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "-c",
                    "user.name=Loop Test",
                    "-c",
                    "user.email=loop@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "base",
                ],
                cwd=repository,
                check=True,
            )

            bytecode = (
                repository
                / "scripts"
                / "tests"
                / "__pycache__"
                / "test_loop_lifecycle.cpython-314.pyc"
            )
            bytecode.parent.mkdir(parents=True)
            bytecode.write_bytes(b"\x00tracked generated bytecode")
            subprocess.run(
                [
                    "git",
                    "add",
                    "scripts/tests/__pycache__/"
                    "test_loop_lifecycle.cpython-314.pyc",
                ],
                cwd=repository,
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "-c",
                    "user.name=Loop Test",
                    "-c",
                    "user.email=loop@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "bytecode",
                ],
                cwd=repository,
                check=True,
            )

            completed = subprocess.run(
                ["bash", "-c", harness],
                cwd=repository,
                env={"PATH": f"{fake_bin}:/usr/bin:/bin"},
                text=True,
                capture_output=True,
                check=False,
            )

        self.assertEqual(0, completed.returncode, completed.stderr)
        self.assertIn("No relevant code changed", completed.stdout)
        self.assertNotIn("P0_REPORTED", completed.stdout)
        self.assertNotIn("verifier missing", completed.stdout)

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
