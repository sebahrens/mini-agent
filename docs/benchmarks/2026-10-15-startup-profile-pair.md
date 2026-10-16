# Same-source debug and release startup profiles

Measured October 15, 2026 for `mini-agent-i7ro6.5`, original HOW(1).

On this host, release reduced the warm contained-JS self-check median from
**1269.65 ms to 849.41 ms (33.1%)**. The executable shrank from
**154.23 MiB to 24.34 MiB**. Both profiles' warm `--print-config` processes
finished below 40 ms and reported containment as **not probed**. Release changes
the cost materially; a complete contained-worker check still has measurable
startup and teardown cost. These are measurements of the delivered startup/cache
implementation, rather than a historical before/after startup comparison.

| Command | Application cache | Debug median / p95 (ms) | Release median / p95 (ms) |
| --- | --- | ---: | ---: |
| `--print-config` | cold | 53.45 / 76.29 | 41.58 / 50.98 |
| `--print-config` | warm | 35.69 / 36.09 | 23.90 / 27.60 |
| `--js-runtime-check` | cold | 1338.75 / 1413.01 | 866.89 / 891.78 |
| `--js-runtime-check` | warm | 1269.65 / 1287.72 | 849.41 / 854.81 |

Warm JS user-CPU medians were 437.40 ms (debug) and 103.53 ms (release);
system-CPU medians were 108.52 ms and 53.83 ms. These whole-process measurements
include the real self-check and retirement. They do not isolate a single hash
or attribute every remaining millisecond to the containment probe.

## Controlled source and builds

These builds predate `mini-agent-cttyn.1` (commit `115265dd`), which changed the
profiles: dev dependencies now build at opt-level 1 instead of 0 and release uses
opt-level 3 instead of `z`. The numbers below describe the earlier profiles and
were not re-measured.

Both binaries have the exact tracked source tree of delivered commit
`c79475ddcf9744eeff3e6f221c17369f32ee5e5d`, including startup `0941b05f` and
the persistent expected-digest semantics. The private source pin is
`bc9f6e8654ead6266a9b4ca6acad705730e96bc7`; both have Git tree
`a3f5d34b5b216aa25ed9fee2b53d6501cb1ac448`. Cargo default features, Cargo.lock,
compiler, host and build inputs are identical between profiles. Distinct exact
build fingerprints correctly include the different optimization profiles.

Host: Apple M3 Ultra, 32 logical CPUs, macOS 26.4.1 arm64; Homebrew Rust
1.95.0 (`59807616e`), LLVM 22.1.3. This local single-host comparison does not
establish a cross-platform latency target.

The existing startup-specific checkout reused its own warm target, with six
Cargo jobs and no target copying. Builds completed sequentially in 49.20 seconds
(debug) and 4 minutes 28 seconds (release). Root paused other Cargo work before
timing. The one release install was the explicitly authorized benchmark
exception to the development build rule; the ordinary rule is unchanged.

```sh
CARGO_BUILD_JOBS=6 cargo install --path . --locked --offline --debug --root <debug-root>
CARGO_BUILD_JOBS=6 cargo install --path . --locked --offline --profile release --root <release-root>
```

| Profile | Bytes | SHA-256 |
| --- | ---: | --- |
| debug | 161,722,224 | `a3fa1bab769a230734d97a9de91c617e23b1a0f7dc2cf03366984dbfeadefaa8` |
| release | 25,524,880 | `aead2b1268c536740a3123f1ef29be9cf317b894598a07554c6ef8c6ae35aa82` |

Exact build fingerprints, all default features, source/lock hashes, install
paths and the compiler identity are retained in the
[raw record](results/2026-10-15-startup-profile-pair.json).

## Sampling and lifecycle evidence

Each command/profile used five new private application roots and then ten new
processes reusing the last successful root. **Cold** means fresh application
state, including an absent expected-digest record. OS filesystem and dyld caches
were left under ordinary OS management. The reported p95 uses nearest rank;
with these small samples it is the observed maximum. All 60 samples ran serially
after compilation, with a 30-second per-process bound.

The [measurement driver](2026-10-15-startup-profile-pair.py) preserves actual HOME
and supplies a closed, nonsecret environment plus six isolated application-root
overrides. The workspace is an empty fixture directory. The commands invoke no
provider, paid search, login or account mutation. `--js-runtime-check` executes
the real contained arithmetic check and returns `PASS (2)`; it is never replaced
with a fake completed boundary or unit-test timer.

Every cold JS process emitted `source_read`; every warm JS process emitted
`matched_image`. The persisted value remains an expectation. Fresh sealed-image
hashing before guardian execution and the authenticated Ready-time rehash remain
required by the unchanged [canonical worker contract](../specs/phase-6-brokered-js-runtime.md).
Every config sample reported `not probed`, emitted no source-digest evidence and
created no digest record. All samples exited zero and retired their actual
worker/guardian processes before the next sample; no forced cleanup was needed.

Only the missing profile comparison was rerun. The delivered startup scheduling,
cache, cancellation, TUI first-keystroke and model-start proofs remain the owning
implementation evidence. This documentation change adds no runtime code or
permission changes.
