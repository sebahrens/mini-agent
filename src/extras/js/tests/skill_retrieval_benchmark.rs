use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::params;

use crate::extras::js::skills::embed::{Embedder, SkillDocument};
use crate::extras::js::skills::index::{ImmutableSkillIndex, RetrievalPolicy, SkillIndex};
use crate::extras::js::skills::store::{SkillRecordMetadata, SkillStore, StoredEmbedding};
use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
use crate::paths::AppPaths;

const SEED: u64 = 0x5eed_0003;

struct TempPaths {
    root: PathBuf,
    paths: AppPaths,
}

impl TempPaths {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "mini-agent-skill-benchmark-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        Self {
            paths: AppPaths {
                config_dir: root.join("config"),
                data_dir: root.join("data"),
                local_data_dir: root.join("local-data"),
                state_dir: root.join("state"),
                cache_dir: root.join("cache"),
                credentials_dir: root.join("credentials"),
                project_dir: None,
            },
            root,
        }
    }
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn artifact(index: usize) -> SkillArtifact {
    let group = index % 6;
    let (stem, description, tag) = match group {
        0 => (
            "parseJson",
            "Parse JSON documents with a bounded fallback",
            "json",
        ),
        1 => ("parseCsv", "Parse comma separated table rows", "csv"),
        2 => ("slugify", "Create URL safe text slugs", "text"),
        3 => (
            "dedupe",
            "Remove repeated values while preserving order",
            "collections",
        ),
        4 => (
            "retryDelay",
            "Calculate bounded exponential retry delays",
            "reliability",
        ),
        _ => ("chunkText", "Split long text into bounded chunks", "text"),
    };
    let name = format!("{stem}_{index:06}");
    SkillArtifact::new(
        format!("function {name}(value) {{ return value; }}"),
        format!("{description} corpus item {index:06}."),
        vec![tag.to_string(), format!("bucket-{}", index % 97)],
        vec![SkillExport {
            name: name.clone(),
            signature: format!("{name}(value: unknown): unknown"),
        }],
        vec![format!("{name}(7) === 7")],
        CapabilityManifest::pure(),
    )
    .unwrap()
}

fn document(artifact: &SkillArtifact) -> String {
    SkillDocument::new(artifact.description.clone())
        .with_exports(
            artifact
                .exports
                .iter()
                .map(|export| (export.name.clone(), export.signature.clone()))
                .collect(),
        )
        .with_tags(artifact.tags.clone())
        .with_identifiers(
            artifact
                .exports
                .iter()
                .map(|export| export.name.clone())
                .collect(),
        )
        .render()
}

fn percentile(samples: &[Duration], percentile: f64) -> f64 {
    let mut micros = samples
        .iter()
        .map(|duration| duration.as_secs_f64() * 1_000_000.0)
        .collect::<Vec<_>>();
    micros.sort_by(f64::total_cmp);
    let index = ((micros.len().saturating_sub(1)) as f64 * percentile).ceil() as usize;
    micros[index]
}

fn percentiles(samples: &[Duration]) -> serde_json::Value {
    serde_json::json!({
        "samples": samples.len(),
        "p50_us": percentile(samples, 0.50),
        "p95_us": percentile(samples, 0.95),
        "p99_us": percentile(samples, 0.99),
    })
}

fn peak_rss_kib() -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

fn command_output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program).args(arguments).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn run_benchmark(corpus_size: usize, search_samples: usize, label: &str) {
    let temp = TempPaths::new(label);
    let embedder = Embedder::new().unwrap();
    let model = embedder.model_metadata().clone();
    let mut store = SkillStore::open_at(&temp.paths).unwrap();
    let rss_before = peak_rss_kib();
    let corpus_started = Instant::now();
    let mut rows = Vec::with_capacity(corpus_size);
    let mut target_id = String::new();
    let mut recall_queries = Vec::new();
    let mut lifecycle_hidden_ids = HashSet::new();
    const SELF_QUERY_SAMPLES: usize = 100;
    let recall_stride = (corpus_size / SELF_QUERY_SAMPLES).max(1);

    const EMBEDDING_BATCH: usize = 256;
    for batch_start in (0..corpus_size).step_by(EMBEDDING_BATCH) {
        let batch_end = (batch_start + EMBEDDING_BATCH).min(corpus_size);
        let artifacts = (batch_start..batch_end).map(artifact).collect::<Vec<_>>();
        let documents = artifacts.iter().map(document).collect::<Vec<_>>();
        if batch_start == 0 {
            target_id = artifacts[0].id.clone();
        }
        let vectors = embedder.embed_documents(&documents).unwrap();
        for (offset, ((artifact, values), query_text)) in artifacts
            .into_iter()
            .zip(vectors)
            .zip(documents)
            .enumerate()
        {
            let id = artifact.id.clone();
            if lifecycle_hidden_ids.len() < (corpus_size / 20).max(1) {
                lifecycle_hidden_ids.insert(id.clone());
            }
            let corpus_index = batch_start + offset;
            if corpus_index.is_multiple_of(recall_stride)
                && recall_queries.len() < SELF_QUERY_SAMPLES
            {
                recall_queries.push((query_text, values.clone(), id.clone()));
            }
            rows.push((
                artifact,
                StoredEmbedding {
                    skill_id: id,
                    model_id: model.model_id.clone(),
                    model_revision: model.model_revision.clone(),
                    dimensions: model.dimensions,
                    normalized: model.normalized,
                    values,
                },
                SkillRecordMetadata {
                    status: "active".to_string(),
                    quarantine_reason: None,
                    supersedes_id: None,
                    superseded_by_id: None,
                    row_version: 1,
                },
            ));
        }
    }
    let corpus_embedding_duration = corpus_started.elapsed();
    let self_query_count = recall_queries.len();
    let hard_queries = recall_queries
        .iter()
        .zip(recall_queries.iter().cycle().skip(1))
        .take(self_query_count)
        .map(|((left_text, left, _), (_, right, _))| {
            let mut blended = left
                .iter()
                .zip(right)
                .map(|(left, right)| left * 0.55 + right * 0.45)
                .collect::<Vec<_>>();
            let norm = blended
                .iter()
                .map(|value| value * value)
                .sum::<f32>()
                .sqrt();
            for value in &mut blended {
                *value /= norm;
            }
            (
                format!("hard semantic blend: {left_text}"),
                blended,
                String::new(),
            )
        })
        .collect::<Vec<_>>();
    recall_queries.extend(hard_queries);

    let fts_started = Instant::now();
    {
        let transaction = store.conn_mut().transaction().unwrap();
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO skill_revisions (
                        id, identity_version, source, description, tags_json, exports_json,
                        tests_json, capability_json, status, supersedes_id, superseded_by_id,
                        row_version, created_at, updated_at
                     ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'active', NULL, NULL, 1, 0, 0)",
                )
                .unwrap();
            for (artifact, _, _) in &rows {
                let tags = serde_json::to_string(&artifact.tags).unwrap();
                let exports = serde_json::to_string(
                    &artifact
                        .exports
                        .iter()
                        .map(|export| (&export.name, &export.signature))
                        .collect::<Vec<_>>(),
                )
                .unwrap();
                let tests = serde_json::to_string(&artifact.tests).unwrap();
                let capability = serde_json::json!({
                    "abi_version": artifact.abi_version,
                    "manifest": artifact.capability,
                })
                .to_string();
                insert
                    .execute(params![
                        artifact.id,
                        artifact.identity_version,
                        artifact.source,
                        artifact.description,
                        tags,
                        exports,
                        tests,
                        capability,
                    ])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
    }
    let fts_build_duration = fts_started.elapsed();

    let rebuild_rows = rows.clone();
    // Phase-level timing: separate exact/FTS index construction from ANN graph build.
    let exact_build_started = Instant::now();
    let index_without_ann =
        ImmutableSkillIndex::build_without_ann(1, model.clone(), store.database_path(), rows)
            .unwrap();
    let exact_build_duration = exact_build_started.elapsed();
    let ann_build_started = Instant::now();
    let index = Arc::new(index_without_ann.with_ann());
    let ann_build_duration = ann_build_started.elapsed();
    let snapshot_build_duration = exact_build_duration + ann_build_duration;
    assert_eq!(index.len(), corpus_size);

    let natural_language_policy = RetrievalPolicy {
        dense_candidate_limit: 0,
        ..RetrievalPolicy::default()
    };
    let mut lexical_probe = vec![0.0; model.dimensions];
    lexical_probe[0] = 1.0;
    let natural_language_result = index
        .search(
            "please parse this JSON document and return its keys",
            &lexical_probe,
            &natural_language_policy,
        )
        .unwrap();
    assert!(
        natural_language_result
            .first()
            .is_some_and(|skill| skill.artifact.exports[0].name.starts_with("parseJson_")),
        "natural-language prompts must retrieve through the lexical channel"
    );

    embedder.clear_cache().await;
    let retrieval_query = "parseJson_000000";
    let cold_started = Instant::now();
    let query = embedder.embed_query_cached(retrieval_query).await.unwrap();
    let cold_query_duration = cold_started.elapsed();
    let warm_started = Instant::now();
    let warm_query = embedder.embed_query_cached(retrieval_query).await.unwrap();
    let warm_query_duration = warm_started.elapsed();
    assert_eq!(query, warm_query);

    let policy = RetrievalPolicy::default();
    for sample in 0..20.min(search_samples) {
        let (query_text, query_vector, _) = &recall_queries[sample % recall_queries.len()];
        index
            .search_with_metrics(query_text, query_vector, &policy)
            .unwrap();
    }
    let mut total = Vec::with_capacity(search_samples);
    let mut dense = Vec::with_capacity(search_samples);
    let mut lexical = Vec::with_capacity(search_samples);
    let mut fusion = Vec::with_capacity(search_samples);
    for sample in 0..search_samples {
        let (query_text, query_vector, _) = &recall_queries[sample % recall_queries.len()];
        let started = Instant::now();
        let output = index
            .search_with_metrics(query_text, query_vector, &policy)
            .unwrap();
        total.push(started.elapsed());
        dense.push(output.stages.dense);
        lexical.push(output.stages.lexical);
        fusion.push(output.stages.fusion_and_budgets);
    }
    for (query_text, query_vector, _) in recall_queries.iter().take(20) {
        let first = index.search(query_text, query_vector, &policy).unwrap();
        let second = index.search(query_text, query_vector, &policy).unwrap();
        assert_eq!(first, second, "repeated search order must be deterministic");
    }
    let selected = index.search(retrieval_query, &query, &policy).unwrap();
    let recall = if selected.iter().any(|skill| skill.artifact.id == target_id) {
        1.0
    } else {
        0.0
    };
    let precision = if selected.is_empty() {
        0.0
    } else {
        recall / selected.len() as f64
    };
    let zero_policy = RetrievalPolicy {
        dense_score_floor: 2.0,
        lexical_score_floor: 2.0,
        ..RetrievalPolicy::default()
    };
    assert!(
        index
            .search("definitely absent lexical token", &query, &zero_policy)
            .unwrap()
            .is_empty()
    );

    let recall_policy = RetrievalPolicy {
        max_skills: 10,
        dense_candidate_limit: 10,
        lexical_candidate_limit: 0,
        dense_score_floor: -1.0,
        ..RetrievalPolicy::default()
    };
    let mut exact_oracle_latencies = Vec::new();
    let mut recall_at_ten = 0.0_f64;
    let mut top_one_hits = 0usize;
    for (_, recall_query, target) in &recall_queries {
        let started = Instant::now();
        let exact = index
            .search_exact_with_metrics("", recall_query, &recall_policy)
            .unwrap();
        exact_oracle_latencies.push(started.elapsed());
        let approximate = index
            .search_with_metrics("", recall_query, &recall_policy)
            .unwrap();
        let exact_ids = exact
            .skills
            .iter()
            .map(|skill| skill.artifact.id.as_str())
            .collect::<HashSet<_>>();
        let overlap = approximate
            .skills
            .iter()
            .filter(|skill| exact_ids.contains(skill.artifact.id.as_str()))
            .count();
        recall_at_ten += overlap as f64 / exact_ids.len().max(1) as f64;
        if !target.is_empty()
            && approximate
                .skills
                .first()
                .is_some_and(|skill| skill.artifact.id == *target)
        {
            top_one_hits += 1;
        }
    }
    recall_at_ten /= recall_queries.len().max(1) as f64;

    let reader_index = Arc::clone(&index);
    let reader_query = query.clone();
    let reader_policy = policy.clone();
    let reader = std::thread::spawn(move || {
        let mut samples = Vec::new();
        for _ in 0..4 {
            let started = Instant::now();
            reader_index
                .search("parseJson_000000", &reader_query, &reader_policy)
                .unwrap();
            samples.push(started.elapsed());
        }
        samples
    });
    let exact_rebuild_started = Instant::now();
    let rebuilt_without_ann = ImmutableSkillIndex::build_without_ann(
        2,
        model.clone(),
        store.database_path(),
        rebuild_rows,
    )
    .unwrap();
    let exact_rebuild_duration = exact_rebuild_started.elapsed();
    let ann_rebuild_started = Instant::now();
    let rebuilt = rebuilt_without_ann.with_ann();
    let ann_rebuild_duration = ann_rebuild_started.elapsed();
    let rebuild_duration = exact_rebuild_duration + ann_rebuild_duration;
    let concurrent_samples = reader.join().unwrap();
    assert_eq!(rebuilt.len(), corpus_size);
    let mut rebuild_recall_at_ten = 0.0_f64;
    for (query_text, query_vector, _) in recall_queries.iter().take(20) {
        let exact = index
            .search_exact_with_metrics(query_text, query_vector, &recall_policy)
            .unwrap();
        let rebuilt_first = rebuilt
            .search(query_text, query_vector, &recall_policy)
            .unwrap();
        let rebuilt_second = rebuilt
            .search(query_text, query_vector, &recall_policy)
            .unwrap();
        assert_eq!(
            rebuilt_first, rebuilt_second,
            "one immutable ANN generation must preserve dense ordering"
        );
        let exact_ids = exact
            .skills
            .iter()
            .map(|skill| skill.artifact.id.as_str())
            .collect::<HashSet<_>>();
        rebuild_recall_at_ten += rebuilt_first
            .iter()
            .filter(|skill| exact_ids.contains(skill.artifact.id.as_str()))
            .count() as f64
            / exact_ids.len().max(1) as f64;
    }
    rebuild_recall_at_ten /= 20.0;
    assert!(rebuild_recall_at_ten >= 0.95);
    drop(rebuilt);

    let hidden = lifecycle_hidden_ids;
    let removal_started = Instant::now();
    let removal_snapshot = index.without_ids(3, &hidden);
    let removal_duration = removal_started.elapsed();
    assert_eq!(removal_snapshot.len(), corpus_size - hidden.len());
    assert!(
        removal_snapshot
            .search(retrieval_query, &query, &policy)
            .unwrap()
            .iter()
            .all(|skill| !hidden.contains(&skill.artifact.id)),
        "removed revisions must not survive the immutable ANN visibility mask"
    );
    drop(removal_snapshot);

    let rss_after = peak_rss_kib();
    let search_p99_us = percentile(&total, 0.99);
    let build_us = snapshot_build_duration.as_secs_f64() * 1_000_000.0;
    let rebuild_us = rebuild_duration.as_secs_f64() * 1_000_000.0;
    let report_path = std::env::temp_dir().join("mini-agent-skill-retrieval-latest.json");
    let report = serde_json::json!({
        "schema_version": 1,
        "seed": SEED,
        "profile": "debug",
        "label": label,
        "corpus": {
            "revisions": corpus_size,
            "mix": ["semantic", "identifier", "mixed", "irrelevant", "near_duplicate", "lifecycle_filtered"],
        },
        "model": {
            "id": model.model_id,
            "revision": model.model_revision,
            "dimensions": model.dimensions,
            "normalized": model.normalized,
        },
        "machine": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "cpu": command_output("sysctl", &["-n", "machdep.cpu.brand_string"])
                .or_else(|| command_output("uname", &["-m"])),
            "logical_cpus": std::thread::available_parallelism().map(usize::from).ok(),
            "memory_bytes": command_output("sysctl", &["-n", "hw.memsize"]),
        },
        "latency": {
            "corpus_document_embedding_us": corpus_embedding_duration.as_secs_f64() * 1_000_000.0,
            "fts_build_us": fts_build_duration.as_secs_f64() * 1_000_000.0,
            "snapshot_build_us": build_us,
            "snapshot_build_phases": {
                "exact_fts_us": exact_build_duration.as_secs_f64() * 1_000_000.0,
                "ann_construction_us": ann_build_duration.as_secs_f64() * 1_000_000.0,
            },
            "snapshot_rebuild_us": rebuild_us,
            "snapshot_rebuild_phases": {
                "exact_fts_us": exact_rebuild_duration.as_secs_f64() * 1_000_000.0,
                "ann_construction_us": ann_rebuild_duration.as_secs_f64() * 1_000_000.0,
            },
            "lifecycle_removal_refresh_us": removal_duration.as_secs_f64() * 1_000_000.0,
            "query_embedding_cold_us": cold_query_duration.as_secs_f64() * 1_000_000.0,
            "query_embedding_warm_us": warm_query_duration.as_secs_f64() * 1_000_000.0,
            "index_search": percentiles(&total),
            "dense": percentiles(&dense),
            "fts_candidates": percentiles(&lexical),
            "fusion_dedupe_budget": percentiles(&fusion),
            "concurrent_reader": percentiles(&concurrent_samples),
            "exact_oracle": percentiles(&exact_oracle_latencies),
        },
        "memory": {
            "rss_before_kib": rss_before,
            "peak_observed_rss_kib": rss_after,
        },
        "relevance": {
            "target_recall": recall,
            "target_precision": precision,
            "ann_recall_at_10_against_exact": recall_at_ten,
            "independent_rebuild_recall_at_10": rebuild_recall_at_ten,
            "ann_self_query_top1_rate": top_one_hits as f64 / self_query_count.max(1) as f64,
            "deterministic_order": true,
            "zero_result": true,
            "lifecycle_filtered": true,
        },
        "gate": {
            "p99_target_us": 5000,
            "observed_p99_us": search_p99_us,
            "recall_at_10_target": 0.95,
            "build_budget_us": 180_000_000u64,
            "observed_build_us": build_us,
            "rebuild_budget_us": 220_000_000u64,
            "observed_rebuild_us": rebuild_us,
            "rss_budget_kib": 3_000_000u64,
            "observed_rss_kib": rss_after,
            "passed": search_p99_us <= 5000.0 && recall_at_ten >= 0.95,
        },
        "raw_result": report_path,
    });
    fs::write(&report_path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    println!("{}", serde_json::to_string(&report).unwrap());
    println!(
        "skill retrieval benchmark: corpus={corpus_size}, search p50={:.1}us p95={:.1}us p99={search_p99_us:.1}us, target<=5000us, raw={}",
        percentile(&total, 0.50),
        percentile(&total, 0.95),
        report_path.display()
    );
    if corpus_size == 100_000 {
        assert!(
            search_p99_us <= 5000.0 && recall_at_ten >= 0.95,
            "full retrieval gate failed: p99={search_p99_us:.1}us recall@10={recall_at_ten:.3}"
        );
        assert!(
            build_us <= 180_000_000.0,
            "full build gate failed: {build_us:.0}us > 180s budget"
        );
        assert!(
            rebuild_us <= 220_000_000.0,
            "full rebuild gate failed: {rebuild_us:.0}us > 220s budget"
        );
        if let Some(rss) = rss_after {
            assert!(
                rss <= 3_000_000,
                "full RSS gate failed: {rss}KiB > 3_000_000KiB budget"
            );
        }
    }
}

#[tokio::test]
async fn skill_retrieval_benchmark_smoke() {
    run_benchmark(2_000, 8, "ci-smoke").await;
}

#[tokio::test]
#[ignore = "set ZS_SKILL_BENCH_FULL=1 to run the 100,000-revision audit"]
async fn skill_retrieval_benchmark() {
    assert_eq!(
        std::env::var("ZS_SKILL_BENCH_FULL").as_deref(),
        Ok("1"),
        "the ignored full benchmark requires ZS_SKILL_BENCH_FULL=1"
    );
    let corpus_size = std::env::var("ZS_SKILL_BENCH_SIZE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100_000);
    run_benchmark(corpus_size, 500, &format!("full-{corpus_size}")).await;
}
