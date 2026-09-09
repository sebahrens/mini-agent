# macOS worker startup overlap

Finding: `mini-agent-jnuz`. Measured September 9, 2026, against commit `759a66f`.

Starting the contained worker while the parent hashes its sealed image reduced
observed mean cold Ready from **1064.40 ms to 1056.05 ms**
(0.78%). Cold Ready p95 was
**1073.52 ms before and 1061.33 ms after**. This is a small local
observation, below the benchmark's 15% repeat-run tolerance, and does not
establish a portable performance improvement. The 1000 ms cold-Ready target
remains unmet and unchanged.

| Latency p95 | Before (ms) | After (ms) |
| --- | ---: | ---: |
| Cold Ready | 1073.517 | 1061.325 |
| Cancel and recover | 1057.204 | 1055.913 |
| Warm pure call | 0.690 | 0.678 |
| Broker IPC, 4 KiB | 0.833 | 0.825 |

Both runs satisfy the one-worker/zero-idle-runtime proof and the warm-call, IPC
and idle-private-memory targets. Cold Ready and cancellation recovery remain
over their 1000 ms targets. Timing targets are informational.

## Ordering and containment

The parent independently hashes the source, seals the distinct single-link
image to `0500`, closes its writable descriptor, and reopens it read-only.
After validating ownership, ACLs and identity, it starts the trusted guardian
and completes the image digest and publication checks while the contained
worker starts. A pending guardian guard kills the process group and reaps the
guardian before publication cleanup on any later proof failure. The supervisor
receives the process and pipes only after successful proof, so it cannot send
a bootstrap challenge or dispatch work beforehand.

Authenticated Ready still triggers descriptor and pathname revalidation and
another image hash before unlink and sequence 2. The guardian heartbeat,
resource limits, deny-default profile, one-worker launch gate, and fresh
request runtimes remain intact.

## Measurement method

Both runs used the same Apple M3 Ultra host, 32 logical CPUs, 512 GiB RAM,
macOS 26.4.1 / Darwin 25.4.0, Rust debug profile, and `--no-default-features
--features js`. The binaries were installed with `cargo install --offline
--locked --path . --debug` plus those feature flags. The canonical
`js_worker_resource_benchmark` used 10 warmups and 100 samples for each latency
metric, with serialized runs and matching binary/harness build fingerprints.
`RUNNER_NAME=macos-local-m3-ultra` supplied the required stable host label.

The raw records retain their distinct exact-build identities:
[before](results/2026-09-09-macos-worker-startup-before.json) and
[after](results/2026-09-09-macos-worker-startup-after.json).
They are local single-platform records; they do not replace the pending
cross-platform baseline manifest. The benchmark's built-in comparison mode
requires the same exact build, so this code-change comparison uses the two
independent measured records instead.

The earlier estimate of a 365 ms saving incorrectly attributed the entire
post-launch remainder to process startup. That interval also contains the
Ready-time image rehash. Here the source hash alone averaged about 362 ms and
the initial image hash about 334 ms. All three digest operations are preserved;
only process startup overlaps the initial image proof.

## Validation

The worker suite passed 240 unit tests and all 7 bootstrap integration tests.
The subprocess inventory passed all 27 tests. All three required Clippy rows
and the minimal-JS and default-plus-skills/LSP debug installs passed. Regression
coverage checks startup after sealing but before digest completion, cleanup
ordering at every publication fault stage, actual child reaping on digest
mismatch, spawn failure, and missing protocol pipes. The production binary
also passed the native denial, guardian, and one-time-image containment matrix.
