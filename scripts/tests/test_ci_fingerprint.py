#!/usr/bin/env python3
"""Tests for CI pass fingerprints and their workflow wiring (mini-agent-m7wn5)."""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts import ci_fingerprint as fp

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = (REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
ACTION = (REPOSITORY_ROOT / ".github" / "actions" / "ci-fingerprint" / "action.yml").read_text(encoding="utf-8")

FILES = {
    "Cargo.toml": "a",
    "Cargo.lock": "lock",
    "rust-toolchain.toml": "tc",
    "src/main.rs": "main",
    "src/permission.rs": "perm",
    "src/tests/mod.rs": "t1",
    "src/ui/feed_tests.rs": "t2",
    "tests/cli.rs": "t3",
    "README.md": "r",
    "docs/plans/idea.md": "d",
    "docs/specs/x.md": "spec",
    ".github/workflows/ci.yml": "wf",
}


def entries(**changes: str | None) -> list[tuple[str, str]]:
    files = dict(FILES)
    for key, value in changes.items():
        real = {
            "readme": "README.md",
            "plan": "docs/plans/idea.md",
            "spec": "docs/specs/x.md",
            "main": "src/main.rs",
            "test": "src/tests/mod.rs",
            "feed": "src/ui/feed_tests.rs",
            "lock": "Cargo.lock",
            "workflow": ".github/workflows/ci.yml",
            "toolchain": "rust-toolchain.toml",
        }[key]
        if value is None:
            files.pop(real)
        else:
            files[real] = value
    return sorted(files.items())


def compute(items, input_class, **kw) -> str:
    kw.setdefault("job", "test")
    return fp.fingerprint(items, input_class=input_class, **kw)


class FingerprintInputSetTests(unittest.TestCase):
    def test_docs_only_change_changes_neither_class(self) -> None:
        for input_class in fp.CLASSES:
            with self.subTest(input_class=input_class):
                before = compute(entries(), input_class)
                self.assertEqual(before, compute(entries(readme="changed", plan="changed"), input_class))
                self.assertEqual(before, compute(entries(plan=None), input_class))

    def test_consumed_docs_are_production_inputs(self) -> None:
        for input_class in fp.CLASSES:
            self.assertNotEqual(compute(entries(), input_class), compute(entries(spec="changed"), input_class))

    def test_test_change_changes_only_production_plus_tests(self) -> None:
        for change in ({"test": "changed"}, {"feed": "changed"}):
            with self.subTest(change=change):
                self.assertEqual(compute(entries(), "production"), compute(entries(**change), "production"))
                self.assertNotEqual(
                    compute(entries(), "production+tests"), compute(entries(**change), "production+tests")
                )

    def test_production_change_changes_both_classes(self) -> None:
        for input_class in fp.CLASSES:
            self.assertNotEqual(compute(entries(), input_class), compute(entries(main="changed"), input_class))

    def test_workflow_lockfile_and_toolchain_invalidate_everything(self) -> None:
        for input_class in fp.CLASSES:
            base = compute(entries(), input_class, pinned={"Cargo.lock": b"a"})
            self.assertNotEqual(base, compute(entries(), input_class, pinned={"Cargo.lock": b"b"}))
            self.assertNotEqual(
                compute(entries(), input_class, pinned={".github/workflows/ci.yml": b"a"}),
                compute(entries(), input_class, pinned={".github/workflows/ci.yml": b"b"}),
            )
            self.assertNotEqual(
                compute(entries(), input_class, pinned={"rust-toolchain.toml": b"1.96"}),
                compute(entries(), input_class, pinned={"rust-toolchain.toml": b"1.97"}),
            )
            self.assertNotEqual(base, compute(entries(workflow="edited"), input_class, pinned={"Cargo.lock": b"a"}))
            self.assertNotEqual(base, compute(entries(lock="edited"), input_class, pinned={"Cargo.lock": b"a"}))

    def test_job_row_os_image_and_arch_are_part_of_the_key(self) -> None:
        base = compute(entries(), "production+tests", row="", os_name="Linux", image="ubuntu24", arch="X64")
        for override in (
            {"job": "clippy"},
            {"row": "--no-default-features"},
            {"os_name": "macOS"},
            {"image": "ubuntu22"},
            {"arch": "ARM64"},
        ):
            args = dict(row="", os_name="Linux", image="ubuntu24", arch="X64")
            args.update(override)
            with self.subTest(override=override):
                self.assertNotEqual(base, compute(entries(), "production+tests", **args))

    def test_field_boundaries_cannot_collide(self) -> None:
        self.assertNotEqual(
            compute(entries(), "production", row="ab", os_name="c"),
            compute(entries(), "production", row="a", os_name="bc"),
        )

    def test_unknown_class_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            compute(entries(), "everything")


class MarkerPolicyTests(unittest.TestCase):
    def test_policy(self) -> None:
        self.assertEqual((True, True), fp.marker_policy("push", "refs/heads/main"))
        self.assertEqual((True, False), fp.marker_policy("push", "refs/heads/phase6-integration"))
        self.assertEqual((True, False), fp.marker_policy("pull_request", "refs/pull/3/merge"))

    def test_tags_schedule_and_dispatch_never_use_markers(self) -> None:
        for event, ref in (
            ("push", "refs/tags/v1.0.0"),
            ("schedule", "refs/heads/main"),
            ("workflow_dispatch", "refs/heads/main"),
            ("", ""),
        ):
            self.assertEqual((False, False), fp.marker_policy(event, ref), (event, ref))


class CommandLineTests(unittest.TestCase):
    def run_cli(self, event: str, ref: str) -> dict[str, str]:
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory) / "repo"
            repo.mkdir()
            git = lambda *a: subprocess.run(["git", *a], cwd=repo, check=True, capture_output=True)
            git("init", "-q")
            (repo / "a.rs").write_text("x")
            (repo / "Cargo.lock").write_text("lock")
            git("add", ".")
            output = Path(directory) / "out"
            output.touch()
            env = dict(os.environ, GITHUB_OUTPUT=str(output))
            script = REPOSITORY_ROOT / "scripts" / "ci_fingerprint.py"
            result = subprocess.run(
                ["python3", str(script), "compute", "--job", "test", "--classes", "production+tests",
                 "--event", event, "--ref", ref],
                cwd=repo, env=env, capture_output=True, text=True,
            )
            self.assertEqual(0, result.returncode, result.stderr)
            return dict(line.split("=", 1) for line in output.read_text().splitlines())

    def test_push_to_main_may_write(self) -> None:
        values = self.run_cli("push", "refs/heads/main")
        self.assertEqual(("true", "true"), (values["read"], values["write"]))
        self.assertEqual(f"ci-pass-test-{values['fingerprint']}", values["key"])
        self.assertRegex(values["fingerprint"], r"^[0-9a-f]{64}$")

    def test_pull_request_reads_but_never_writes(self) -> None:
        values = self.run_cli("pull_request", "refs/pull/1/merge")
        self.assertEqual(("true", "false"), (values["read"], values["write"]))

    def test_tag_schedule_dispatch_emit_no_key(self) -> None:
        for event, ref in (("push", "refs/tags/v1"), ("schedule", "refs/heads/main"), ("workflow_dispatch", "refs/heads/main")):
            values = self.run_cli(event, ref)
            self.assertEqual(("false", "false", ""), (values["read"], values["write"], values["key"]))

    def test_outside_a_repository_fails_open(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run(
                ["python3", str(REPOSITORY_ROOT / "scripts" / "ci_fingerprint.py"), "compute",
                 "--job", "test", "--classes", "production", "--event", "push", "--ref", "refs/heads/main"],
                cwd=directory, capture_output=True, text=True,
            )
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertIn("read=false", result.stdout)


def job_body(name: str) -> str:
    match = re.search(rf"(?ms)^  {re.escape(name)}:\n(?P<body>.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)", WORKFLOW)
    assert match, name
    return match.group("body")


class WorkflowWiringTests(unittest.TestCase):
    FINGERPRINTED = ("clippy", "test", "all-features", "parallel-test-smoke", "mcp-stdio", "lsp-lifecycle")
    NEVER = (
        "linux-sandbox-policy",
        "macos-worker-containment-gate",
        "windows-worker-containment-gate",
        "windows-general-sandbox-policy",
        "phase6-cross-platform-gate",
        "windows-worker-launcher-unit",
        "fmt",
        "ci-success",
        "changes",
    )

    def test_containment_and_policy_jobs_never_use_markers(self) -> None:
        for job in self.NEVER:
            with self.subTest(job=job):
                self.assertNotIn("ci-fingerprint", job_body(job))

    def test_fingerprinted_jobs_check_first_and_gate_every_later_step(self) -> None:
        for job in self.FINGERPRINTED:
            body = job_body(job)
            steps = body.split("    steps:\n", 1)[1]
            blocks = re.split(r"(?m)^      - ", steps)[1:]
            with self.subTest(job=job):
                self.assertIn("actions/checkout", blocks[0])
                self.assertIn("id: fp", blocks[1])
                self.assertIn("mode: check", blocks[1])
                self.assertIn(f"job: {job}", blocks[1])
                self.assertIn("classes: production+tests", blocks[1])
                for block in blocks[2:-1]:
                    self.assertIn("steps.fp.outputs.skip != 'true'", block, block)
                last = blocks[-1]
                self.assertIn("mode: save", last)
                self.assertIn("success() && steps.fp.outputs.skip != 'true' && steps.fp.outputs.write == 'true'", last)
                self.assertIn("key: ${{ steps.fp.outputs.key }}", last)

    def test_marker_action_is_pinned_lookup_only_and_never_writes_on_check(self) -> None:
        self.assertEqual(2, len(re.findall(r"actions/cache/(?:restore|save)@[0-9a-f]{40} # v", ACTION)))
        self.assertIn("lookup-only: true", ACTION)
        self.assertRegex(ACTION, r"(?m)^      if: inputs\.mode == 'save' && inputs\.key != ''$")
        self.assertIn("if: inputs.mode == 'check' && steps.compute.outputs.read == 'true'", ACTION)

    def test_save_is_gated_by_the_main_push_policy_in_the_script(self) -> None:
        # The workflow passes only the script's `key`/`write`; tags, schedule,
        # dispatch and pull requests never get write=true (MarkerPolicyTests).
        for job in self.FINGERPRINTED:
            self.assertIn("steps.fp.outputs.write == 'true'", job_body(job))


if __name__ == "__main__":
    unittest.main()
