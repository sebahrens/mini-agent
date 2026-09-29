#!/usr/bin/env python3
"""Tests for the release workflow's CI verification gate."""

from __future__ import annotations

import unittest
from typing import Any

from scripts import verify_release_ci

REPO = "sebahrens/mini-agent"
SHA = "a" * 40
TAG = "v9.9.9"
TAG_SUITE = 111
MAIN_SUITE = 222


def run(*, suite: int, branch: str, status: str = "completed", sha: str = SHA) -> dict[str, Any]:
    return {
        "check_suite_id": suite,
        "head_branch": branch,
        "head_sha": sha,
        "event": "push",
        "status": status,
    }


def check(*, suite: int, conclusion: str | None, status: str = "completed") -> dict[str, Any]:
    return {
        "name": "ci-success",
        "status": status,
        "conclusion": conclusion,
        "check_suite": {"id": suite},
        "html_url": f"https://example.invalid/{suite}/{conclusion}",
    }


class FakeApi:
    """Serve a scripted sequence of (workflow runs, check runs) snapshots."""

    def __init__(self, snapshots: list[tuple[list[dict[str, Any]], list[dict[str, Any]]]]):
        self.snapshots = snapshots
        self.index = 0
        self.paths: list[str] = []

    def __call__(self, path: str) -> Any:
        self.paths.append(path)
        if "/actions/workflows/ci.yml/runs?" in path:
            # Each poll starts with the workflow-run lookup; it selects the
            # snapshot that the same poll's check-run lookup then reads.
            self.current = self.snapshots[min(self.index, len(self.snapshots) - 1)]
            self.index += 1
            return {"workflow_runs": self.current[0]}
        if "/check-runs?" in path:
            return {"check_runs": self.current[1]}
        raise AssertionError(f"unexpected API path {path}")


class FakeClock:
    def __init__(self) -> None:
        self.now = 0.0
        self.sleeps: list[float] = []

    def __call__(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.sleeps.append(seconds)
        self.now += seconds


def wait(api: FakeApi, *, timeout: float = 600, interval: float = 30) -> tuple[bool, str, FakeClock]:
    clock = FakeClock()
    ok, reason = verify_release_ci.wait_for_ci(
        api,
        REPO,
        SHA,
        TAG,
        timeout=timeout,
        interval=interval,
        clock=clock,
        sleep=clock.sleep,
        log=lambda _message: None,
    )
    return ok, reason, clock


class VerifyReleaseCiTests(unittest.TestCase):
    def test_successful_ci_success_on_the_tag_run_passes(self) -> None:
        api = FakeApi([([run(suite=TAG_SUITE, branch=TAG)], [check(suite=TAG_SUITE, conclusion="success")])])

        ok, reason, clock = wait(api)

        self.assertTrue(ok, reason)
        self.assertEqual([], clock.sleeps)
        self.assertTrue(any(f"commits/{SHA}/check-runs?check_name=ci-success" in p for p in api.paths))
        self.assertTrue(any(f"head_sha={SHA}&event=push" in p for p in api.paths))

    def test_waits_for_a_running_tag_ci_and_then_passes(self) -> None:
        api = FakeApi(
            [
                ([], []),
                ([run(suite=TAG_SUITE, branch=TAG, status="in_progress")], []),
                (
                    [run(suite=TAG_SUITE, branch=TAG, status="in_progress")],
                    [check(suite=TAG_SUITE, conclusion=None, status="in_progress")],
                ),
                ([run(suite=TAG_SUITE, branch=TAG)], [check(suite=TAG_SUITE, conclusion="success")]),
            ]
        )

        ok, reason, clock = wait(api)

        self.assertTrue(ok, reason)
        self.assertEqual([30, 30, 30], clock.sleeps)

    def test_failed_ci_success_fails_without_waiting_for_the_deadline(self) -> None:
        api = FakeApi([([run(suite=TAG_SUITE, branch=TAG)], [check(suite=TAG_SUITE, conclusion="failure")])])

        ok, reason, clock = wait(api)

        self.assertFalse(ok)
        self.assertIn("without a successful ci-success", reason)
        self.assertEqual([], clock.sleeps)

    def test_completed_run_without_a_ci_success_check_fails(self) -> None:
        api = FakeApi([([run(suite=TAG_SUITE, branch=TAG)], [])])

        ok, reason, _clock = wait(api)

        self.assertFalse(ok)
        self.assertIn("no ci-success check run", reason)

    def test_a_main_push_success_does_not_stand_in_for_the_tag_run(self) -> None:
        # A main push can pass ci-success after skipping the matrix for a
        # documentation-only diff; only the tag's own full run counts.
        api = FakeApi(
            [
                (
                    [run(suite=MAIN_SUITE, branch="main"), run(suite=TAG_SUITE, branch=TAG)],
                    [check(suite=MAIN_SUITE, conclusion="success"), check(suite=TAG_SUITE, conclusion="failure")],
                )
            ]
        )

        ok, reason, _clock = wait(api)

        self.assertFalse(ok)
        self.assertIn("failure", reason)

    def test_a_same_named_check_from_another_suite_is_ignored(self) -> None:
        api = FakeApi(
            [([run(suite=TAG_SUITE, branch=TAG, status="in_progress")], [check(suite=999, conclusion="success")])]
        )

        ok, reason, clock = wait(api, timeout=90)

        self.assertFalse(ok)
        self.assertIn("timed out", reason)
        self.assertEqual([30, 30, 30], clock.sleeps)

    def test_a_run_for_another_commit_is_ignored(self) -> None:
        api = FakeApi(
            [([run(suite=TAG_SUITE, branch=TAG, sha="b" * 40)], [check(suite=TAG_SUITE, conclusion="success")])]
        )

        ok, reason, _clock = wait(api, timeout=0)

        self.assertFalse(ok)
        self.assertIn("no CI run for tag", reason)

    def test_a_rerun_success_in_the_tag_suite_passes(self) -> None:
        api = FakeApi(
            [
                (
                    [run(suite=TAG_SUITE, branch=TAG)],
                    [check(suite=TAG_SUITE, conclusion="failure"), check(suite=TAG_SUITE, conclusion="success")],
                )
            ]
        )

        ok, reason, _clock = wait(api)

        self.assertTrue(ok, reason)

    def test_api_errors_are_retried_and_never_approve(self) -> None:
        def failing_api(_path: str) -> Any:
            raise RuntimeError("HTTP 502")

        clock = FakeClock()
        ok, reason = verify_release_ci.wait_for_ci(
            failing_api,
            REPO,
            SHA,
            TAG,
            timeout=60,
            interval=30,
            clock=clock,
            sleep=clock.sleep,
            log=lambda _message: None,
        )

        self.assertFalse(ok)
        self.assertIn("timed out", reason)
        self.assertIn("HTTP 502", reason)

    def test_main_requires_repository_sha_and_tag(self) -> None:
        self.assertEqual(
            2,
            verify_release_ci.main(["--repository", "", "--sha", SHA, "--tag", TAG]),
        )


if __name__ == "__main__":
    unittest.main()
