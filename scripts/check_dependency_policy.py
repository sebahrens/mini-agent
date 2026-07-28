#!/usr/bin/env python3
"""Validate dependency policy invariants and time-bounded exceptions."""

from __future__ import annotations

import argparse
import datetime as dt
import re
import sys
import tomllib
from pathlib import Path
from typing import Any


CRATES_IO_INDEX = "https://github.com/rust-lang/crates.io-index"
EXCEPTION_KINDS = {"advisory", "license", "source"}
SEVERITY_ORDER = {"low": 0, "medium": 1, "high": 2, "critical": 3}
RUSTSEC_ID = re.compile(r"^RUSTSEC-\d{4}-\d{4}$")


class PolicyError(ValueError):
    """Raised when dependency policy can be bypassed or is malformed."""


def load_toml(path: Path) -> dict[str, Any]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise PolicyError(message)


def parse_date(value: Any, field: str) -> dt.date:
    require(isinstance(value, str), f"{field} must be an ISO date string")
    try:
        return dt.date.fromisoformat(value)
    except ValueError as error:
        raise PolicyError(f"{field} must use YYYY-MM-DD") from error


def validate_exception(
    entry: dict[str, Any], *, today: dt.date, max_days: int
) -> None:
    required = {"kind", "id", "owner", "rationale", "created", "expires"}
    missing = sorted(required - entry.keys())
    require(not missing, f"exception is missing fields: {', '.join(missing)}")

    kind = entry["kind"]
    require(kind in EXCEPTION_KINDS, f"unsupported exception kind: {kind!r}")
    allowed = required | ({"source-kind"} if kind == "source" else {"crate"})
    unexpected = sorted(entry.keys() - allowed)
    require(
        not unexpected,
        f"exception has unsupported fields: {', '.join(unexpected)}",
    )
    require(
        isinstance(entry["owner"], str) and entry["owner"].startswith("@"),
        "exception owner must be an accountable @github-handle",
    )
    require(
        isinstance(entry["rationale"], str) and len(entry["rationale"].strip()) >= 20,
        "exception rationale must describe impact and removal plan",
    )

    created = parse_date(entry["created"], "created")
    expires = parse_date(entry["expires"], "expires")
    require(created <= today, "exception creation date cannot be in the future")
    require(expires > today, f"exception {entry['id']} expired on {expires}")
    require(expires > created, "exception expiry must be after creation")
    require(
        (expires - created).days <= max_days,
        f"exception duration exceeds {max_days} days",
    )

    if kind in {"advisory", "license"}:
        crate = entry.get("crate")
        require(
            isinstance(crate, str) and re.fullmatch(r"[^@\s]+@\d+\.\d+\.\d+[^@\s]*", crate),
            f"{kind} exception must scope crate to an exact name@version",
        )
    if kind == "advisory":
        require(
            isinstance(entry["id"], str) and RUSTSEC_ID.fullmatch(entry["id"]),
            "advisory exception id must be RUSTSEC-YYYY-NNNN",
        )
    elif kind == "license":
        require(
            isinstance(entry["id"], str) and bool(entry["id"].strip()),
            "license exception id must be an SPDX identifier",
        )
    else:
        require(
            entry.get("source-kind") in {"git", "registry"},
            "source exception requires source-kind = git or registry",
        )
        require(
            isinstance(entry["id"], str) and entry["id"].startswith("https://"),
            "source exception id must be an exact HTTPS URL",
        )


def cargo_deny_advisory_ids(config: dict[str, Any]) -> set[str]:
    ignored = config.get("advisories", {}).get("ignore", [])
    return {
        item if isinstance(item, str) else item.get("id")
        for item in ignored
        if isinstance(item, str) or isinstance(item, dict)
    }


def exception_projections(
    exceptions: list[dict[str, Any]],
) -> tuple[set[str], set[tuple[str, str]], set[tuple[str, str]]]:
    advisories: set[str] = set()
    licenses: set[tuple[str, str]] = set()
    sources: set[tuple[str, str]] = set()
    for entry in exceptions:
        if entry["kind"] == "advisory":
            advisories.add(entry["id"])
        elif entry["kind"] == "license":
            licenses.add((entry["crate"], entry["id"]))
        else:
            sources.add((entry["source-kind"], entry["id"]))
    return advisories, licenses, sources


def configured_projections(
    audit: dict[str, Any], deny: dict[str, Any]
) -> tuple[set[str], set[tuple[str, str]], set[tuple[str, str]]]:
    audit_ids = set(audit.get("advisories", {}).get("ignore", []))
    deny_ids = cargo_deny_advisory_ids(deny)
    require(
        audit_ids == deny_ids,
        "advisory ignore lists in audit.toml and deny.toml must match",
    )

    licenses = {
        (entry["crate"], license_id)
        for entry in deny.get("licenses", {}).get("exceptions", [])
        for license_id in entry.get("allow", [])
    }
    source_config = deny.get("sources", {})
    sources = {
        ("registry", url)
        for url in source_config.get("allow-registry", [])
        if url != CRATES_IO_INDEX
    }
    sources.update(("git", url) for url in source_config.get("allow-git", []))
    return audit_ids, licenses, sources


def validate_policy(root: Path, *, today: dt.date | None = None) -> None:
    today = today or dt.date.today()
    cargo = load_toml(root / "Cargo.toml")
    audit = load_toml(root / ".cargo" / "audit.toml")
    deny = load_toml(root / "deny.toml")
    ledger = load_toml(root / "dependency-exceptions.toml")

    metadata = cargo.get("workspace", {}).get("metadata", {}).get(
        "dependency-policy", {}
    )
    max_days = metadata.get("max-exception-days")
    require(
        isinstance(max_days, int) and 0 < max_days <= 90,
        "max-exception-days must be between 1 and 90",
    )

    audit_advisories = audit.get("advisories", {})
    require(
        audit_advisories.get("severity-threshold") == "medium",
        "cargo-audit severity threshold must be medium",
    )
    require(
        audit.get("database", {}).get("fetch") is True
        and audit.get("database", {}).get("stale") is False,
        "cargo-audit must fetch a non-stale advisory database",
    )
    denied_output = set(audit.get("output", {}).get("deny", []))
    require(
        {"unsound", "yanked"} <= denied_output,
        "cargo-audit must deny unsound and yanked dependencies",
    )
    require(
        audit.get("yanked", {}).get("enabled") is True,
        "cargo-audit yank checking must be enabled",
    )

    deny_advisories = deny.get("advisories", {})
    require(deny_advisories.get("yanked") == "deny", "yanked crates must be denied")
    require(
        deny_advisories.get("unmaintained") == "workspace",
        "unmaintained direct dependencies must be denied",
    )
    require(
        deny_advisories.get("unsound") == "all",
        "all unsound dependencies must be denied",
    )
    require(
        deny.get("bans", {}).get("wildcards") == "deny",
        "wildcard dependency requirements must be denied",
    )
    require(
        deny.get("graph", {}).get("all-features") is True,
        "cargo-deny must evaluate all features",
    )

    sources = deny.get("sources", {})
    require(
        sources.get("unknown-registry") == "deny"
        and sources.get("unknown-git") == "deny",
        "unknown registries and git sources must be denied",
    )
    require(
        CRATES_IO_INDEX in sources.get("allow-registry", []),
        "the crates.io index must be the explicit base registry",
    )
    require(
        sources.get("required-git-spec") == "rev",
        "allowed git dependencies must pin a full revision",
    )
    require(
        deny.get("licenses", {}).get("include-dev") is True,
        "dev dependency licenses must be checked",
    )

    require(ledger.get("version") == 1, "unsupported exception ledger version")
    exceptions = ledger.get("exceptions")
    require(isinstance(exceptions, list), "exceptions must be a list")
    identities: set[tuple[Any, ...]] = set()
    for entry in exceptions:
        require(isinstance(entry, dict), "each exception must be a TOML table")
        validate_exception(entry, today=today, max_days=max_days)
        identity = (
            entry["kind"],
            entry["id"],
            entry.get("crate"),
            entry.get("source-kind"),
        )
        require(identity not in identities, f"duplicate exception: {entry['id']}")
        identities.add(identity)

    expected = exception_projections(exceptions)
    configured = configured_projections(audit, deny)
    require(
        expected == configured,
        "exception ledger and enforcement configuration are out of sync",
    )

    workflow = (root / ".github" / "workflows" / "ci.yml").read_text()
    for tool in ("cargo-audit", "cargo-deny"):
        version = metadata.get(f"{tool}-version")
        require(
            isinstance(version, str) and bool(version),
            f"{tool}-version must be pinned in Cargo.toml",
        )
        install = f"cargo install --locked {tool} --version {version}"
        require(install in workflow, f"CI tool pin is missing or stale: {install}")

    required_commands = [
        "python3 scripts/tests/test_check_dependency_policy.py",
        "python3 scripts/check_dependency_policy.py",
        "cargo metadata --locked --all-features --format-version 1",
        "cargo audit --file Cargo.lock",
        "cargo deny --locked check bans licenses sources",
        "cargo deny check advisories",
        "git diff --exit-code -- Cargo.lock",
    ]
    for command in required_commands:
        require(command in workflow, f"required CI policy command is missing: {command}")


def dependency_is_denied(
    audit: dict[str, Any], deny: dict[str, Any], case: dict[str, str]
) -> bool:
    """Evaluate deliberately simple synthetic cases used by policy unit tests."""

    kind = case["kind"]
    if kind == "license":
        allowed = set(deny.get("licenses", {}).get("allow", []))
        return case["id"] not in allowed
    if kind == "source":
        sources = deny.get("sources", {})
        source_kind = case["source-kind"]
        allowed = set(sources.get(f"allow-{source_kind}", []))
        fallback = sources.get(f"unknown-{source_kind}")
        return case["id"] not in allowed and fallback == "deny"
    if kind == "advisory":
        threshold = audit.get("advisories", {}).get("severity-threshold")
        ignored = set(audit.get("advisories", {}).get("ignore", []))
        return (
            case["id"] not in ignored
            and SEVERITY_ORDER[case["severity"]] >= SEVERITY_ORDER[threshold]
        )
    raise PolicyError(f"unsupported synthetic case: {kind}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parents[1],
        help="repository root (defaults to the script's parent repository)",
    )
    args = parser.parse_args()
    try:
        validate_policy(args.root.resolve())
    except (OSError, tomllib.TOMLDecodeError, PolicyError) as error:
        print(f"dependency policy error: {error}", file=sys.stderr)
        return 1
    print("dependency policy configuration is valid")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
