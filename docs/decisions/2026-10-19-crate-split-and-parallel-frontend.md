# Harness compile time, crate split and the parallel frontend

Beads: mini-agent-vbwia (profile the test-harness compile), mini-agent-r3fgs (crate split vs
parallel frontend). Decision: **no crate split, no repository-level parallel-frontend setting, and no
test-only code change**; the measurements below show no safe test-side win and a split that would
not move the incremental floor.

## Method

32-core Apple-silicon Mac, quiet machine, Homebrew rustc/cargo 1.95.0 (the `rust-toolchain.toml`
pin of 1.96.0 is not honoured by Homebrew, see CONTRIBUTING.md), dependencies cached, base commit
862916f4. "Full" means `CARGO_INCREMENTAL=0` after `touch src/hex.rs`, so the whole `mini-agent`
crate is recompiled; "incremental" appends one `pub(crate) const` to a source file (touching the
mtime alone is not an edit: rustc compares content hashes and the build finishes in the same time).
Unstable flags were applied with `RUSTC_BOOTSTRAP=1 cargo rustc --bin mini-agent [--profile test] --
-Z ...`, which applies them to this crate only. Switching `RUSTC_BOOTSTRAP` on or off rebuilds every
dependency once (that run takes ~84 s and ~407 s of CPU and was discarded). Wall seconds / CPU
seconds (user), median of the runs listed.

## Where the harness compile goes (vbwia)

The test harness (`cargo test --bin mini-agent --no-run`) is 60.1 s / 97 CPU-s against 27.1 s / 44
CPU-s for the production bin. Average parallelism is only 1.6 cores. All-target `cargo test
--no-run` is 73.1 s / 188 CPU-s (adds the integration-test binaries).

`-Z time-passes` on the harness (total 60.9 s):

| Pass | s | share |
| --- | --- | --- |
| `type_check_crate` | 13.9 | 23% |
| `MIR_borrow_checking` | 14.4 | 24% |
| `coherence_checking` | 5.7 | 9% |
| `expand_crate` (macros) | 2.2 | 4% |
| frontend subtotal, serial | ~36 | 59% |
| `monomorphization_collector_graph_walk` | 10.4 | 17% |
| `codegen_to_LLVM_IR` | 10.4 | 17% |
| `partition_and_assert_distinct_symbols` | 2.9 | 5% |
| `LLVM_passes` (overlaps the above, parallel) | 11.8 | |
| `link` | 1.7 | 3% |

Attribution of generated code, from `--emit=llvm-ir` (29.0 M IR lines, grouped by symbol path):
generic code from dependencies instantiated inside mini-agent is the bulk: tokio 15.5%, core 14.1%,
agent-client-protocol 6.1%, std 3.7%, alloc 2.7%, rig-core 2.2%, serde_core 1.6%, rmcp 0.7%.
mini-agent's own code is much smaller: `src/tests` 4.0%, `ui` 3.7% (+0.9% ui tests), `agent` 2.8%,
`provider` 1.9%, `extras::acp` 1.3%, `extras::js` 1.2% (+1.2% tests). The largest single
test-attributed source is `extras::acp::protocol_tests` (6.7% including the SDK generics it
instantiates: 89 tests, 40 `connect_with` call sites, each with a distinct closure type). Heavy
production generics are shared with the bin: `spawn_agent_with_start_mode` and
`build_agent_inner_with_snapshot` are instantiated once per provider (7 times, 10-16 k IR lines each),
`Config`'s `Deserialize::visit_map` once per deserializer (8 times, ~20 k lines each), and
`spawn_blocking_scoped_on` / `spawn_async_scoped` closures add ~3.7% through tokio's task
machinery. The 20 largest functions are 10-21 k IR lines out of 29 M, so no single outlier exists.

Marginal cost of test modules (full rebuild, `-Z threads=1`, baseline 61.0 s):

| Change | Wall |
| --- | --- |
| comment out `subprocess_inventory_tests` + `worktree_tests` + `checker_tests` (15.8 k lines, the three largest proof-style files) | 60.1 s (-1.1 s, 1.7%) |
| `-C codegen-units=16` / `64` (default 256) | 60.9 / 59.1 s (noise) |

Test LOC is 63 k in `src/tests` plus inline test modules, but compile cost follows monomorphised
async/generic instantiations, not lines: pure proof/data tests are nearly free. Moving fixtures to
`include_str!` or gating the proof modules behind a feature would save at most ~1 s of 60 and would
remove tests from the default run, so none of it was done. The only larger test-side lever would be
funnelling the 40 ACP `connect_with` sites through one boxed-closure helper (up to ~6% of IR, i.e.
2-3 s at best); it rewrites 89 tests for a few seconds and is not recommended without a
measurement-driven reason. The cheap production-side lever is the same instantiation fan-out
(providers x agent spawn/build, Config x deserializers), which also benefits the bin; it changes
production signatures and belongs in its own bead if wanted.

## Incremental floor

| Edit (one added `const`) | harness | bin |
| --- | --- | --- |
| `src/fs.rs` | 14.0-15.0 s | 6.6-7.0 s |
| `src/ui/app.rs` | 13.9-15.0 s | 6.5-6.8 s |
| `src/agent/mod.rs` | 13.6-14.9 s | 6.4-6.9 s |
| mtime-only touch of any of the three | 12.8-13.3 s | 5.9-6.1 s |

The floor does not depend on which module changes. `-Z time-passes` of an incremental harness
rebuild (15.2 s): macro expansion 2.3 s, codegen 3.9 s, link 1.8 s, dep-graph/result-cache
persistence 1.8 s, partition 1.8 s, mono collection 1.3 s, borrowck/typeck ~1.8 s, plus ~5 s of
system time (incremental session I/O). Re-checking is not the dominant term.

## Parallel frontend (`-Z threads=N`, needs nightly or `RUSTC_BOOTSTRAP`)

Full rebuild, `CARGO_INCREMENTAL=0`:

| Threads | harness wall / CPU | bin wall / CPU |
| --- | --- | --- |
| 1 (default) | 60.1-61.2 s / 97 | 27.1-27.6 s / 44 |
| 4 | 39.7 s / 109 | |
| 8 | 34.1 s / 118 | 14.9 s / 54 |
| 16 | 29.5 s / 141 | 12.6 s / 64 |
| 32 | 29.3 s / 162 | |

Incremental (edit in `src/ui/app.rs`, steady state after the cache is rebuilt for the new flag):
harness 14.8 s -> 13.4 s with 8 threads; bin 6.8 s -> 6.2 s.

The parallel frontend nearly halves a cold or CI-style full build (-43% harness, -46% bin at 8
threads) but only recovers ~10% of the incremental edit loop, which is what contributors mostly
hit. It is not available on stable: it requires a nightly toolchain or `RUSTC_BOOTSTRAP=1`
(unsupported), and changing that environment invalidates the dependency cache. CI pins the stable
1.96.0 toolchain for reproducibility. Adopting it would change both.

## Crate split (`ui` above everything)

Facts from the module-graph analysis: 43 of 61 top-level modules form one strongly connected
component; 4,005 non-test cross-module import sites; a heuristic minimum feedback arc set is 383
sites across 91 module pairs (session->agent 49, agent->js 24, sandbox->js 17). Only `ui` is cheap
to lift: about 38 back-edge sites across 13 pairs point into it (context->ui 9, print->ui 8,
session->ui 5, config->ui 3, auth->ui 2, spend->ui 2, provider->ui 2, startup->ui 2, ...), so the
type dependencies (themes, print formatting types) would first have to move down. A per-feature
split is not viable (extras depend upward on agent/startup/ui) and would touch the Phase 6
sandbox<->js surface.

The measurements above remove the case for the split: a one-line edit costs the same ~14 s in
harness whichever module is touched, and the floor is made of per-crate fixed costs (macro
expansion, codegen, link, incremental persistence) that a lib crate would pay again for the
`ui` crate plus the remainder. A split would cut the full-build serial frontend at best by the
share of `ui` code (~70 k of 488 k LOC, 14%) while adding cross-crate generic and inlining costs,
a second crate to link-test, and 38 sites of refactoring on the security-relevant startup and
session paths.

## Recommendation

1. Do not split the crate (r3fgs). Revisit only if the incremental floor, not the full build,
   becomes the complaint and `ui` grows well beyond its current share.
2. Do not set `-Z threads` in repository config. Contributors who already run nightly can opt in
   locally with a separate target directory, for example
   `CARGO_TARGET_DIR=target-nightly RUSTFLAGS="-Z threads=8" cargo +nightly test --no-run`; expect
   roughly 40% off cold builds and ~10% off incremental ones. Re-evaluate when the parallel
   frontend reaches stable.
3. No test-side code change (vbwia): there is no outlier module and the biggest test files cost
   ~1 s. If compile time must come down further, the highest-leverage items are the per-provider
   instantiation fan-out of `spawn_agent_with_start_mode` / `build_agent_inner_with_snapshot`, the
   8x `Config` deserializer instantiation and the 40 ACP `connect_with` test closures. Each needs
   its own measured bead.
4. Run CI-style local builds on the pinned rustup toolchain (CONTRIBUTING.md, Toolchain) so
   timings and Clippy results match CI.
