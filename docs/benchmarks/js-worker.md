# JavaScript worker resource benchmark

This debug-profile benchmark measures whether mini-agent's single contained JavaScript worker is
small and responsive. It does not turn host-sensitive timings into a shared-runner gate, and it
does not treat observed memory or CPU use as a security boundary. The native 256 MiB address-space
and 35 CPU-second ceilings are enforced and probed by the platform containment jobs separately.

The checked-in [baseline manifest](results/js-worker-baseline.json) is the reviewed aggregate from
the dedicated Linux, macOS 26, and Windows reference runners. It describes those exact machines
and builds; it is not a claim about unmeasured hosts. A platform whose production containment is
unavailable emits a status-only
`containment_unavailable` record with closed backend, assurance, and reason code. It contains no
latency, memory, or count fields and does not satisfy a resource target. Windows may emit
measurements only after the configured
installed executable independently passes the same runtime LPAC child attestation,
Job/resource/mitigation/child-process-policy checks, authenticated protocol, and clean-shutdown
preflight as production status. The caller wait starts before launch preparation; the propagated
deadline prevents a creation-lock wait that finishes late from starting a process. Because
synchronous `CreateProcessW` is not cancellable, a result that returns late remains owned by the
helper until teardown rather than implying a hard bound on the worker's complete lifetime. The
benchmark's executable-specific cache is test-only and cannot alter production availability.

## Reviewed reference evidence

CI run `31319107422` at commit `aa203c5` produced the three platform records used by the checked-in
aggregate. The final aggregator at commit `9c6f164` independently revalidated those records
after allowing only an eight-machine-epsilon tolerance for a derived delta that changed by one
floating-point representation step during a cross-platform JSON round trip.

| Platform | Cold Ready p95 | Warm call p95 | 4 KiB IPC p95 | Recovery p95 | Max private memory | Worker / helpers / idle runtimes |
|----------|---------------:|--------------:|----------------:|-------------:|-------------------:|----------------------------------:|
| Linux | 8.213 ms | 2.422 ms | 2.895 ms | 19.687 ms | 17.75 MiB | 1 / 2 / 0 |
| macOS 26 | 7,267.302 ms | 1.649 ms | 1.892 ms | 7,024.533 ms | 3.52 MiB | 1 / 1 / 0 |
| Windows | 19.060 ms | 2.219 ms | 2.613 ms | 29.632 ms | 2.06 MiB | 1 / 0 / 0 |

Linux and Windows pass every initial performance goal and their complete comparison pairs remain
within the documented variance. macOS passes the warm-call, IPC, memory, one-worker, and
zero-idle-runtime goals. Its two runs record cold Ready at 7.53 s / 7.27 s and post-cancel recovery
at 8.77 s / 7.02 s, above the initial 300 ms / 1 s goals. Cold Ready is within the documented
pairwise variance; recovery and the faster warm/IPC observations are not, so the macOS comparison
is explicitly recorded as noisy rather than rewritten to pass.

The macOS cold and recovery measurements include the complete one-time-image Seatbelt and guardian
preflight or teardown. For the first release, review accepts that reference-runner cost without
weakening those security checks or changing the targets: the process remains warm for normal calls,
shared-runner timings are informational, and a matched otherwise-quiet-host result has not been
promoted to a release blocker. Follow-up `mini-agent-avx3` owns profiling and optimization with the
same containment guarantees. The independently enforced native limits remain separate from this
performance review.

## Reference method

Install the production crate with `cargo install --path . --debug`, then use that installed debug
application as the worker executable. The harness rejects its own libtest executable, so startup
and private-memory evidence cover the real application worker entry point rather than Rust's test
harness. A reference record identifies the host, OS,
architecture, kernel, CPU, logical CPU count, RAM, package version, and exact build identity. Keep
the machine otherwise idle and use the same runner image or physical host when comparing runs.

Every latency series discards 10 warmups and retains 100 samples. The harness reports the mean,
nearest-rank p50 and p95, unbiased sample variance, standard deviation, minimum, and maximum in
microseconds. Idle private memory also discards 10 reads and retains 100. Platform measurements
are:

- Linux: private clean, dirty, and hugetlb pages from `/proc/<pid>/smaps_rollup`.
- macOS: physical footprint from `vmmap -summary`.
- Windows: `PrivateMemorySize64` from PowerShell `Get-Process`.

The measured scenarios are cold authenticated `Ready` without a JS call, a warm pure `42` call, a
4 KiB `read_file` broker round trip using a benchmark-only in-memory effect handler, idle private
memory, and cancellation followed by a successful pure call on a fresh generation. The broker IPC
measurement deliberately excludes permission prompts, durable audit synchronization, and real
filesystem I/O. On Linux, the harness walks the bounded `/proc` containment tree after
authenticated `Ready`, identifies exactly one worker by the configured installed executable's
device/inode,
samples that exact worker rather than the bwrap namespace-init helper, and reports helper counts
separately. Idle private memory is exact-worker-only and deliberately excludes bwrap helper
overhead. Ambiguous, changing, or multi-worker trees fail closed. On macOS 26, bounded `libproc`
enumeration requires exactly one non-guardian member in the authenticated guardian's dedicated
process group; the trusted guardian is counted separately, and memory is measured for the exact
worker only. On an attested Windows backend, the harness uses the directly owned `CreateProcessW`
application PID and queries the
owned creation-time Job's active-process count; the Job handle is authority, not a helper process.
It reports zero idle QuickJS
runtimes as a protocol/lifecycle proof rather than a cross-process measurement: authenticated
`StepResult` is emitted only after request-local `execute_fresh_step` returns and drops its
`Runtime`; schema validation requires that exact proof label.

Run one measurement and write the upload artifact with:

```bash
MINI_AGENT_JS_WORKER_BENCH_EXE="${CARGO_HOME:-$HOME/.cargo}/bin/mini-agent" \
MINI_AGENT_JS_WORKER_BENCH=1 \
MINI_AGENT_JS_WORKER_BENCH_OUTPUT="$PWD/js-worker-${RUNNER_OS:-local}.json" \
cargo test --locked --no-default-features --features js \
  js_worker_resource_benchmark -- --ignored --exact --nocapture
```

Use `mini-agent.exe` on Windows. `MINI_AGENT_JS_WORKER_BENCH_EXE` is required only when production
containment is available and the platform emits measurements; unavailable platforms still emit a
status-only artifact without launching a worker.

The exact test filter may be prefixed by the Rust module path on some runners; if it selects zero
tests, use `js_worker_resource_benchmark` without `--exact`. CI should upload the resulting JSON
even when a p95 target is missed. Schema errors, wrong results, ambiguous/multiple workers, or
inability to measure memory on an available backend remain hard failures. Containment
unavailability produces a valid status-only artifact instead of running measurements.

Each reference pair is observation collection, not a target exit gate. A single performance-goal
miss is recorded in `target_results` and does not block Phase 6. It becomes blocking only after the
miss is reproduced on a matched, otherwise quiet host using the comparison method below and a
review explicitly promotes that repeatable miss to a blocking acceptance issue. Until then the
checked-in aggregate reports what was observed without converting a target boolean into a verdict.

To create the repeatability record required in the three-platform aggregate, run once to a
`-reference.json` output, then run the same command again with:

```bash
MINI_AGENT_JS_WORKER_BENCH_COMPARE=/path/to/previous.json
```

The comparison input is schema-validated and must match the current host, OS, architecture,
kernel, CPU, logical CPU count, RAM, debug profile, package/build identity, and containment
backend/assurance exactly. On Windows, the kernel identity includes the operating-system caption,
version, and build number from `Win32_OperatingSystem`; a generic `Windows_NT` label is not
measured evidence. Measurement values and target booleans may differ. The final result
retains the reference machine and containment identity, records the previous/current p95 values,
each derived relative delta against the
documented 15% repeatability envelope, and an aggregate `all_within_documented_variance` flag.
That flag is evidence, not a test verdict. Single-platform reference artifacts may omit comparison;
an aggregate rejects every measured record that omits it. Repeat an out-of-envelope run or target
miss on the same quiet host before investigating. A repeatable miss remains recorded evidence
unless review explicitly makes it blocking; optimize a promoted blocking miss before changing a
target. Any target amendment requires a reviewed rationale and checked-in evidence.

After downloading the three artifacts, aggregate and validate them deterministically with:

```bash
MINI_AGENT_JS_WORKER_BENCH_INPUTS="linux.json:macos.json:windows.json" \
MINI_AGENT_JS_WORKER_BENCH_OUTPUT="$PWD/docs/benchmarks/results/js-worker-baseline.json" \
cargo test --locked --no-default-features --features js \
  js_worker_resource_aggregate -- --ignored --nocapture
```

Use semicolon-separated paths in PowerShell. Aggregation rejects malformed reports, more or fewer
than one platform record per artifact, duplicates, and any set other than exactly Linux, macOS,
and Windows. Each record may contain a measured run or a closed status-only unavailable result. The
aggregate command schema-validates the generated output before writing it. It sorts platform
evidence in that order and prints the complete source-free JSON to CI output as
well as writing the baseline file, so evidence can still be recovered when artifact download
credentials are not available locally.

## Performance goals

| Measurement | Linux | macOS | Windows |
|---|---:|---:|---:|
| Cold authenticated Ready, p95 | ≤250 ms | ≤300 ms | ≤750 ms |
| Warm pure call, p95 | ≤10 ms | ≤10 ms | ≤10 ms |
| 4 KiB broker IPC, p95 | ≤10 ms | ≤10 ms | ≤10 ms |
| Idle private memory, maximum observed | ≤32 MiB | ≤32 MiB | ≤32 MiB |
| Post-cancel recovery, p95 | ≤1 s | ≤1 s | ≤1 s |
| Live worker / idle runtimes | 1 / 0 | 1 / 0 | 1 / 0 |

These are performance goals only. The independently enforced security ceilings remain 256 MiB of
native process memory and 35 CPU seconds regardless of benchmark behavior. A status-only
unavailable record has no performance result and therefore passes none of these goals.

## CI integration

The dedicated CI jobs install the debug binary after each platform containment probe and run the
command above twice: first to a reference path,
then to the final path with `MINI_AGENT_JS_WORKER_BENCH_COMPARE` naming the reference. Upload
`js-worker-${RUNNER_OS}.json` as an artifact named `js-worker-resource-${RUNNER_OS}`. Do not add a
condition on any `target_results` or comparison field unless review has promoted a repeatable miss
to a blocking acceptance issue. Unavailable platforms still upload their status-only evidence. The
final evidence job downloads all three artifacts and uses
`js_worker_resource_aggregate` to require, sort, and schema-validate one record per OS before
accepting the generated manifest. The separate
`worker_resource_baseline_manifest_is_honest_and_schema_valid` test protects the checked-in
manifest's schema and evidence-state contract.
