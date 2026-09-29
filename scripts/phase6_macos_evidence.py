#!/usr/bin/env python3
"""Generate the source-free macOS Phase 6 gate evidence.

The evidence used to hard-code ``assurance`` and ``native_resource_gate`` for
every macOS runner, so a run whose production-binary containment matrix passed
and whose resource artifact recorded a live seatbelt worker still reported
``unavailable-no-production-worker``. That summary cannot tell a supported host
with weaker (deprecated best-effort) containment apart from a host where the
production worker is genuinely unavailable and the runtime fails closed.

Every field below is derived from an actual step result: the validated
production-binary probe, whether the resource step was applicable on this
runner, and whether it ran.
"""

from __future__ import annotations

import argparse
import json
from typing import Any

#: The probe either validated the production binary's containment matrix,
#: reported the host unsupported, failed, or never ran.
PROBE_PASSED = "passed"
PROBE_UNSUPPORTED = "unsupported-host"
PROBE_FAILED = "failed"
PROBE_NOT_RUN = "not-run"

#: The resource measurement is only applicable on runners the matrix selects.
RESOURCE_RECORDED = "recorded"
RESOURCE_SKIPPED = "skipped-unsupported-runner"
RESOURCE_NOT_RUN = "not-run"

#: The live named-FIFO regression (mini-agent-vvzsy, confirmed per runner by
#: mini-agent-vy77b): the worker profile denied opening a named FIFO while
#: inherited anonymous pipes kept working, the test skipped because Seatbelt
#: was unusable (which the workflow rejects), it failed, or it never ran.
NAMED_FIFO_DENIED = "denied"
NAMED_FIFO_SKIPPED = "skipped-seatbelt-unusable"
NAMED_FIFO_FAILED = "failed"
NAMED_FIFO_NOT_RUN = "not-run"
NAMED_FIFO_RESULTS = (
    NAMED_FIFO_DENIED,
    NAMED_FIFO_SKIPPED,
    NAMED_FIFO_FAILED,
    NAMED_FIFO_NOT_RUN,
)


def containment_evidence(
    probe: str, resource: str, named_fifo: str = NAMED_FIFO_NOT_RUN
) -> dict[str, Any]:
    """Availability, assurance and native-gate evidence for one macOS run."""
    if probe == PROBE_PASSED:
        availability = "available"
        # Matches the runtime's typed status for a seatbelt worker: contained,
        # but on a deprecated best-effort mechanism rather than an enforced one.
        assurance = "deprecated-best-effort"
        # macOS exposes no probed native resource gate; that is a property of
        # the platform, not evidence that no production worker exists.
        native_gate = "unavailable-no-native-mechanism"
    elif probe == PROBE_UNSUPPORTED:
        availability = "unavailable"
        assurance = "fail-closed-unavailable"
        native_gate = "unavailable-no-production-worker"
    elif probe == PROBE_FAILED:
        availability = "unknown"
        assurance = "unknown-probe-failed"
        native_gate = "unknown-probe-failed"
    else:
        availability = "unknown"
        assurance = "unknown-probe-not-run"
        native_gate = "unknown-probe-not-run"

    evidence = {
        "backend": "seatbelt",
        "worker_availability": availability,
        "assurance": assurance,
        "real_probe": probe,
        "production_binary_matrix": probe,
        "resource_measurement": resource,
        "native_resource_gate": native_gate,
        "named_fifo_denial": named_fifo,
    }
    validate(evidence)
    return evidence


class InconsistentEvidence(Exception):
    """The generated evidence contradicts itself."""


def validate(containment: dict[str, Any]) -> None:
    """Reject a summary that cannot describe a single real run."""
    named_fifo = containment.get("named_fifo_denial", NAMED_FIFO_NOT_RUN)
    if named_fifo not in NAMED_FIFO_RESULTS:
        raise InconsistentEvidence(f"unknown named-FIFO result {named_fifo!r}")
    measured = containment["resource_measurement"] == RESOURCE_RECORDED
    no_worker = containment["native_resource_gate"] == "unavailable-no-production-worker"
    if measured and no_worker:
        raise InconsistentEvidence(
            "measured production-worker resources cannot coexist with "
            "unavailable-no-production-worker"
        )
    if measured and containment["worker_availability"] != "available":
        raise InconsistentEvidence(
            "resources were measured, so the production worker was available"
        )
    if containment["real_probe"] != PROBE_PASSED and containment["assurance"].startswith(
        "deprecated-best-effort"
    ):
        raise InconsistentEvidence(
            "containment assurance must not be claimed without a passing probe"
        )


def build_evidence(
    runner: str,
    job_status: str,
    probe: str,
    adversarial: str,
    resource: str,
    named_fifo: str = NAMED_FIFO_NOT_RUN,
) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "platform": "macos",
        "runner": runner,
        "result": job_status,
        "containment": containment_evidence(probe, resource, named_fifo),
        "adversarial": {
            "feature_rows": ["js", "skills"],
            "suites": adversarial,
            "zero_test_guard": "enforced",
        },
        "raw_output": False,
    }


def summary_line(evidence: dict[str, Any]) -> str:
    containment = evidence["containment"]
    return (
        f"PHASE6_GATE platform=macos runner={evidence['runner']} "
        f"result={evidence['result']} probe={containment['real_probe']} "
        f"worker={containment['worker_availability']} "
        f"assurance={containment['assurance']} "
        f"resources={containment['resource_measurement']} "
        f"named_fifo={containment['named_fifo_denial']} "
        f"suites={evidence['adversarial']['suites']} raw_output=false"
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runner", required=True)
    parser.add_argument("--job-status", required=True)
    parser.add_argument("--probe", required=True)
    parser.add_argument("--adversarial", required=True)
    parser.add_argument("--resource", required=True)
    parser.add_argument(
        "--named-fifo", choices=NAMED_FIFO_RESULTS, default=NAMED_FIFO_NOT_RUN
    )
    parser.add_argument("--json-out", required=True)
    parser.add_argument("--log-out", required=True)
    args = parser.parse_args(argv)

    evidence = build_evidence(
        args.runner,
        args.job_status,
        args.probe,
        args.adversarial,
        args.resource,
        args.named_fifo,
    )
    with open(args.json_out, "w", encoding="utf-8") as handle:
        json.dump(evidence, handle, indent=2, sort_keys=True)
        handle.write("\n")
    with open(args.log_out, "w", encoding="utf-8") as handle:
        handle.write(summary_line(evidence))
        handle.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
