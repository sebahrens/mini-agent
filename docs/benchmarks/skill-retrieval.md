# Learned-JS skill retrieval benchmark

Phase 3 uses a checked-in deterministic harness to measure the production immutable HNSW-dense
plus SQLite FTS5/RRF index against an exact contiguous oracle. Query embedding and index search are timed separately. The full corpus
contains 100,000 active content-addressed revisions with semantic, identifier, mixed, irrelevant,
near-duplicate, and lifecycle-filtered cases; vectors use the production deterministic backend's
384 dimensions.

Run the non-flaky CI smoke audit with:

```bash
cargo test --features js,skills skill_retrieval_benchmark_smoke -- --nocapture
```

Run the full debug-profile audit with:

```bash
ZS_SKILL_BENCH_FULL=1 cargo test --features js,skills skill_retrieval_benchmark -- --ignored --nocapture
```

Each run emits one machine-readable JSON line and a human summary. The same JSON is written to
`$TMPDIR/mini-agent-skill-retrieval-latest.json`. It includes the deterministic seed, corpus mix,
model identity/revision/dimensions, OS/CPU/RAM, sample counts, cold and warm embedding latency,
dense/FTS/fusion and total-search percentiles, build/rebuild/removal costs, concurrent-reader
latency, observed RSS, relevance checks, and the 5 ms p99 verdict. CI validates fields and
invariants at 2,000 revisions but deliberately does not apply a host-sensitive latency gate.

## Latest reference result

The accepted 2026-07-31 debug-profile run used an Intel Core i7-1068NG7 (8 logical CPUs), 32 GiB
RAM, macOS x86_64, the deterministic-v1 model, 384 dimensions, 100,000 revisions, and 60 search
samples over corpus-wide self queries and normalized hard blends. HNSW used 24 construction
connections, `ef_construction=100`, an `ef_search` floor of 36, a 32-candidate production frontier,
and a 40-candidate recall-audit frontier; exact contiguous matrix search remained the oracle.

| Metric | Result |
|---|---:|
| Total index search p50 / p95 / p99 | 2.638 / 3.990 / **4.501 ms** |
| ANN dense p99 | 3.380 ms |
| FTS candidates p99 | 1.489 ms |
| Fusion/dedupe/budget p99 | 0.118 ms |
| Exact-oracle p99 | 70.575 ms |
| ANN recall@10 against exact | **96.75%** |
| Independent-rebuild recall@10 | **98.5%** |
| Self-query top-1 rate | **98%** |
| Concurrent-reader p99 | 3.627 ms |
| Snapshot build / rebuild | 92.750 / 110.369 s |
| 5,000-row lifecycle visibility mask | 5.567 ms |
| Cold / warm query embedding | 0.383 / 0.044 ms |
| Observed peak RSS | 1,665,412 KiB |

The p99 gate is ≤5 ms and the recall@10 gate is ≥95%; both pass. Ordering is deterministic within
an immutable generation; independently built randomized HNSW graphs must each meet the exact-oracle
recall gate. Lifecycle removal applies a bounded immutable visibility mask immediately, proves
5,000 removed IDs absent, and defers physical compaction to a later rebuild. The durable record is
[`results/skill-retrieval-2026-07-31.json`](results/skill-retrieval-2026-07-31.json).

## Phase 5 lifecycle and evidence operations

The Phase 5 audit retains 100,000 revision rows and measures lifecycle-adjacent operations
separately from retrieval. Run it with:

```bash
ZS_SKILL_BENCH_FULL=1 cargo test --features js,skills phase5_operations_benchmark -- --ignored --nocapture
```

The accepted 2026-08-01 debug-profile run passed every deliberately conservative budget:

| Operation | Result | Budget |
|---|---:|---:|
| Deterministic routing, 100,000 turns | 826.323 ms | ≤2,000 ms |
| Durable ingestion, 256 events | 30.837 ms | ≤500 ms |
| Promotion policy, 10,000 evaluations | 978.692 ms | ≤5,000 ms |
| Raw-event compaction, 256 events | 5.196 ms | ≤1,000 ms |
| Generation-state refresh check | 0.102 ms | ≤100 ms |
| Privacy purge at 100,000 retained rows | 49.219 ms | ≤1,000 ms |

The machine-readable record is
[`results/phase5-operations-2026-08-01.json`](results/phase5-operations-2026-08-01.json).
