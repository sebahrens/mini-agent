# macOS JS worker cold start: profile and reviewed target exception

Finding: `mini-agent-wldr.19` (release code review 2026-09-08).

The v1.8.0 CI artifact recorded macOS 26 cold Ready p95 at 2678 ms and 1879 ms
against a 300 ms target, and cancel-and-recover p95 at 2172 ms and 1901 ms
against 1000 ms, while warm pure calls stayed under 1.3 ms. The cost was known
to be concentrated in obtaining a fresh contained worker, but not which phase
dominated. This records the profile, the change made, and the resulting target
decision.

## What was added

`launch_executable_unchecked_with_probe` now records a source-free phase profile
for every fresh worker: publication sweep, one-time image preparation (split
into clone, source digest and image digest), Seatbelt profile rendering and
guardian spawn. Only elapsed microseconds are recorded — never paths, identities
or digests — and the resource benchmark prints the breakdown alongside each cold
sample, so a future regression names the phase that grew instead of only its
total.

## Measurement

Host: macOS 26.4.1 (Darwin 25.4.0), Apple Silicon, debug profile, 110 MB worker
executable, installed with `cargo install --path . --debug
--no-default-features --features js`, driven by `js_worker_resource_benchmark`
with its usual 10 warmups and 100 samples.

| Phase | Typical | Share |
| --- | ---: | ---: |
| Total cold Ready | ~835 ms | 100% |
| One-time image preparation | ~385 ms | 46% |
| — sealed image byte proof (SHA-256 of the clone) | ~367 ms | 44% |
| — `clonefile` snapshot | ~0.6 ms | <0.1% |
| — source digest | ~30 ms | 3.6% |
| Publication sweep | ~0.3 ms | <0.1% |
| Seatbelt profile rendering | ~0.008 ms | <0.1% |
| Guardian spawn | ~1.3 ms | 0.2% |
| Process start under Seatbelt through authenticated Ready | ~445 ms | 53% |

Two phases account for 97% of a fresh worker: proving the sealed one-time image
byte-for-byte against its source, and starting the debug worker binary under
Seatbelt until it authenticates Ready. Everything the review suspected might
dominate — the publication sweep, the image snapshot itself, profile rendering,
the guardian launch — is collectively under 1%.

## What was considered and rejected

Memoizing the source digest against the source's exact identity (device, inode,
size and both timestamps) removes ~30 ms per launch after the first, and the
caller already revalidates the descriptor's metadata before and after the copy.
It was implemented, measured, and then reverted: the publisher's normative
contract is to *independently* hash both pinned descriptors on every
publication, and a 3.6% saving does not justify deviating from it without
review.

Raising the image proof's read buffer from 64 KiB to 1 MiB changed nothing
measurable. That phase is bound by materializing the clone's copy-on-write
extents, not by syscall count or SHA throughput, so the smaller buffer was
kept.

## Decision

The 300 ms macOS cold-Ready target cannot be met without removing the sealed
image's byte-for-byte proof, which is a containment guarantee, or without
benchmarking something other than the debug binary the containment matrix
requires. Rather than leave a target that green CI silently fails, the macOS
cold-Ready target is restated at **1000 ms**, matching the measured envelope on
a supported host with the proof intact. Linux (250 ms) and Windows (750 ms) are
unchanged, as are the warm-call, IPC, memory, process-count and idle-runtime
targets, and the enforced security ceilings, which are never inferred from these
measurements.

`post_cancel_recovery` keeps its single cross-platform 1000 ms target. Recovery
replaces the worker, so on macOS it carries the same fresh-launch cost and is
expected to report `false` there. Widening that target would hide the same cost
twice; the honest record is a target that reports what it measures.

## Follow-up

The overlap experiment and matched-host measurements are recorded in
[the September 9 follow-up](2026-09-09-macos-worker-startup-overlap.md).
The original projection of roughly 365 ms savings was too optimistic: the
post-launch remainder above also includes the independent image rehash during
Ready-time unlink, which must remain serialized. The follow-up preserves all
three digest operations and measures the resulting startup overlap directly.
