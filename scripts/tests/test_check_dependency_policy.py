#!/usr/bin/env python3
"""Unit tests for dependency-policy deny and exception behavior."""

from __future__ import annotations

import datetime as dt
import sys
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPOSITORY_ROOT / "scripts"))

import check_dependency_policy as policy  # noqa: E402


class DependencyPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.audit = policy.load_toml(REPOSITORY_ROOT / ".cargo" / "audit.toml")
        cls.deny = policy.load_toml(REPOSITORY_ROOT / "deny.toml")

    def test_repository_policy_is_valid(self) -> None:
        policy.validate_policy(
            REPOSITORY_ROOT,
            today=dt.date(2026, 8, 2),
        )

    def test_fake_denied_license_fails(self) -> None:
        self.assertTrue(
            policy.dependency_is_denied(
                self.audit,
                self.deny,
                {"kind": "license", "id": "AGPL-3.0-only"},
            )
        )

    def test_fake_high_severity_advisory_fails(self) -> None:
        self.assertTrue(
            policy.dependency_is_denied(
                self.audit,
                self.deny,
                {
                    "kind": "advisory",
                    "id": "RUSTSEC-2099-0001",
                    "severity": "high",
                },
            )
        )

    def test_fake_unknown_git_source_fails(self) -> None:
        self.assertTrue(
            policy.dependency_is_denied(
                self.audit,
                self.deny,
                {
                    "kind": "source",
                    "source-kind": "git",
                    "id": "https://example.invalid/unreviewed/repository",
                },
            )
        )

    def test_expired_exception_fails(self) -> None:
        expired = {
            "kind": "advisory",
            "id": "RUSTSEC-2099-0001",
            "crate": "fake-crate@1.2.3",
            "owner": "@security-owner",
            "rationale": "Temporary mitigation is deployed while upgrading.",
            "created": "2026-01-01",
            "expires": "2026-01-31",
        }
        with self.assertRaisesRegex(policy.PolicyError, "expired"):
            policy.validate_exception(
                expired,
                today=dt.date(2026, 2, 1),
                max_days=90,
            )


if __name__ == "__main__":
    unittest.main()
