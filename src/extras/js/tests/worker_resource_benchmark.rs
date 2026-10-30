use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extras::js::protocol::{
    EffectOperation, EffectRequest, EffectResult, GrantId, RunStep, StepOutcome,
};
use crate::extras::js::supervisor::{
    EffectFuture, InvocationEffectHandler, JsWorkerSupervisor, WorkerError, WorkerReusePolicy,
};
#[cfg(target_os = "linux")]
use crate::extras::js::supervisor::{WORKER_EXIT_OBSERVER, test_worker_exit_class};
use crate::extras::js::types::PermCancellation;
use crate::sandbox::worker::{
    BenchmarkWorkerLauncher, TestWorkerLauncher, WorkerContainmentAssurance,
    WorkerContainmentStatus, WorkerLauncher,
};

const WARMUPS: usize = 10;
const SAMPLES: usize = 100;
/// Bounded resamples for a single benchmark iteration whose native worker died in a way the
/// cgroup evidence corroborates as host memory pressure rather than a containment regression.
/// mini-agent-hdl3i: hosted Linux runners intermittently SIGKILL the worker under memory
/// pressure during `linux-sandbox-policy`; retrying the one affected sample keeps a transient
/// host kill from discarding the whole benchmark run while any other kill still fails the test.
#[cfg(target_os = "linux")]
const HOST_RESOURCE_RETRY_ATTEMPTS: u32 = 2;
const IPC_PAYLOAD_BYTES: usize = 4 * 1024;
const DOCUMENTED_P95_VARIANCE_RATIO: f64 = 0.15;
const LINUX_COLD_READY_TARGET_US: f64 = 250_000.0;
/// Reviewed macOS exception, recorded in
/// `docs/benchmarks/2026-09-08-macos-worker-cold-start.md`.
///
/// The publisher independently hashes source and image; Ready-time unlink
/// revalidates the image digest. Process startup overlaps the initial sealed
/// image proof. The original profile and the matched-host overlap experiment
/// are recorded in docs/benchmarks; timing targets remain informational.
const MACOS_COLD_READY_TARGET_US: f64 = 1_000_000.0;
const WINDOWS_COLD_READY_TARGET_US: f64 = 750_000.0;
const WARM_PURE_CALL_TARGET_US: f64 = 10_000.0;
const BROKER_IPC_TARGET_US: f64 = 10_000.0;
const IDLE_PRIVATE_TARGET_BYTES: u64 = 32 * 1024 * 1024;
/// Cancel-and-recover replaces the worker and carries the same fresh-launch
/// cost plus teardown. Keep this single cross-platform target and report the
/// measured outcome without widening it to hide a platform overrun.
const POST_CANCEL_RECOVERY_TARGET_US: f64 = 1_000_000.0;
const IDLE_RUNTIME_OBSERVATION_KIND: &str = "protocol_lifecycle_proof";
const IDLE_RUNTIME_PROOF: &str = "authenticated StepResult is emitted only after execute_fresh_step returns and drops its request-local QuickJS Runtime";
const LINUX_PROCESS_OBSERVATION_KIND: &str = "linux_proc_exact_executable_tree";
const LINUX_PROCESS_PROOF: &str = "bounded /proc descendant traversal after authenticated Ready; exact worker matched by the configured installed debug executable device and inode; helpers counted separately";
const MACOS_PROCESS_OBSERVATION_KIND: &str = "macos_guardian_process_group";
const MACOS_PROCESS_PROOF: &str = "bounded libproc enumeration after authenticated Ready; the authenticated guardian owns its dedicated process group; exactly one live non-guardian member is the worker and the guardian is counted separately";
const WINDOWS_PROCESS_OBSERVATION_KIND: &str = "windows_owned_job_direct_process_proof";
const WINDOWS_PROCESS_PROOF: &str = "direct CreateProcessW application PID from WorkerChild::contained; active process count queried from the owned creation-time Job; Job handle is not a helper process";
const MAX_CONTAINMENT_TREE_PROCESSES: usize = 64;
const LINUX_MEMORY_MEASUREMENT: &str =
    "/proc/<pid>/smaps_rollup Private_Clean + Private_Dirty + Private_Hugetlb";
const MACOS_MEMORY_MEASUREMENT: &str = "vmmap -summary Physical footprint";
const WINDOWS_MEMORY_MEASUREMENT: &str = "Get-Process PrivateMemorySize64";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BenchmarkReport {
    schema_version: u32,
    benchmark: String,
    evidence_state: String,
    profile: String,
    sampling: SamplingPolicy,
    targets: Targets,
    security_ceilings: SecurityCeilings,
    platform_evidence: Vec<PlatformEvidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum PlatformEvidence {
    Measured {
        run: Box<BenchmarkRun>,
    },
    ContainmentUnavailable {
        machine: Box<Machine>,
        containment: Box<Containment>,
        reason_code: String,
    },
}

impl PlatformEvidence {
    fn operating_system(&self) -> &str {
        match self {
            Self::Measured { run } => &run.machine.os,
            Self::ContainmentUnavailable { machine, .. } => &machine.os,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SamplingPolicy {
    warmups: usize,
    samples: usize,
    percentile: String,
    variance: String,
    documented_p95_variance_ratio: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Targets {
    cold_ready_us: BTreeMap<String, f64>,
    warm_pure_call_us: f64,
    broker_ipc_4kib_us: f64,
    idle_private_bytes: u64,
    post_cancel_recovery_us: f64,
    maximum_worker_processes: u32,
    idle_runtimes: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SecurityCeilings {
    native_memory_bytes: u64,
    native_cpu_seconds: u64,
    relationship_to_targets: String,
    verification: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BenchmarkRun {
    machine: Machine,
    containment: Containment,
    latency: LatencyMeasurements,
    idle_private_memory: MemoryMeasurement,
    counts: CountMeasurements,
    target_results: TargetResults,
    comparison: Option<RunComparison>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Machine {
    host: String,
    os: String,
    arch: String,
    kernel: String,
    cpu: String,
    logical_cpus: usize,
    memory_bytes: u64,
    binary_profile: String,
    package_version: String,
    build_identity: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Containment {
    backend: String,
    assurance: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LatencyMeasurements {
    cold_ready: Statistics,
    warm_pure_call: Statistics,
    broker_ipc_4kib: Statistics,
    post_cancel_recovery: Statistics,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Statistics {
    samples: usize,
    mean_us: f64,
    p50_us: f64,
    p95_us: f64,
    variance_us2: f64,
    standard_deviation_us: f64,
    minimum_us: f64,
    maximum_us: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct MemoryMeasurement {
    samples: usize,
    mean_bytes: f64,
    maximum_bytes: u64,
    measurement: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CountMeasurements {
    maximum_observed_worker_processes: u32,
    maximum_observed_containment_helper_processes: u32,
    idle_worker_processes: u32,
    worker_process_observation_kind: String,
    worker_process_observation: String,
    idle_runtimes: u32,
    idle_runtime_observation_kind: String,
    idle_runtime_observation: String,
}

#[derive(Clone, Copy, Debug)]
struct ProcessObservation {
    exact_worker_pid: u32,
    worker_processes: u32,
    containment_helper_processes: u32,
    observation_kind: &'static str,
    observation: &'static str,
}

#[derive(Default)]
struct ProcessCountAccumulator {
    maximum_worker_processes: u32,
    maximum_containment_helper_processes: u32,
    last_idle_worker_processes: u32,
    observation_kind: Option<&'static str>,
    observation: Option<&'static str>,
}

impl ProcessCountAccumulator {
    fn observe(&mut self, observation: ProcessObservation) {
        if let Some(kind) = self.observation_kind {
            assert_eq!(kind, observation.observation_kind);
            assert_eq!(self.observation, Some(observation.observation));
        } else {
            self.observation_kind = Some(observation.observation_kind);
            self.observation = Some(observation.observation);
        }
        self.maximum_worker_processes = self
            .maximum_worker_processes
            .max(observation.worker_processes);
        self.maximum_containment_helper_processes = self
            .maximum_containment_helper_processes
            .max(observation.containment_helper_processes);
        self.last_idle_worker_processes = observation.worker_processes;
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TargetResults {
    cold_ready: bool,
    warm_pure_call: bool,
    broker_ipc_4kib: bool,
    idle_private_memory: bool,
    post_cancel_recovery: bool,
    one_worker_zero_idle_runtimes: bool,
    timing_targets_are_informational: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RunComparison {
    reference_machine: Machine,
    reference_containment: Containment,
    previous_build_identity: String,
    metrics: BTreeMap<String, StatisticsComparison>,
    all_within_documented_variance: bool,
    informational_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StatisticsComparison {
    previous_p95_us: f64,
    current_p95_us: f64,
    p95_relative_delta: f64,
    allowed_relative_delta: f64,
    within_documented_variance: bool,
}

#[derive(Clone, Default)]
struct PayloadEffects;

impl InvocationEffectHandler for PayloadEffects {
    fn handle_effect(
        &mut self,
        request: EffectRequest,
        _cancellation: PermCancellation,
    ) -> EffectFuture<'_> {
        Box::pin(async move {
            match request.operation {
                EffectOperation::ReadFile { .. } => EffectResult::ReadFile {
                    content: "x".repeat(IPC_PAYLOAD_BYTES),
                },
                _ => panic!("benchmark requested an unexpected effect operation"),
            }
        })
    }
}

#[derive(Clone, Default)]
struct CancellationProbeEffects {
    worker_requests: Arc<AtomicUsize>,
}

impl InvocationEffectHandler for CancellationProbeEffects {
    fn handle_effect(
        &mut self,
        request: EffectRequest,
        _cancellation: PermCancellation,
    ) -> EffectFuture<'_> {
        assert!(
            matches!(request.operation, EffectOperation::ReadFile { .. }),
            "cancellation probe requested an unexpected effect operation"
        );
        self.worker_requests.fetch_add(1, Ordering::Release);
        Box::pin(async {
            EffectResult::ReadFile {
                content: String::new(),
            }
        })
    }
}

fn summarize_microseconds(samples: &[f64]) -> Statistics {
    assert!(!samples.is_empty(), "statistics need at least one sample");
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
    let variance = if sorted.len() > 1 {
        sorted
            .iter()
            .map(|sample| (sample - mean).powi(2))
            .sum::<f64>()
            / (sorted.len() - 1) as f64
    } else {
        0.0
    };
    Statistics {
        samples: sorted.len(),
        mean_us: mean,
        p50_us: nearest_rank(&sorted, 0.50),
        p95_us: nearest_rank(&sorted, 0.95),
        variance_us2: variance,
        standard_deviation_us: variance.sqrt(),
        minimum_us: sorted[0],
        maximum_us: *sorted.last().unwrap(),
    }
}

fn nearest_rank(sorted: &[f64], percentile: f64) -> f64 {
    let rank = (percentile * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn compare_statistics(previous: &Statistics, current: &Statistics) -> StatisticsComparison {
    let relative = if previous.p95_us == 0.0 {
        if current.p95_us == 0.0 { 0.0 } else { 1.0 }
    } else {
        (current.p95_us - previous.p95_us).abs() / previous.p95_us
    };
    StatisticsComparison {
        previous_p95_us: previous.p95_us,
        current_p95_us: current.p95_us,
        p95_relative_delta: relative,
        allowed_relative_delta: DOCUMENTED_P95_VARIANCE_RATIO,
        within_documented_variance: relative <= DOCUMENTED_P95_VARIANCE_RATIO,
    }
}

fn validate_statistics(statistics: &Statistics) -> Result<(), String> {
    let values = [
        statistics.mean_us,
        statistics.p50_us,
        statistics.p95_us,
        statistics.variance_us2,
        statistics.standard_deviation_us,
        statistics.minimum_us,
        statistics.maximum_us,
    ];
    if statistics.samples != SAMPLES
        || values
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
        || statistics.minimum_us > statistics.p50_us
        || statistics.p50_us > statistics.p95_us
        || statistics.p95_us > statistics.maximum_us
        || statistics.mean_us < statistics.minimum_us
        || statistics.mean_us > statistics.maximum_us
    {
        return Err("latency statistics are malformed or use a non-canonical sample count".into());
    }
    let expected_deviation = statistics.variance_us2.sqrt();
    let deviation_tolerance = f64::EPSILON * expected_deviation.abs().max(1.0) * 8.0;
    if (statistics.standard_deviation_us - expected_deviation).abs() > deviation_tolerance {
        return Err("latency variance and standard deviation are inconsistent".into());
    }
    let range = statistics.maximum_us - statistics.minimum_us;
    let sample_factor = statistics.samples as f64 / (statistics.samples - 1) as f64;
    let maximum_variance = sample_factor * range.powi(2) / 4.0;
    let variance_tolerance = f64::EPSILON * maximum_variance.abs().max(1.0) * 8.0;
    if statistics.variance_us2 > maximum_variance + variance_tolerance {
        return Err("latency variance exceeds the unbiased-sample range bound".into());
    }
    Ok(())
}

fn validate_memory_measurement(os: &str, memory: &MemoryMeasurement) -> Result<(), String> {
    let expected_method = match os {
        "linux" => LINUX_MEMORY_MEASUREMENT,
        "macos" => MACOS_MEMORY_MEASUREMENT,
        "windows" => WINDOWS_MEMORY_MEASUREMENT,
        _ => return Err("operating system has no reviewed private-memory method".into()),
    };
    if memory.samples != SAMPLES
        || !memory.mean_bytes.is_finite()
        || memory.mean_bytes <= 0.0
        || memory.maximum_bytes == 0
        || memory.mean_bytes > memory.maximum_bytes as f64
        || memory.measurement != expected_method
    {
        return Err("private-memory evidence is malformed or uses the wrong method".into());
    }
    Ok(())
}

fn windows_kernel_identity_is_specific(identity: &str) -> bool {
    let Some((version_identity, build)) = identity.rsplit_once(" build ") else {
        return false;
    };
    !version_identity.is_empty()
        && version_identity
            .chars()
            .any(|character| character.is_ascii_digit())
        && !build.is_empty()
        && build.chars().all(|character| character.is_ascii_digit())
}

fn validate_report(value: &Value) -> Result<(), String> {
    let report: BenchmarkReport = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid benchmark schema: {error}"))?;
    if report.schema_version != 1 || report.benchmark != "mini-agent-js-worker" {
        return Err("unexpected benchmark identity or schema version".into());
    }
    if report.profile != "debug"
        || report.sampling.warmups != WARMUPS
        || report.sampling.samples != SAMPLES
        || report.sampling.percentile != "nearest-rank"
        || report.sampling.variance != "unbiased sample variance in microseconds squared"
        || report.sampling.documented_p95_variance_ratio != DOCUMENTED_P95_VARIANCE_RATIO
    {
        return Err("sampling policy drifted from the reviewed benchmark method".into());
    }
    if report.security_ceilings.native_memory_bytes != 256 * 1024 * 1024
        || report.security_ceilings.native_cpu_seconds != 35
    {
        return Err("security ceilings drifted from the containment contract".into());
    }
    if report.targets.cold_ready_us
        != BTreeMap::from([
            ("linux".into(), LINUX_COLD_READY_TARGET_US),
            ("macos".into(), MACOS_COLD_READY_TARGET_US),
            ("windows".into(), WINDOWS_COLD_READY_TARGET_US),
        ])
        || report.targets.warm_pure_call_us != WARM_PURE_CALL_TARGET_US
        || report.targets.broker_ipc_4kib_us != BROKER_IPC_TARGET_US
        || report.targets.idle_private_bytes != IDLE_PRIVATE_TARGET_BYTES
        || report.targets.post_cancel_recovery_us != POST_CANCEL_RECOVERY_TARGET_US
        || report.targets.maximum_worker_processes != 1
        || report.targets.idle_runtimes != 0
    {
        return Err("performance target constants drifted from the reviewed benchmark".into());
    }
    match report.platform_evidence.len() {
        0 if report.evidence_state == "pending_external_runs" => {}
        1 if report.evidence_state == "single_platform_record" => {}
        3 if report.evidence_state == "complete_for_recorded_platforms" => {}
        _ => return Err("evidence state does not match the number of platform records".into()),
    }
    let mut operating_systems = report
        .platform_evidence
        .iter()
        .map(PlatformEvidence::operating_system)
        .collect::<Vec<_>>();
    if operating_systems
        .iter()
        .any(|os| !matches!(*os, "linux" | "macos" | "windows"))
    {
        return Err("record uses an unsupported operating system".into());
    }
    operating_systems.sort_unstable();
    operating_systems.dedup();
    if operating_systems.len() != report.platform_evidence.len() {
        return Err("report contains duplicate operating-system evidence".into());
    }
    for evidence in &report.platform_evidence {
        let machine = match evidence {
            PlatformEvidence::Measured { run } => &run.machine,
            PlatformEvidence::ContainmentUnavailable { machine, .. } => machine.as_ref(),
        };
        if machine.binary_profile != "debug"
            || machine.package_version != env!("CARGO_PKG_VERSION")
            || machine.build_identity.is_empty()
            || machine.arch.is_empty()
            || machine.kernel.is_empty()
        {
            return Err("platform record lacks canonical debug-build machine metadata".into());
        }
        let containment = match evidence {
            PlatformEvidence::Measured { run } => &run.containment,
            PlatformEvidence::ContainmentUnavailable { containment, .. } => containment.as_ref(),
        };
        if !containment_matches_platform(evidence.operating_system(), containment) {
            return Err("platform evidence contains non-canonical containment metadata".into());
        }
        let PlatformEvidence::Measured { run } = evidence else {
            let PlatformEvidence::ContainmentUnavailable {
                reason_code,
                containment,
                ..
            } = evidence
            else {
                unreachable!();
            };
            if reason_code != "containment_unavailable"
                || !matches!(
                    containment.assurance.as_str(),
                    "enforced" | "deprecated-best-effort"
                )
            {
                return Err("unavailable evidence must use closed status metadata".into());
            }
            continue;
        };
        if run.machine.logical_cpus == 0
            || run.machine.memory_bytes == 0
            || run.machine.cpu == "unknown"
            || run.machine.host == "unknown"
            || run.machine.kernel == "unknown"
            || (run.machine.os == "windows"
                && !windows_kernel_identity_is_specific(&run.machine.kernel))
        {
            return Err("measured run lacks complete reference-machine metadata".into());
        }
        if !matches!(run.machine.os.as_str(), "linux" | "macos" | "windows") {
            return Err("run uses an unsupported operating system".into());
        }
        for statistics in [
            &run.latency.cold_ready,
            &run.latency.warm_pure_call,
            &run.latency.broker_ipc_4kib,
            &run.latency.post_cancel_recovery,
        ] {
            validate_statistics(statistics)?;
        }
        validate_memory_measurement(&run.machine.os, &run.idle_private_memory)?;
        if run.counts.maximum_observed_worker_processes != 1
            || run.counts.idle_worker_processes != 1
            || run.counts.idle_runtimes != 0
        {
            return Err("run does not prove the resource-count invariants".into());
        }
        if run.counts.idle_runtime_observation_kind != IDLE_RUNTIME_OBSERVATION_KIND
            || run.counts.idle_runtime_observation != IDLE_RUNTIME_PROOF
        {
            return Err("idle-runtime claim lacks the exact lifecycle proof".into());
        }
        if !process_proof_matches(&run.machine.os, &run.counts) {
            return Err("worker-process claim lacks its platform observation proof".into());
        }
        if !helper_count_is_sane(&run.machine.os, &run.counts) {
            return Err("containment-helper count contradicts its observation method".into());
        }
        let derived_count_target = run.counts.maximum_observed_worker_processes == 1
            && run.counts.idle_worker_processes == 1
            && run.counts.idle_runtimes == 0;
        if run.target_results.one_worker_zero_idle_runtimes != derived_count_target {
            return Err("process/runtime target result is not derived from its evidence".into());
        }
        let expected_targets = TargetResults {
            cold_ready: run.latency.cold_ready.p95_us
                <= report.targets.cold_ready_us[run.machine.os.as_str()],
            warm_pure_call: run.latency.warm_pure_call.p95_us <= report.targets.warm_pure_call_us,
            broker_ipc_4kib: run.latency.broker_ipc_4kib.p95_us
                <= report.targets.broker_ipc_4kib_us,
            idle_private_memory: run.idle_private_memory.maximum_bytes
                <= report.targets.idle_private_bytes,
            post_cancel_recovery: run.latency.post_cancel_recovery.p95_us
                <= report.targets.post_cancel_recovery_us,
            one_worker_zero_idle_runtimes: derived_count_target,
            timing_targets_are_informational: true,
        };
        if run.target_results.cold_ready != expected_targets.cold_ready
            || run.target_results.warm_pure_call != expected_targets.warm_pure_call
            || run.target_results.broker_ipc_4kib != expected_targets.broker_ipc_4kib
            || run.target_results.idle_private_memory != expected_targets.idle_private_memory
            || run.target_results.post_cancel_recovery != expected_targets.post_cancel_recovery
            || run.target_results.one_worker_zero_idle_runtimes
                != expected_targets.one_worker_zero_idle_runtimes
        {
            return Err("target results are not derived from their recorded measurements".into());
        }
        if !run.target_results.timing_targets_are_informational {
            return Err("shared-runner timing results must remain informational".into());
        }
        match &run.comparison {
            Some(comparison) => validate_comparison(run, comparison)?,
            None if report.evidence_state == "complete_for_recorded_platforms" => {
                return Err(
                    "aggregated measured evidence requires a repeatability comparison".into(),
                );
            }
            None => {}
        }
    }
    Ok(())
}

fn process_proof_matches(os: &str, counts: &CountMeasurements) -> bool {
    match os {
        "linux" => {
            counts.worker_process_observation_kind == LINUX_PROCESS_OBSERVATION_KIND
                && counts.worker_process_observation == LINUX_PROCESS_PROOF
        }
        "macos" => {
            counts.worker_process_observation_kind == MACOS_PROCESS_OBSERVATION_KIND
                && counts.worker_process_observation == MACOS_PROCESS_PROOF
        }
        "windows" => {
            counts.worker_process_observation_kind == WINDOWS_PROCESS_OBSERVATION_KIND
                && counts.worker_process_observation == WINDOWS_PROCESS_PROOF
        }
        _ => false,
    }
}

fn helper_count_is_sane(os: &str, counts: &CountMeasurements) -> bool {
    match os {
        "linux" => {
            counts.maximum_observed_containment_helper_processes
                < MAX_CONTAINMENT_TREE_PROCESSES as u32
        }
        "macos" => counts.maximum_observed_containment_helper_processes == 1,
        "windows" => counts.maximum_observed_containment_helper_processes == 0,
        _ => false,
    }
}

fn validate_comparison(run: &BenchmarkRun, comparison: &RunComparison) -> Result<(), String> {
    let current_p95 = BTreeMap::from([
        ("broker_ipc_4kib", run.latency.broker_ipc_4kib.p95_us),
        ("cold_ready", run.latency.cold_ready.p95_us),
        (
            "post_cancel_recovery",
            run.latency.post_cancel_recovery.p95_us,
        ),
        ("warm_pure_call", run.latency.warm_pure_call.p95_us),
    ]);
    if comparison.reference_machine != run.machine
        || comparison.reference_containment != run.containment
        || comparison.previous_build_identity != comparison.reference_machine.build_identity
        || !comparison.informational_only
        || comparison
            .metrics
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            != current_p95.keys().copied().collect::<Vec<_>>()
    {
        return Err("repeatability comparison metadata or metric set is invalid".into());
    }
    for (name, metric) in &comparison.metrics {
        let expected_current = current_p95[name.as_str()];
        let expected_delta = if metric.previous_p95_us == 0.0 {
            if metric.current_p95_us == 0.0 {
                0.0
            } else {
                1.0
            }
        } else {
            (metric.current_p95_us - metric.previous_p95_us).abs() / metric.previous_p95_us
        };
        if !metric.previous_p95_us.is_finite() || metric.previous_p95_us < 0.0 {
            return Err(format!(
                "repeatability comparison for {name} has an invalid prior p95"
            ));
        }
        if metric.current_p95_us != expected_current {
            return Err(format!(
                "repeatability comparison for {name} does not match the recorded current p95"
            ));
        }
        if !derived_float_matches(metric.p95_relative_delta, expected_delta) {
            return Err(format!(
                "repeatability comparison for {name} is not derived from its p95 values"
            ));
        }
        if metric.allowed_relative_delta != DOCUMENTED_P95_VARIANCE_RATIO {
            return Err(format!(
                "repeatability comparison for {name} uses the wrong variance policy"
            ));
        }
        if metric.within_documented_variance != (expected_delta <= DOCUMENTED_P95_VARIANCE_RATIO) {
            return Err(format!(
                "repeatability comparison for {name} has an invalid variance verdict"
            ));
        }
    }
    if comparison.all_within_documented_variance
        != comparison
            .metrics
            .values()
            .all(|metric| metric.within_documented_variance)
    {
        return Err("aggregate repeatability verdict is not derived from metric verdicts".into());
    }
    Ok(())
}

fn derived_float_matches(recorded: f64, derived: f64) -> bool {
    if recorded == derived {
        return true;
    }
    let scale = recorded.abs().max(derived.abs()).max(1.0);
    (recorded - derived).abs() <= 8.0 * f64::EPSILON * scale
}

fn containment_matches_platform(os: &str, containment: &Containment) -> bool {
    matches!(
        (
            os,
            containment.backend.as_str(),
            containment.assurance.as_str()
        ),
        ("linux", "bubblewrap", "enforced")
            | ("macos", "seatbelt", "deprecated-best-effort")
            | ("windows", "windows-lpac", "enforced")
    )
}

fn aggregate_reports(reports: Vec<BenchmarkReport>) -> Result<BenchmarkReport, String> {
    let mut aggregate = benchmark_report_template();
    for report in reports {
        validate_report(&serde_json::to_value(&report).map_err(|error| error.to_string())?)?;
        if report.platform_evidence.len() != 1 {
            return Err("each CI artifact must contain exactly one platform record".into());
        }
        aggregate.platform_evidence.extend(report.platform_evidence);
    }
    aggregate
        .platform_evidence
        .sort_by_key(|evidence| match evidence.operating_system() {
            "linux" => 0,
            "macos" => 1,
            "windows" => 2,
            _ => 3,
        });
    let operating_systems = aggregate
        .platform_evidence
        .iter()
        .map(PlatformEvidence::operating_system)
        .collect::<Vec<_>>();
    if operating_systems != ["linux", "macos", "windows"] {
        return Err("aggregate requires exactly one Linux, macOS, and Windows run".into());
    }
    aggregate.evidence_state = "complete_for_recorded_platforms".into();
    Ok(aggregate)
}

fn elapsed_us(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000_000.0
}

fn benchmark_report_template() -> BenchmarkReport {
    BenchmarkReport {
        schema_version: 1,
        benchmark: "mini-agent-js-worker".into(),
        evidence_state: "pending_external_runs".into(),
        profile: "debug".into(),
        sampling: SamplingPolicy {
            warmups: WARMUPS,
            samples: SAMPLES,
            percentile: "nearest-rank".into(),
            variance: "unbiased sample variance in microseconds squared".into(),
            documented_p95_variance_ratio: DOCUMENTED_P95_VARIANCE_RATIO,
        },
        targets: Targets {
            cold_ready_us: BTreeMap::from([
                ("linux".into(), LINUX_COLD_READY_TARGET_US),
                ("macos".into(), MACOS_COLD_READY_TARGET_US),
                ("windows".into(), WINDOWS_COLD_READY_TARGET_US),
            ]),
            warm_pure_call_us: WARM_PURE_CALL_TARGET_US,
            broker_ipc_4kib_us: BROKER_IPC_TARGET_US,
            idle_private_bytes: IDLE_PRIVATE_TARGET_BYTES,
            post_cancel_recovery_us: POST_CANCEL_RECOVERY_TARGET_US,
            maximum_worker_processes: 1,
            idle_runtimes: 0,
        },
        security_ceilings: SecurityCeilings {
            native_memory_bytes: 256 * 1024 * 1024,
            native_cpu_seconds: 35,
            relationship_to_targets:
                "enforced security ceilings; never inferred from benchmark measurements".into(),
            verification:
                "verified by the platform containment probe job, separately from this benchmark"
                    .into(),
        },
        platform_evidence: Vec::new(),
    }
}

fn production_supervisor(launcher: BenchmarkWorkerLauncher) -> Arc<JsWorkerSupervisor> {
    Arc::new(JsWorkerSupervisor::with_production_launcher_for_benchmark(
        launcher,
    ))
}

/// Matches `extras::js::supervisor::MAX_PROCESS_AGE` (the production reuse policy's age
/// ceiling). Only the invocation ceiling below needs tightening for this benchmark.
const BENCHMARK_WORKER_REUSE_MAX_AGE: Duration = Duration::from_secs(15 * 60);
/// mini-agent-hdl3i: the warm/ipc/memory/post-cancel-recovery phases share ONE persistent
/// worker across up to `WARMUPS + SAMPLES` calls each, by design: steady-state latency must
/// reflect a reused production worker, not a fresh process every call. Production's default
/// reuse ceiling (`WorkerReusePolicy::default()`, 256 invocations / 15 minutes) is sized for
/// real traffic. A debug build issuing hundreds of back-to-back trivial calls on a slow or
/// contended CI runner can spend far more native CPU per call than that assumption expects, so
/// the shared worker could accumulate close to its full enforced `RLIMIT_CPU` ceiling (35
/// seconds; see `src/sandbox/worker/linux.rs`) before the production ceiling would ever retire
/// it, producing an uncorroborated SIGKILL that `classify_worker_kill` correctly cannot call
/// host resource exhaustion (CI run 39639586216 and 39689562062 on mini-agent-hdl3i). Recycling
/// far more often than production keeps every single worker's own accumulated CPU time well
/// below the ceiling even under much worse host slowdown than observed. Recycling is also
/// already-tested production machinery (`WorkerReusePolicy`/`invoke_with_state`); this only
/// tightens the invocation count it recycles on for THIS benchmark's own persistent supervisor.
/// A few of the ~220 warm/ipc samples land right after a recycle and measure a relaunch instead
/// of steady-state latency; those phases are already documented as informational-only (see "without
/// gating noisy timing percentiles" in the CI job name), so a handful of inflated samples is an
/// accepted trade-off, exactly like the existing host-resource-retry resampling below.
const BENCHMARK_WORKER_MAX_INVOCATIONS: u64 = 25;

/// Like [`production_supervisor`], but recycles its worker far more often than the production
/// default so no single worker process can approach the enforced native CPU ceiling. Used only
/// for the persistent supervisor shared across the warm/ipc/memory/recovery phases; the cold
/// loop's per-iteration supervisors already discard their worker after one invocation.
fn production_supervisor_with_bounded_cpu_budget(
    launcher: BenchmarkWorkerLauncher,
) -> Arc<JsWorkerSupervisor> {
    Arc::new(
        JsWorkerSupervisor::with_production_launcher_and_policy_for_benchmark(
            launcher,
            WorkerReusePolicy::new(
                BENCHMARK_WORKER_REUSE_MAX_AGE,
                BENCHMARK_WORKER_MAX_INVOCATIONS,
            ),
        ),
    )
}

fn benchmark_executable() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let configured = std::env::var_os("MINI_AGENT_JS_WORKER_BENCH_EXE").ok_or(
        "set MINI_AGENT_JS_WORKER_BENCH_EXE to the cargo-installed debug mini-agent binary",
    )?;
    let executable = PathBuf::from(configured).canonicalize()?;
    if !executable.is_file() {
        return Err("MINI_AGENT_JS_WORKER_BENCH_EXE is not a regular file".into());
    }
    let test_executable = std::env::current_exe()?.canonicalize()?;
    if executable == test_executable {
        return Err("resource benchmark refuses to measure its libtest executable".into());
    }
    let expected_name = if cfg!(windows) {
        "mini-agent.exe"
    } else {
        "mini-agent"
    };
    if executable.file_name().and_then(|name| name.to_str()) != Some(expected_name) {
        return Err(
            "MINI_AGENT_JS_WORKER_BENCH_EXE must name the installed mini-agent binary".into(),
        );
    }
    Ok(executable)
}

fn assurance_label(assurance: WorkerContainmentAssurance) -> String {
    match assurance {
        WorkerContainmentAssurance::Enforced => "enforced",
        WorkerContainmentAssurance::DeprecatedBestEffort => "deprecated-best-effort",
    }
    .into()
}

/// Host memory-pressure counters read from this process's own cgroup v2 `memory.events`.
/// `oom_kill` increases once per process the kernel's out-of-memory killer actually terminated
/// inside this cgroup; `oom` increases once per out-of-memory event the cgroup handled by any
/// means. The benchmark's native worker is a plain child process launched from this same test
/// binary: `bwrap --unshare-cgroup[-try]` only hides the cgroup path from the sandboxed worker's
/// own view, it does not move cgroup membership, so this process's counters are the worker's
/// counters too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
struct CgroupMemoryEvents {
    oom: u64,
    oom_kill: u64,
}

/// Parse the `key value` lines of a cgroup v2 `memory.events` file. Unknown counters and
/// malformed lines are ignored rather than rejected, since the kernel is free to add new
/// counters and this benchmark only ever needs `oom`/`oom_kill`.
fn parse_cgroup_memory_events(contents: &str) -> CgroupMemoryEvents {
    let mut events = CgroupMemoryEvents::default();
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let (Some(key), Some(value)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "oom" => events.oom = value,
            "oom_kill" => events.oom_kill = value,
            _ => {}
        }
    }
    events
}

/// Read this process's current cgroup v2 `memory.events`, if cgroup v2 accounting is mounted
/// and reachable at all. Absence (no `/proc/self/cgroup`, a non-cgroup-v2 hierarchy, or an
/// unreadable `memory.events`) is reported as `None` rather than an error: cgroup evidence is
/// optional corroboration for a kill the benchmark already observed, never a prerequisite.
#[cfg(target_os = "linux")]
fn read_cgroup_memory_events() -> Option<CgroupMemoryEvents> {
    let cgroup = fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim();
    if relative.is_empty() {
        return None;
    }
    let contents = fs::read_to_string(format!("/sys/fs/cgroup{relative}/memory.events")).ok()?;
    Some(parse_cgroup_memory_events(&contents))
}

/// The kernel's 1-minute load average alongside the number of logical CPUs available to this
/// process. `/proc/loadavg`'s load average counts runnable (and briefly uninterruptibly
/// blocked) tasks system-wide, not just this process's own threads, so a value above the CPU
/// count means more runnable work exists than the host has cores for right now. This
/// corroborates a parent-initiated launch/handshake timeout (mini-agent-hdl3i) as an
/// explainable slow host rather than a hang.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
struct HostCpuContention {
    load_average_1m: f64,
    logical_cpus: usize,
}

/// Pure threshold check over already-read evidence: no filesystem access.
fn host_cpu_is_contended(contention: &HostCpuContention) -> bool {
    contention.logical_cpus > 0 && contention.load_average_1m > contention.logical_cpus as f64
}

/// Read this process's view of `/proc/loadavg` and logical CPU count. `None` whenever either is
/// unreadable; unreadable contention evidence can never corroborate a parent timeout, the same
/// fail-closed stance `read_cgroup_memory_events` takes for cgroup accounting.
#[cfg(target_os = "linux")]
fn read_host_cpu_contention() -> Option<HostCpuContention> {
    let contents = fs::read_to_string("/proc/loadavg").ok()?;
    let load_average_1m = contents.split_whitespace().next()?.parse().ok()?;
    let logical_cpus = std::thread::available_parallelism().ok()?.get();
    Some(HostCpuContention {
        load_average_1m,
        logical_cpus,
    })
}

/// What the benchmark observed about one lost worker connection, gathered before deciding
/// whether to resample it as transient host pressure or fail the benchmark outright.
#[derive(Clone, Copy, Debug, Default)]
struct WorkerKillEvidence {
    /// The closed exit-signal class from `test_worker_exit_class`, when the native worker's
    /// exit was actually observed for this attempt. `None` when no exit was observed (for
    /// example, a protocol error that was not caused by worker death).
    exit_class: Option<&'static str>,
    /// Whether this process's own cgroup v2 `memory.events` could be read both before and
    /// after the attempt. Unreadable cgroup accounting can never corroborate host exhaustion.
    cgroup_available: bool,
    /// Increase in the cgroup's `oom_kill` counter observed across the attempt.
    oom_kill_delta: u64,
    /// mini-agent-hdl3i: whether the attempt's own top-level error was `WorkerError::TimedOut`.
    /// `classify_worker_exit` (src/extras/js/supervisor.rs) never produces `TimedOut`: every one
    /// of its branches examines an actually-observed `ExitStatus`. `TimedOut` can only come from
    /// a `tokio::select!` deadline branch firing before the worker's exit was ever observed
    /// (launch-gate wait, launch-thread handoff, or the Hello/Ready read/write deadline in
    /// `read_worker`/`write_parent_encoded`). So `true` here unambiguously means the PARENT gave
    /// up on its own `STEP_TIMEOUT` watchdog, not that the kernel killed the worker.
    parent_timeout: bool,
    /// Host CPU contention measured at classification time, when `/proc/loadavg` was readable.
    host_cpu_contention: Option<HostCpuContention>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerKillCause {
    /// A generic SIGKILL coincides with this process's own cgroup recording a fresh
    /// `oom_kill` during the attempt: the host's out-of-memory killer, not worker containment,
    /// is the most likely source. Safe to resample.
    HostResourceExhaustion,
    /// mini-agent-hdl3i: the attempt's top-level error was `WorkerError::TimedOut` (the parent's
    /// own launch/handshake `STEP_TIMEOUT` watchdog), corroborated by host CPU contention
    /// measured at classification time. Safe to resample: a slower host legitimately exceeding a
    /// generous parent-side deadline is not a containment regression.
    ParentTimeoutUnderContention,
    /// Every other observed failure: a different signal, a plain SIGKILL with no corroborating
    /// `oom_kill` evidence, cgroup accounting that could not be read at all, or a parent timeout
    /// with no corroborating host CPU contention. Must still fail the benchmark so a deliberate
    /// containment regression, or a genuine hang, stays red.
    ContainmentRegression,
}

/// Pure classification over already-parsed evidence: no filesystem or process access.
///
/// A parent-initiated timeout (`parent_timeout`) is resampleable only when host CPU contention
/// was both readable and above the logical CPU count; an uncorroborated timeout stays a
/// containment regression, since it could equally be a genuine hang. Otherwise, a plain
/// `signal_kill` is host resource exhaustion only when the host's own cgroup accounting both was
/// readable and recorded a fresh `oom_kill` while this attempt was running; every other signal
/// (abort/segv/bus/other) and every uncorroborated kill is a containment regression.
fn classify_worker_kill(evidence: &WorkerKillEvidence) -> WorkerKillCause {
    if evidence.parent_timeout {
        return match evidence.host_cpu_contention {
            Some(contention) if host_cpu_is_contended(&contention) => {
                WorkerKillCause::ParentTimeoutUnderContention
            }
            _ => WorkerKillCause::ContainmentRegression,
        };
    }
    if evidence.exit_class == Some("signal_kill")
        && evidence.cgroup_available
        && evidence.oom_kill_delta > 0
    {
        WorkerKillCause::HostResourceExhaustion
    } else {
        WorkerKillCause::ContainmentRegression
    }
}

/// Snapshot the cgroup counters before an attempt and reconcile them with whatever exit status
/// the attempt's scoped `WORKER_EXIT_OBSERVER` channel received, if any, plus host CPU
/// contention evidence for a parent-initiated `error`.
#[cfg(target_os = "linux")]
fn classify_benchmark_exit(
    before: Option<CgroupMemoryEvents>,
    exit_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ExitStatus>,
    error: &WorkerError,
) -> WorkerKillEvidence {
    let after = read_cgroup_memory_events();
    let exit_class = exit_rx.try_recv().ok().map(test_worker_exit_class);
    WorkerKillEvidence {
        exit_class,
        cgroup_available: before.is_some() && after.is_some(),
        oom_kill_delta: match (before, after) {
            (Some(before), Some(after)) => after.oom_kill.saturating_sub(before.oom_kill),
            _ => 0,
        },
        parent_timeout: matches!(error, WorkerError::TimedOut),
        host_cpu_contention: read_host_cpu_contention(),
    }
}

/// Every retry for one sample was still classified as host resource exhaustion. Carries only
/// closed diagnostic labels/counts and the worker's own closed `WorkerError` message, never
/// worker stdout/stderr, exception text, or raw exit numbers.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct HostResourceExhausted {
    last_error: WorkerError,
    exit_class: Option<&'static str>,
    oom_kill_delta: u64,
    attempts: u32,
}

#[cfg(target_os = "linux")]
impl std::fmt::Display for HostResourceExhausted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "js worker benchmark sample repeatedly lost its worker to host resource exhaustion \
             across {} attempts (exit_class={:?}, oom_kill_delta={}): {}",
            self.attempts, self.exit_class, self.oom_kill_delta, self.last_error
        )
    }
}

#[cfg(target_os = "linux")]
impl std::error::Error for HostResourceExhausted {}

/// mini-agent-hdl3i: every retry for one sample was still classified as a parent-initiated
/// launch/handshake timeout corroborated by host CPU contention. Carries only closed
/// diagnostic labels/counts and the worker's own closed `WorkerError` message, never worker
/// stdout/stderr, exception text, or raw exit numbers.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct ParentTimeoutSkip {
    last_error: WorkerError,
    exit_class: Option<&'static str>,
    host_cpu_contention: Option<HostCpuContention>,
    step_timeout_seconds: u64,
    attempts: u32,
}

#[cfg(target_os = "linux")]
impl std::fmt::Display for ParentTimeoutSkip {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "js worker benchmark sample repeatedly lost its worker to its own parent-side \
             launch/handshake watchdog ({}s) under corroborated host CPU contention across {} \
             attempts (exit_class={:?}, host_cpu_contention={:?}): {}",
            self.step_timeout_seconds,
            self.attempts,
            self.exit_class,
            self.host_cpu_contention,
            self.last_error
        )
    }
}

#[cfg(target_os = "linux")]
impl std::error::Error for ParentTimeoutSkip {}

/// A sample-loop attempt failed in a way `classify_worker_kill` could not call host resource
/// exhaustion or a corroborated parent timeout (a different signal, an uncorroborated SIGKILL,
/// an uncorroborated parent timeout, or no observed worker exit at all). This still becomes a
/// hard test failure; it exists only to carry the same closed diagnostic evidence
/// `HostResourceExhausted`/`ParentTimeoutSkip` carry for the resampled cases, so a genuine
/// containment regression is just as diagnosable from the failure artifact — including whether
/// the parent gave up on its own watchdog (`parent_timeout`) or the kernel delivered a signal
/// the worker's own exit status reported (`exit_class`).
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct ContainmentRegressionKill {
    source: WorkerError,
    exit_class: Option<&'static str>,
    parent_timeout: bool,
    host_cpu_contention: Option<HostCpuContention>,
    attempts: u32,
    memory_events_before: Option<CgroupMemoryEvents>,
    memory_events_after: Option<CgroupMemoryEvents>,
}

#[cfg(target_os = "linux")]
impl std::fmt::Display for ContainmentRegressionKill {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "js worker benchmark sample lost its worker in a way that was not corroborated host \
             resource exhaustion or a corroborated parent timeout, across {} attempt(s) \
             (exit_class={:?}, parent_timeout={}, host_cpu_contention={:?}, memory.events \
             before={:?}, after={:?}): {}",
            self.attempts,
            self.exit_class,
            self.parent_timeout,
            self.host_cpu_contention,
            self.memory_events_before,
            self.memory_events_after,
            self.source
        )
    }
}

#[cfg(target_os = "linux")]
impl std::error::Error for ContainmentRegressionKill {}

/// Retry the wrapped single-sample attempt expression up to `HOST_RESOURCE_RETRY_ATTEMPTS`
/// additional times when its failure is corroborated host resource exhaustion OR a corroborated
/// parent-initiated launch/handshake timeout (mini-agent-hdl3i). Any other failure, or
/// exhausting the retries while still in one of those two classes, returns an error: the caller
/// decides whether an exhausted-retries `HostResourceExhausted`/`ParentTimeoutSkip` becomes a
/// recorded skip or an ordinary containment-regression failure becomes a hard test failure.
///
/// Timing samples that were resampled include the discarded attempts' wait-for-death latency;
/// this benchmark's timing targets are already informational only (see "without gating noisy
/// timing percentiles" in the CI job), so a small number of inflated latency samples is an
/// accepted trade-off for not discarding an otherwise-complete run.
#[cfg(target_os = "linux")]
macro_rules! retry_sample_on_host_resource_exhaustion {
    ($attempt:expr) => {{
        let mut retries_left = HOST_RESOURCE_RETRY_ATTEMPTS;
        let mut attempts = 1u32;
        loop {
            let (exit_tx, mut exit_rx) = tokio::sync::mpsc::unbounded_channel();
            let before = read_cgroup_memory_events();
            match WORKER_EXIT_OBSERVER.scope(exit_tx, $attempt).await {
                Ok(value) => break Ok(value),
                Err(error) => {
                    let evidence = classify_benchmark_exit(before, &mut exit_rx, &error);
                    let cause = classify_worker_kill(&evidence);
                    if retries_left > 0
                        && matches!(
                            cause,
                            WorkerKillCause::HostResourceExhaustion
                                | WorkerKillCause::ParentTimeoutUnderContention
                        )
                    {
                        match cause {
                            WorkerKillCause::HostResourceExhaustion => eprintln!(
                                "JS_WORKER_BENCHMARK_HOST_RESOURCE_RETRY exit_class={:?} \
                                 oom_kill_delta={} attempt={attempts} retries_left={retries_left}",
                                evidence.exit_class, evidence.oom_kill_delta,
                            ),
                            WorkerKillCause::ParentTimeoutUnderContention => eprintln!(
                                "JS_WORKER_BENCHMARK_PARENT_TIMEOUT_RETRY \
                                 host_cpu_contention={:?} attempt={attempts} \
                                 retries_left={retries_left}",
                                evidence.host_cpu_contention,
                            ),
                            WorkerKillCause::ContainmentRegression => unreachable!(
                                "the outer match already excludes ContainmentRegression"
                            ),
                        }
                        retries_left -= 1;
                        attempts += 1;
                        continue;
                    }
                    break Err(match cause {
                        WorkerKillCause::HostResourceExhaustion => Box::new(HostResourceExhausted {
                            last_error: error,
                            exit_class: evidence.exit_class,
                            oom_kill_delta: evidence.oom_kill_delta,
                            attempts,
                        })
                            as Box<dyn std::error::Error>,
                        WorkerKillCause::ParentTimeoutUnderContention => {
                            Box::new(ParentTimeoutSkip {
                                last_error: error,
                                exit_class: evidence.exit_class,
                                host_cpu_contention: evidence.host_cpu_contention,
                                step_timeout_seconds: crate::extras::js::types::STEP_TIMEOUT
                                    .as_secs(),
                                attempts,
                            }) as Box<dyn std::error::Error>
                        }
                        WorkerKillCause::ContainmentRegression => {
                            Box::new(ContainmentRegressionKill {
                                source: error,
                                exit_class: evidence.exit_class,
                                parent_timeout: evidence.parent_timeout,
                                host_cpu_contention: evidence.host_cpu_contention,
                                attempts,
                                memory_events_before: before,
                                memory_events_after: read_cgroup_memory_events(),
                            }) as Box<dyn std::error::Error>
                        }
                    });
                }
            }
        }
    }};
}

/// Parse the `utime`/`stime` fields (in clock ticks) out of a `/proc/<pid>/stat` line. Pure
/// over already-read text: no filesystem access, so this runs identically on every host
/// regardless of whether `/proc` is mounted. The `comm` field can itself contain `") "`, so the
/// split anchors on the LAST `") "` the same way `crate::tests::process_state` already does.
#[cfg(target_os = "linux")]
fn parse_proc_stat_cpu_ticks(stat: &str) -> Option<(u64, u64)> {
    let (_, tail) = stat.rsplit_once(") ")?;
    let fields: Vec<_> = tail.split_whitespace().collect();
    let utime = fields.get(11)?.parse().ok()?;
    let stime = fields.get(12)?.parse().ok()?;
    Some((utime, stime))
}

/// The kernel's clock ticks per second, for converting `/proc/<pid>/stat` CPU fields to
/// seconds. `None` only if the host's `sysconf` itself refuses the query.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn linux_clock_ticks_per_second() -> Option<u64> {
    // SAFETY: `sysconf` with the fixed `_SC_CLK_TCK` name takes no pointers and performs no
    // side effects; it is always safe to call.
    u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()
}

/// Best-effort native CPU time (user + system) the given process has consumed so far, read
/// from its own `/proc/<pid>/stat`. `None` whenever the process is gone, `/proc` is
/// unavailable, or the host's clock-tick rate could not be read; this is diagnostic evidence
/// only and never gates the benchmark's pass/fail decision.
#[cfg(target_os = "linux")]
fn read_worker_cpu_seconds(pid: u32) -> Option<f64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (utime, stime) = parse_proc_stat_cpu_ticks(&stat)?;
    let ticks_per_second = linux_clock_ticks_per_second()?;
    if ticks_per_second == 0 {
        return None;
    }
    Some((utime + stime) as f64 / ticks_per_second as f64)
}

/// Snapshot the persistent worker's own accumulated native CPU time, for the diagnostic
/// context attached to a sample-loop failure below. Reads the supervisor's currently idle
/// connection's PID; `None` whenever there is none (for example, a prior iteration's failure
/// already tore it down) or the CPU time itself could not be read.
#[cfg(target_os = "linux")]
async fn capture_worker_cpu_seconds(supervisor: &JsWorkerSupervisor) -> Option<f64> {
    let pid = supervisor.process_id_for_test().await?;
    read_worker_cpu_seconds(pid)
}

/// Diagnostic context for a sample-loop attempt that failed in a way `annotate_sample_loop_failure`
/// decided was not an already-handled host-resource-exhaustion resample. Carries which phase and
/// iteration failed, how long the benchmark had been running, and (best effort) the persistent
/// worker's own native CPU time against its configured ceiling, so the failure is diagnosable
/// from the written JSON artifact alone without needing to reproduce it.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct SampleLoopFailure {
    phase: &'static str,
    iteration: usize,
    total_elapsed_seconds: f64,
    worker_cpu_seconds_before_attempt: Option<f64>,
    worker_cpu_limit_seconds: u64,
    source: Box<dyn std::error::Error>,
}

#[cfg(target_os = "linux")]
impl std::fmt::Display for SampleLoopFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} sample loop failed at iteration {} ({:.1}s into the benchmark run; the worker \
             had consumed {} of its {}s native CPU ceiling before this attempt started): {}",
            self.phase,
            self.iteration,
            self.total_elapsed_seconds,
            self.worker_cpu_seconds_before_attempt
                .map(|seconds| format!("{seconds:.1}s"))
                .unwrap_or_else(|| "an unreadable amount".into()),
            self.worker_cpu_limit_seconds,
            self.source,
        )
    }
}

#[cfg(target_os = "linux")]
impl std::error::Error for SampleLoopFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Attach sample-loop diagnostic context to a failed attempt, unless it is already the
/// existing `HostResourceExhausted` or `ParentTimeoutSkip` carrier that the caller's top-level
/// match still needs to recognize by exact type to trigger the established skip-and-record
/// path. Every other failure (an uncorroborated kill, a crash signal, or any other error) is
/// wrapped so it can be diagnosed from the artifact alone.
#[cfg(target_os = "linux")]
fn annotate_sample_loop_failure(
    error: Box<dyn std::error::Error>,
    phase: &'static str,
    iteration: usize,
    total_elapsed: Duration,
    worker_cpu_seconds_before_attempt: Option<f64>,
) -> Box<dyn std::error::Error> {
    if error.downcast_ref::<HostResourceExhausted>().is_some()
        || error.downcast_ref::<ParentTimeoutSkip>().is_some()
    {
        return error;
    }
    Box::new(SampleLoopFailure {
        phase,
        iteration,
        total_elapsed_seconds: total_elapsed.as_secs_f64(),
        worker_cpu_seconds_before_attempt,
        worker_cpu_limit_seconds: crate::sandbox::worker::linux_worker_cpu_limit_seconds_for_test(),
        source: error,
    })
}

#[cfg(target_os = "linux")]
async fn observe_worker_processes(
    supervisor: &JsWorkerSupervisor,
    worker_executable: &Path,
) -> Result<ProcessObservation, Box<dyn std::error::Error>> {
    // The reuse policy may retire the worker when a call finishes (the benchmark's bounded
    // invocation budget, or the Linux native-CPU retirement threshold), leaving no idle
    // connection. Observation runs outside every timed section, so re-establish a ready
    // worker here rather than inside the next measured call.
    supervisor.prepare_ready_for_benchmark_for_test().await?;
    let launcher_pid = supervisor
        .process_id_for_test()
        .await
        .ok_or("supervisor has no authenticated idle worker connection")?;
    observe_containment_tree(launcher_pid, worker_executable)
}

#[cfg(target_os = "linux")]
fn observe_containment_tree(
    launcher_pid: u32,
    worker_executable: &Path,
) -> Result<ProcessObservation, Box<dyn std::error::Error>> {
    use std::collections::BTreeSet;
    use std::os::unix::fs::MetadataExt;

    let expected = fs::metadata(worker_executable)?;
    let expected_identity = (expected.dev(), expected.ino());
    let mut pending = VecDeque::from([launcher_pid]);
    let mut observed = BTreeSet::new();
    let mut exact_workers = Vec::new();
    while let Some(pid) = pending.pop_front() {
        if !observed.insert(pid) {
            continue;
        }
        if observed.len() > MAX_CONTAINMENT_TREE_PROCESSES {
            return Err("containment process tree exceeded the benchmark observation bound".into());
        }
        let executable = fs::metadata(format!("/proc/{pid}/exe"))?;
        if (executable.dev(), executable.ino()) == expected_identity {
            exact_workers.push(pid);
        }
        let children = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))?;
        for child in children.split_whitespace() {
            pending.push_back(child.parse()?);
        }
    }
    if exact_workers.len() != 1 {
        return Err(format!(
            "authenticated containment tree has {} exact-executable worker processes",
            exact_workers.len()
        )
        .into());
    }
    Ok(ProcessObservation {
        exact_worker_pid: exact_workers[0],
        worker_processes: exact_workers.len() as u32,
        containment_helper_processes: (observed.len() - exact_workers.len()) as u32,
        observation_kind: LINUX_PROCESS_OBSERVATION_KIND,
        observation: LINUX_PROCESS_PROOF,
    })
}

#[cfg(windows)]
async fn observe_worker_processes(
    supervisor: &JsWorkerSupervisor,
    _worker_executable: &Path,
) -> Result<ProcessObservation, Box<dyn std::error::Error>> {
    // The reuse policy may retire the worker when a call finishes (the benchmark's bounded
    // invocation budget, or the Linux native-CPU retirement threshold), leaving no idle
    // connection. Observation runs outside every timed section, so re-establish a ready
    // worker here rather than inside the next measured call.
    supervisor.prepare_ready_for_benchmark_for_test().await?;
    let observation = supervisor
        .windows_process_observation_for_test()
        .await?
        .ok_or("supervisor has no authenticated idle Windows worker connection")?;
    if observation.active_job_processes == 0 {
        return Err("owned Windows worker Job contains no active process".into());
    }
    Ok(ProcessObservation {
        exact_worker_pid: observation.exact_worker_pid,
        worker_processes: observation.active_job_processes,
        containment_helper_processes: 0,
        observation_kind: WINDOWS_PROCESS_OBSERVATION_KIND,
        observation: WINDOWS_PROCESS_PROOF,
    })
}

#[cfg(target_os = "macos")]
async fn observe_worker_processes(
    supervisor: &JsWorkerSupervisor,
    _worker_executable: &Path,
) -> Result<ProcessObservation, Box<dyn std::error::Error>> {
    // The reuse policy may retire the worker when a call finishes (the benchmark's bounded
    // invocation budget, or the Linux native-CPU retirement threshold), leaving no idle
    // connection. Observation runs outside every timed section, so re-establish a ready
    // worker here rather than inside the next measured call.
    supervisor.prepare_ready_for_benchmark_for_test().await?;
    let guardian_pid = supervisor
        .process_id_for_test()
        .await
        .ok_or("supervisor has no authenticated idle worker connection")?;
    observe_macos_guardian_group(guardian_pid)
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MacosProcBsdShortInfo {
    pid: u32,
    parent_pid: u32,
    process_group: u32,
    status: u32,
    command: [std::os::raw::c_char; 16],
    flags: u32,
    uid: u32,
    gid: u32,
    real_uid: u32,
    real_gid: u32,
    saved_uid: u32,
    saved_gid: u32,
    reserved: u32,
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn observe_macos_guardian_group(
    guardian_pid: u32,
) -> Result<ProcessObservation, Box<dyn std::error::Error>> {
    use std::collections::BTreeSet;

    const PROC_PIDT_SHORTBSDINFO: i32 = 13;
    const ZOMBIE_STATUS: u32 = 5;
    unsafe extern "C" {
        fn proc_listpgrppids(group: libc::pid_t, buffer: *mut libc::c_void, size: i32) -> i32;
        fn proc_pidinfo(
            pid: libc::pid_t,
            flavor: i32,
            argument: u64,
            buffer: *mut libc::c_void,
            size: i32,
        ) -> i32;
    }

    let group = libc::pid_t::try_from(guardian_pid)?;
    // libproc's null-buffer sizing result is an allocation hint rather than a
    // trustworthy live-member count on every supported macOS release. Use a
    // fixed one-over-limit buffer so observation stays bounded and detects a
    // saturated result as ambiguous instead of allocating from the hint.
    let mut pids = vec![0; MAX_CONTAINMENT_TREE_PROCESSES + 1];
    let bytes = pids
        .len()
        .checked_mul(std::mem::size_of::<libc::pid_t>())
        .and_then(|size| i32::try_from(size).ok())
        .ok_or("macOS guardian process group buffer exceeded its bound")?;
    let read = unsafe { proc_listpgrppids(group, pids.as_mut_ptr().cast(), bytes) };
    if read < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if read == 0 {
        return Err("authenticated macOS guardian process group is empty".into());
    }
    if read as usize > MAX_CONTAINMENT_TREE_PROCESSES {
        return Err("macOS guardian process group changed beyond the observation bound".into());
    }
    pids.truncate(read as usize);

    let mut live = BTreeSet::new();
    for pid in pids.into_iter().filter(|pid| *pid > 0) {
        let mut info = std::mem::MaybeUninit::<MacosProcBsdShortInfo>::uninit();
        let received = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDT_SHORTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<MacosProcBsdShortInfo>() as i32,
            )
        };
        if received != std::mem::size_of::<MacosProcBsdShortInfo>() as i32 {
            continue;
        }
        let info = unsafe { info.assume_init() };
        if info.status != ZOMBIE_STATUS && info.process_group == guardian_pid {
            live.insert(pid as u32);
        }
    }
    if !live.remove(&guardian_pid) {
        return Err("authenticated macOS guardian is absent from its process group".into());
    }
    if live.len() != 1 {
        return Err(format!(
            "authenticated macOS guardian process group has {} live non-guardian members",
            live.len()
        )
        .into());
    }
    Ok(ProcessObservation {
        exact_worker_pid: *live.first().expect("one live worker was established"),
        worker_processes: 1,
        containment_helper_processes: 1,
        observation_kind: MACOS_PROCESS_OBSERVATION_KIND,
        observation: MACOS_PROCESS_PROOF,
    })
}

async fn pure_call(supervisor: &JsWorkerSupervisor) -> Result<(), WorkerError> {
    let result = supervisor
        .execute(
            RunStep::new("42".into()),
            PayloadEffects,
            PermCancellation::new(),
        )
        .await?;
    if result.outcome != StepOutcome::Value("42".into()) {
        return Err(WorkerError::Protocol);
    }
    Ok(())
}

async fn ipc_call(supervisor: &JsWorkerSupervisor) -> Result<(), WorkerError> {
    let request = RunStep::new("read_file('benchmark-payload').length".into())
        .with_model_grant(GrantId::new(uuid::Uuid::from_u128(1)).unwrap());
    let result = supervisor
        .execute(request, PayloadEffects, PermCancellation::new())
        .await?;
    if result.outcome != StepOutcome::Value(IPC_PAYLOAD_BYTES.to_string()) {
        return Err(WorkerError::Protocol);
    }
    Ok(())
}

async fn wait_for_repeated_worker_effects(requests: &AtomicUsize) -> Result<(), WorkerError> {
    tokio::time::timeout(Duration::from_secs(2), async {
        while requests.load(Ordering::Acquire) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| WorkerError::TimedOut)
}

async fn cancel_and_recover(supervisor: Arc<JsWorkerSupervisor>) -> Result<(), WorkerError> {
    let cancelled_generation = supervisor
        .generation_for_test()
        .await
        .ok_or(WorkerError::Protocol)?;
    let cancellation = PermCancellation::new();
    let effects = CancellationProbeEffects::default();
    let worker_requests = effects.worker_requests.clone();
    let task_supervisor = supervisor.clone();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        task_supervisor
            .execute(
                RunStep::new("for (;;) { read_file('benchmark-payload'); }".into())
                    .with_model_grant(GrantId::new(uuid::Uuid::from_u128(2)).unwrap()),
                effects,
                task_cancellation,
            )
            .await
    });
    // A second worker-originated request proves that the RunStep arrived, the worker consumed
    // one broker response, and JavaScript resumed inside the non-terminating loop.
    wait_for_repeated_worker_effects(&worker_requests).await?;
    cancellation.cancel();
    if task.await.map_err(|_| WorkerError::Transport)? != Err(WorkerError::Cancelled) {
        return Err(WorkerError::Protocol);
    }
    pure_call(&supervisor).await?;
    let recovered_generation = supervisor
        .generation_for_test()
        .await
        .ok_or(WorkerError::Protocol)?;
    if recovered_generation <= cancelled_generation {
        return Err(WorkerError::Protocol);
    }
    Ok(())
}

async fn run_production_benchmark(
    worker_executable: PathBuf,
    launcher: BenchmarkWorkerLauncher,
    backend: String,
    assurance: String,
) -> Result<BenchmarkRun, Box<dyn std::error::Error>> {
    const {
        assert!(
            cfg!(debug_assertions),
            "worker benchmark must use a debug binary"
        )
    };

    // mini-agent-hdl3i: wall-clock elapsed since this benchmark run started, attached to any
    // sample-loop failure below so it can be compared against the ~30-31s observed in CI run
    // 39639586216 and 39689562062.
    #[cfg(target_os = "linux")]
    let test_started = Instant::now();

    let mut process_counts = ProcessCountAccumulator::default();
    let mut cold = Vec::with_capacity(SAMPLES);
    for iteration in 0..(WARMUPS + SAMPLES) {
        let supervisor = production_supervisor(launcher.clone());
        let started = Instant::now();
        // mini-agent-hdl3i: this iteration's supervisor is brand new, so there is no
        // already-established worker whose CPU time could be read before this attempt.
        #[cfg(target_os = "linux")]
        {
            let result: Result<(), Box<dyn std::error::Error>> = retry_sample_on_host_resource_exhaustion!(
                supervisor.prepare_ready_for_benchmark_for_test()
            );
            if let Err(error) = result {
                return Err(annotate_sample_loop_failure(
                    error,
                    "cold_ready",
                    iteration,
                    test_started.elapsed(),
                    None,
                ));
            }
        }
        #[cfg(not(target_os = "linux"))]
        supervisor.prepare_ready_for_benchmark_for_test().await?;
        let elapsed = elapsed_us(started);
        // A retried attempt launches more than one generation on this same fresh supervisor
        // before succeeding, so only the lower bound (this iteration never reused an older
        // supervisor's worker) is asserted here, not an exact count.
        assert!(
            supervisor
                .generation_for_test()
                .await
                .is_some_and(|generation| generation >= 1)
        );
        process_counts.observe(observe_worker_processes(&supervisor, &worker_executable).await?);
        if iteration >= WARMUPS {
            cold.push(elapsed);
            // Source-free phase breakdown of the fresh-worker launch, so a
            // cold-start regression names the phase that grew instead of only
            // its total.
            #[cfg(target_os = "macos")]
            if let Some(profile) = crate::sandbox::worker::macos_last_launch_profile() {
                eprintln!(
                    "JS_WORKER_LAUNCH_PHASES total_us={elapsed:.0} sweep_us={} image_us={} \
                     clone_us={} source_digest_us={} image_digest_us={} profile_us={} spawn_us={}",
                    profile.publication_sweep_us,
                    profile.image_preparation_us,
                    profile.image_clone_us,
                    profile.source_digest_us,
                    profile.image_digest_us,
                    profile.profile_render_us,
                    profile.guardian_spawn_us,
                );
            }
        }
        supervisor.shutdown_for_test().await?;
    }

    let supervisor = production_supervisor_with_bounded_cpu_budget(launcher);
    supervisor.prepare_ready_for_benchmark_for_test().await?;
    process_counts.observe(observe_worker_processes(&supervisor, &worker_executable).await?);

    let mut warm = Vec::with_capacity(SAMPLES);
    for iteration in 0..(WARMUPS + SAMPLES) {
        #[cfg(target_os = "linux")]
        let worker_cpu_before_attempt = capture_worker_cpu_seconds(&supervisor).await;
        let started = Instant::now();
        #[cfg(target_os = "linux")]
        {
            let result: Result<(), Box<dyn std::error::Error>> =
                retry_sample_on_host_resource_exhaustion!(pure_call(&supervisor));
            if let Err(error) = result {
                return Err(annotate_sample_loop_failure(
                    error,
                    "warm_pure_call",
                    iteration,
                    test_started.elapsed(),
                    worker_cpu_before_attempt,
                ));
            }
        }
        #[cfg(not(target_os = "linux"))]
        pure_call(&supervisor).await?;
        let elapsed = elapsed_us(started);
        process_counts.observe(observe_worker_processes(&supervisor, &worker_executable).await?);
        if iteration >= WARMUPS {
            warm.push(elapsed);
        }
    }

    let mut ipc = Vec::with_capacity(SAMPLES);
    for iteration in 0..(WARMUPS + SAMPLES) {
        #[cfg(target_os = "linux")]
        let worker_cpu_before_attempt = capture_worker_cpu_seconds(&supervisor).await;
        let started = Instant::now();
        #[cfg(target_os = "linux")]
        {
            let result: Result<(), Box<dyn std::error::Error>> =
                retry_sample_on_host_resource_exhaustion!(ipc_call(&supervisor));
            if let Err(error) = result {
                return Err(annotate_sample_loop_failure(
                    error,
                    "broker_ipc_4kib",
                    iteration,
                    test_started.elapsed(),
                    worker_cpu_before_attempt,
                ));
            }
        }
        #[cfg(not(target_os = "linux"))]
        ipc_call(&supervisor).await?;
        let elapsed = elapsed_us(started);
        process_counts.observe(observe_worker_processes(&supervisor, &worker_executable).await?);
        if iteration >= WARMUPS {
            ipc.push(elapsed);
        }
    }

    let mut memory = Vec::with_capacity(SAMPLES);
    for iteration in 0..(WARMUPS + SAMPLES) {
        let observation = observe_worker_processes(&supervisor, &worker_executable).await?;
        process_counts.observe(observation);
        let measured = private_memory_bytes(observation.exact_worker_pid)?;
        if iteration >= WARMUPS {
            memory.push(measured);
        }
    }

    let mut recovery = Vec::with_capacity(SAMPLES);
    for iteration in 0..(WARMUPS + SAMPLES) {
        #[cfg(target_os = "linux")]
        let worker_cpu_before_attempt = capture_worker_cpu_seconds(&supervisor).await;
        let started = Instant::now();
        #[cfg(target_os = "linux")]
        {
            let result: Result<(), Box<dyn std::error::Error>> =
                retry_sample_on_host_resource_exhaustion!(cancel_and_recover(supervisor.clone()));
            if let Err(error) = result {
                return Err(annotate_sample_loop_failure(
                    error,
                    "post_cancel_recovery",
                    iteration,
                    test_started.elapsed(),
                    worker_cpu_before_attempt,
                ));
            }
        }
        #[cfg(not(target_os = "linux"))]
        cancel_and_recover(supervisor.clone()).await?;
        let elapsed = elapsed_us(started);
        process_counts.observe(observe_worker_processes(&supervisor, &worker_executable).await?);
        if iteration >= WARMUPS {
            recovery.push(elapsed);
        }
    }

    let final_processes = observe_worker_processes(&supervisor, &worker_executable).await?;
    process_counts.observe(final_processes);
    supervisor.shutdown_for_test().await?;

    let cold = summarize_microseconds(&cold);
    let warm = summarize_microseconds(&warm);
    let ipc = summarize_microseconds(&ipc);
    let recovery = summarize_microseconds(&recovery);
    let maximum_memory = *memory.iter().max().unwrap();
    let mean_memory = memory.iter().map(|bytes| *bytes as f64).sum::<f64>() / memory.len() as f64;
    let machine = machine();
    let template = benchmark_report_template();
    let cold_target = template.targets.cold_ready_us[&machine.os];
    let process_observation_kind = process_counts
        .observation_kind
        .ok_or("benchmark recorded no worker-process observation method")?;
    let process_observation = process_counts
        .observation
        .ok_or("benchmark recorded no worker-process observation proof")?;
    Ok(BenchmarkRun {
        machine,
        containment: Containment { backend, assurance },
        target_results: TargetResults {
            cold_ready: cold.p95_us <= cold_target,
            warm_pure_call: warm.p95_us <= template.targets.warm_pure_call_us,
            broker_ipc_4kib: ipc.p95_us <= template.targets.broker_ipc_4kib_us,
            idle_private_memory: maximum_memory <= template.targets.idle_private_bytes,
            post_cancel_recovery: recovery.p95_us <= template.targets.post_cancel_recovery_us,
            one_worker_zero_idle_runtimes: process_counts.maximum_worker_processes == 1
                && process_counts.last_idle_worker_processes == 1
                && !process_observation_kind.is_empty(),
            timing_targets_are_informational: true,
        },
        latency: LatencyMeasurements {
            cold_ready: cold,
            warm_pure_call: warm,
            broker_ipc_4kib: ipc,
            post_cancel_recovery: recovery,
        },
        idle_private_memory: MemoryMeasurement {
            samples: memory.len(),
            mean_bytes: mean_memory,
            maximum_bytes: maximum_memory,
            measurement: private_memory_method().into(),
        },
        counts: CountMeasurements {
            maximum_observed_worker_processes: process_counts.maximum_worker_processes,
            maximum_observed_containment_helper_processes: process_counts
                .maximum_containment_helper_processes,
            idle_worker_processes: process_counts.last_idle_worker_processes,
            worker_process_observation_kind: process_observation_kind.into(),
            worker_process_observation: process_observation.into(),
            idle_runtimes: 0,
            idle_runtime_observation_kind: IDLE_RUNTIME_OBSERVATION_KIND.into(),
            idle_runtime_observation: IDLE_RUNTIME_PROOF.into(),
        },
        comparison: None,
    })
}

fn add_comparison(
    run: &mut BenchmarkRun,
    previous_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let previous_value: Value = serde_json::from_slice(&fs::read(previous_path)?)?;
    validate_report(&previous_value)
        .map_err(|error| format!("comparison file failed benchmark validation: {error}"))?;
    let previous: BenchmarkReport = serde_json::from_value(previous_value)?;
    let previous = previous
        .platform_evidence
        .iter()
        .rev()
        .find_map(|candidate| match candidate {
            PlatformEvidence::Measured { run: candidate }
                if candidate.machine.os == run.machine.os =>
            {
                Some(candidate.as_ref())
            }
            _ => None,
        })
        .ok_or("comparison file contains no measured run for the current OS")?;
    if previous.machine != run.machine || previous.containment != run.containment {
        return Err("comparison file was not recorded with the same machine, debug build, and containment method".into());
    }
    let metrics = BTreeMap::from([
        (
            "cold_ready".into(),
            compare_statistics(&previous.latency.cold_ready, &run.latency.cold_ready),
        ),
        (
            "warm_pure_call".into(),
            compare_statistics(
                &previous.latency.warm_pure_call,
                &run.latency.warm_pure_call,
            ),
        ),
        (
            "broker_ipc_4kib".into(),
            compare_statistics(
                &previous.latency.broker_ipc_4kib,
                &run.latency.broker_ipc_4kib,
            ),
        ),
        (
            "post_cancel_recovery".into(),
            compare_statistics(
                &previous.latency.post_cancel_recovery,
                &run.latency.post_cancel_recovery,
            ),
        ),
    ]);
    run.comparison = Some(RunComparison {
        reference_machine: previous.machine.clone(),
        reference_containment: previous.containment.clone(),
        previous_build_identity: previous.machine.build_identity.clone(),
        all_within_documented_variance: metrics
            .values()
            .all(|metric| metric.within_documented_variance),
        metrics,
        informational_only: true,
    });
    Ok(())
}

fn machine() -> Machine {
    Machine {
        host: environment_first(&["RUNNER_NAME", "HOSTNAME", "COMPUTERNAME"]),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        kernel: kernel_identity(),
        cpu: cpu_identity(),
        logical_cpus: std::thread::available_parallelism().map_or(0, usize::from),
        memory_bytes: total_memory_bytes(),
        binary_profile: "debug".into(),
        package_version: env!("CARGO_PKG_VERSION").into(),
        build_identity: crate::extras::js::protocol::BuildIdentity::current()
            .as_str()
            .to_string(),
    }
}

fn environment_first(names: &[&str]) -> String {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(unix)]
fn command_output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(unix)]
fn kernel_identity() -> String {
    command_output("uname", &["-sr"]).unwrap_or_else(|| "unknown".into())
}

#[cfg(windows)]
fn kernel_identity() -> String {
    windows_powershell(&[
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Get-CimInstance Win32_OperatingSystem | ForEach-Object { '{0}|{1}|{2}' -f $_.Caption,$_.Version,$_.BuildNumber }",
    ])
    .and_then(|identity| parse_windows_kernel_identity(&identity))
    .unwrap_or_else(|| "unknown".into())
}

#[cfg(any(windows, test))]
fn parse_windows_kernel_identity(identity: &str) -> Option<String> {
    let mut fields = identity.trim().split('|').map(str::trim);
    let caption = fields.next().filter(|field| !field.is_empty())?;
    let version = fields.next().filter(|field| !field.is_empty())?;
    let build = fields.next().filter(|field| !field.is_empty())?;
    if fields.next().is_some() {
        return None;
    }
    Some(format!("{caption} {version} build {build}"))
}

#[cfg(target_os = "linux")]
fn cpu_identity() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|contents| {
            contents
                .lines()
                .find_map(|line| line.strip_prefix("model name\t: ").map(str::to_owned))
        })
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(target_os = "macos")]
fn cpu_identity() -> String {
    command_output("/usr/sbin/sysctl", &["-n", "machdep.cpu.brand_string"])
        .or_else(|| command_output("/usr/sbin/sysctl", &["-n", "hw.model"]))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(windows)]
fn cpu_identity() -> String {
    environment_first(&["PROCESSOR_IDENTIFIER", "PROCESSOR_ARCHITECTURE"])
}

#[cfg(target_os = "linux")]
fn total_memory_bytes() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|contents| {
            contents
                .lines()
                .find_map(|line| line.strip_prefix("MemTotal:").map(str::to_owned))
        })
        .and_then(|value| value.split_whitespace().next()?.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_mul(1024)
}

#[cfg(target_os = "macos")]
fn total_memory_bytes() -> u64 {
    command_output("/usr/sbin/sysctl", &["-n", "hw.memsize"])
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

#[cfg(windows)]
fn total_memory_bytes() -> u64 {
    windows_powershell(&[
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory",
    ])
    .and_then(|value| value.parse().ok())
    .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn private_memory_bytes(pid: u32) -> io::Result<u64> {
    let contents = fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))?;
    let kib = contents
        .lines()
        .filter_map(|line| {
            ["Private_Clean:", "Private_Dirty:", "Private_Hugetlb:"]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
        })
        .filter_map(|value| value.split_whitespace().next()?.parse::<u64>().ok())
        .sum::<u64>();
    Ok(kib.saturating_mul(1024))
}

#[cfg(target_os = "linux")]
fn private_memory_method() -> &'static str {
    LINUX_MEMORY_MEASUREMENT
}

#[cfg(target_os = "macos")]
fn private_memory_bytes(pid: u32) -> io::Result<u64> {
    let output = std::process::Command::new("/usr/bin/vmmap")
        .args(["-summary", &pid.to_string()])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("vmmap could not inspect the worker"));
    }
    let summary = String::from_utf8_lossy(&output.stdout);
    summary
        .lines()
        .find_map(|line| line.trim().strip_prefix("Physical footprint:"))
        .and_then(parse_scaled_bytes)
        .ok_or_else(|| io::Error::other("vmmap omitted the physical footprint"))
}

#[cfg(target_os = "macos")]
fn private_memory_method() -> &'static str {
    MACOS_MEMORY_MEASUREMENT
}

#[cfg(windows)]
fn private_memory_bytes(pid: u32) -> io::Result<u64> {
    windows_powershell(&[
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!("(Get-Process -Id {pid}).PrivateMemorySize64"),
    ])
    .and_then(|value| value.parse().ok())
    .ok_or_else(|| io::Error::other("PowerShell could not read PrivateMemorySize64"))
}

#[cfg(windows)]
fn private_memory_method() -> &'static str {
    WINDOWS_MEMORY_MEASUREMENT
}

#[cfg(any(target_os = "macos", test))]
fn parse_scaled_bytes(value: &str) -> Option<u64> {
    let value = value.trim();
    let split = value.find(|character: char| !character.is_ascii_digit() && character != '.')?;
    let amount = value[..split].parse::<f64>().ok()?;
    let unit = value[split..].split_whitespace().next()?;
    let multiplier = match unit {
        "B" => 1.0,
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((amount * multiplier).round() as u64)
}

#[cfg(windows)]
fn windows_powershell(arguments: &[&str]) -> Option<String> {
    let output = std::process::Command::new("powershell.exe")
        .args(arguments)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// mini-agent-hdl3i diagnostic record for a sample that exhausted its host-resource retries.
/// Deliberately not a `BenchmarkReport`: this is a skip notice, not a measurement, and must
/// never be mistaken for reviewed evidence by `js_worker_resource_aggregate` or the checked-in
/// baseline validator.
#[cfg(target_os = "linux")]
#[derive(Serialize)]
struct HostResourceSkipRecord {
    schema_version: u32,
    benchmark: &'static str,
    status: &'static str,
    exit_class: Option<&'static str>,
    oom_kill_delta: u64,
    attempts: u32,
    cause: String,
}

#[cfg(target_os = "linux")]
fn write_host_resource_skip(
    output: &Path,
    exhausted: &HostResourceExhausted,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = HostResourceSkipRecord {
        schema_version: 1,
        benchmark: "mini-agent-js-worker",
        status: "host_resource_exhaustion_skip",
        exit_class: exhausted.exit_class,
        oom_kill_delta: exhausted.oom_kill_delta,
        attempts: exhausted.attempts,
        cause: exhausted.last_error.to_string(),
    };
    let json = serde_json::to_string_pretty(&record)?;
    fs::write(output, format!("{json}\n"))?;
    Ok(())
}

/// mini-agent-hdl3i diagnostic record for a sample that exhausted its retries on a
/// corroborated parent-initiated launch/handshake timeout. Deliberately not a `BenchmarkReport`
/// for the same reason as `HostResourceSkipRecord`: a skip notice is never reviewed evidence.
#[cfg(target_os = "linux")]
#[derive(Serialize)]
struct ParentTimeoutSkipRecord {
    schema_version: u32,
    benchmark: &'static str,
    status: &'static str,
    exit_class: Option<&'static str>,
    host_cpu_contention: Option<HostCpuContention>,
    step_timeout_seconds: u64,
    attempts: u32,
    cause: String,
}

#[cfg(target_os = "linux")]
fn write_parent_timeout_skip(
    output: &Path,
    skip: &ParentTimeoutSkip,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = ParentTimeoutSkipRecord {
        schema_version: 1,
        benchmark: "mini-agent-js-worker",
        status: "parent_timeout_under_contention_skip",
        exit_class: skip.exit_class,
        host_cpu_contention: skip.host_cpu_contention,
        step_timeout_seconds: skip.step_timeout_seconds,
        attempts: skip.attempts,
        cause: skip.last_error.to_string(),
    };
    let json = serde_json::to_string_pretty(&record)?;
    fs::write(output, format!("{json}\n"))?;
    Ok(())
}

/// mini-agent-hdl3i best-effort diagnostic record for an ordinary (non-host-resource) failure.
/// Written before the test still returns its real error, so the CI artifact-upload step finds
/// a file regardless of which `cargo test` invocation in the job step failed. A write failure
/// here is logged and swallowed: it must never replace or mask the original test failure.
///
/// The `phase`/`iteration`/`total_elapsed_seconds`/`worker_cpu_*` fields are present whenever
/// the failure happened inside one of the warm/ipc/recovery/cold sample loops (a
/// `SampleLoopFailure`); `exit_class`/`parent_timeout`/`host_cpu_contention`/`attempts`/
/// `memory_events_*` are present whenever that failure's cause was a classified worker kill (a
/// `ContainmentRegressionKill`) — `parent_timeout=true` means the PARENT gave up on its own
/// `STEP_TIMEOUT` watchdog before the worker's exit was ever observed; `parent_timeout=false`
/// with a populated `exit_class` means the worker's own exit status (a kernel-delivered signal,
/// for example) was observed. Every field is `None` for a failure from outside those loops,
/// leaving only `cause`.
#[cfg(target_os = "linux")]
#[derive(Serialize)]
struct BenchmarkFailureRecord {
    schema_version: u32,
    benchmark: &'static str,
    status: &'static str,
    cause: String,
    phase: Option<&'static str>,
    iteration: Option<usize>,
    total_elapsed_seconds: Option<f64>,
    worker_cpu_seconds_before_attempt: Option<f64>,
    worker_cpu_limit_seconds: Option<u64>,
    exit_class: Option<&'static str>,
    parent_timeout: Option<bool>,
    host_cpu_contention: Option<HostCpuContention>,
    attempts: Option<u32>,
    memory_events_before: Option<CgroupMemoryEvents>,
    memory_events_after: Option<CgroupMemoryEvents>,
}

#[cfg(target_os = "linux")]
fn write_benchmark_failure(output: &Path, error: &(dyn std::error::Error + 'static)) {
    let sample_loop = error.downcast_ref::<SampleLoopFailure>();
    let regression =
        sample_loop.and_then(|failure| failure.source.downcast_ref::<ContainmentRegressionKill>());
    let record = BenchmarkFailureRecord {
        schema_version: 1,
        benchmark: "mini-agent-js-worker",
        status: "failed",
        cause: error.to_string(),
        phase: sample_loop.map(|failure| failure.phase),
        iteration: sample_loop.map(|failure| failure.iteration),
        total_elapsed_seconds: sample_loop.map(|failure| failure.total_elapsed_seconds),
        worker_cpu_seconds_before_attempt: sample_loop
            .and_then(|failure| failure.worker_cpu_seconds_before_attempt),
        worker_cpu_limit_seconds: sample_loop.map(|failure| failure.worker_cpu_limit_seconds),
        exit_class: regression.and_then(|kill| kill.exit_class),
        parent_timeout: regression.map(|kill| kill.parent_timeout),
        host_cpu_contention: regression.and_then(|kill| kill.host_cpu_contention),
        attempts: regression.map(|kill| kill.attempts),
        memory_events_before: regression.and_then(|kill| kill.memory_events_before),
        memory_events_after: regression.and_then(|kill| kill.memory_events_after),
    };
    match serde_json::to_string_pretty(&record) {
        Ok(json) => {
            if let Err(write_error) = fs::write(output, format!("{json}\n")) {
                eprintln!(
                    "JS_WORKER_BENCHMARK_DIAGNOSTIC_WRITE_FAILED path={} error={write_error}",
                    output.display()
                );
            }
        }
        Err(serialize_error) => {
            eprintln!("JS_WORKER_BENCHMARK_DIAGNOSTIC_SERIALIZE_FAILED error={serialize_error}");
        }
    }
}

#[tokio::test]
#[ignore = "run explicitly on an OS reference host and upload the JSON artifact"]
async fn js_worker_resource_benchmark() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        std::env::var("MINI_AGENT_JS_WORKER_BENCH").as_deref(),
        Ok("1"),
        "set MINI_AGENT_JS_WORKER_BENCH=1 for an intentional reference run"
    );
    let mut report = benchmark_report_template();
    report.evidence_state = "single_platform_record".into();
    let worker_executable = benchmark_executable()?;
    let launcher = BenchmarkWorkerLauncher::new(worker_executable.clone());
    // Resolved once up front: every exit path below, including the mini-agent-hdl3i host-
    // resource skip and ordinary-failure diagnostics, writes to this same path so the CI
    // artifact-upload step always finds a file.
    let output = std::env::var_os("MINI_AGENT_JS_WORKER_BENCH_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "mini-agent-js-worker-benchmark-{}.json",
                uuid::Uuid::new_v4()
            ))
        });
    let evidence = match launcher.containment_status() {
        WorkerContainmentStatus::Available { backend, assurance } => {
            match run_production_benchmark(
                worker_executable,
                launcher,
                backend.to_string(),
                assurance_label(assurance),
            )
            .await
            {
                Ok(mut run) => {
                    if let Some(previous) = std::env::var_os("MINI_AGENT_JS_WORKER_BENCH_COMPARE") {
                        add_comparison(&mut run, Path::new(&previous))?;
                    }
                    PlatformEvidence::Measured { run: Box::new(run) }
                }
                // mini-agent-hdl3i: every resample attempt was corroborated host resource
                // exhaustion (a plain SIGKILL alongside this process's own cgroup recording a
                // fresh oom_kill). Record the skip and let the job stay green; a deliberate
                // containment regression never reaches this arm, since `classify_worker_kill`
                // requires cgroup-corroborated evidence of a plain `signal_kill`.
                #[cfg(target_os = "linux")]
                Err(error) if error.downcast_ref::<HostResourceExhausted>().is_some() => {
                    let exhausted = error
                        .downcast::<HostResourceExhausted>()
                        .expect("downcast_ref above just proved this succeeds");
                    write_host_resource_skip(&output, &exhausted)?;
                    println!(
                        "JS_WORKER_BENCHMARK_HOST_RESOURCE_SKIP output={}",
                        output.display()
                    );
                    return Ok(());
                }
                // mini-agent-hdl3i: every resample attempt was a parent-initiated launch/
                // handshake timeout (`WorkerError::TimedOut`, which `classify_worker_exit` never
                // produces) corroborated by host CPU contention measured at classification time.
                // Record the skip and let the job stay green; an uncorroborated timeout, or any
                // kernel-observed exit signal, never reaches this arm (see `classify_worker_kill`).
                #[cfg(target_os = "linux")]
                Err(error) if error.downcast_ref::<ParentTimeoutSkip>().is_some() => {
                    let skip = error
                        .downcast::<ParentTimeoutSkip>()
                        .expect("downcast_ref above just proved this succeeds");
                    write_parent_timeout_skip(&output, &skip)?;
                    println!(
                        "JS_WORKER_BENCHMARK_PARENT_TIMEOUT_SKIP output={}",
                        output.display()
                    );
                    return Ok(());
                }
                // Any other kill or transport error still fails the test: write a best-effort
                // diagnostic record first so the archive step finds a file either way, then
                // propagate the original failure so the job stays red.
                #[cfg(target_os = "linux")]
                Err(error) => {
                    write_benchmark_failure(&output, error.as_ref());
                    return Err(error);
                }
                #[cfg(not(target_os = "linux"))]
                Err(error) => return Err(error),
            }
        }
        WorkerContainmentStatus::Unavailable {
            backend, assurance, ..
        } => PlatformEvidence::ContainmentUnavailable {
            machine: Box::new(machine()),
            containment: Box::new(Containment {
                backend: backend.to_string(),
                assurance: assurance_label(assurance),
            }),
            reason_code: "containment_unavailable".into(),
        },
    };
    report.platform_evidence.push(evidence);
    let value = serde_json::to_value(&report)?;
    validate_report(&value)?;
    let json = serde_json::to_string_pretty(&report)?;
    fs::write(&output, format!("{json}\n"))?;
    println!("JS_WORKER_BENCHMARK_OUTPUT={}", output.display());
    println!("{json}");
    Ok(())
}

#[test]
#[ignore = "aggregate three downloaded per-OS artifacts explicitly"]
fn js_worker_resource_aggregate() -> Result<(), Box<dyn std::error::Error>> {
    let inputs = std::env::var_os("MINI_AGENT_JS_WORKER_BENCH_INPUTS")
        .ok_or("set MINI_AGENT_JS_WORKER_BENCH_INPUTS to three platform artifact paths")?;
    let reports = std::env::split_paths(&inputs)
        .map(fs::read)
        .map(|bytes| Ok(serde_json::from_slice::<BenchmarkReport>(&bytes?)?))
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    let aggregate = aggregate_reports(reports)?;
    let value = serde_json::to_value(&aggregate)?;
    validate_report(&value)?;
    let json = serde_json::to_string_pretty(&aggregate)?;
    let output = std::env::var_os("MINI_AGENT_JS_WORKER_BENCH_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("docs/benchmarks/results/js-worker-baseline.json"));
    fs::write(&output, format!("{json}\n"))?;
    println!("JS_WORKER_BENCHMARK_AGGREGATE_OUTPUT={}", output.display());
    println!("{json}");
    Ok(())
}

#[test]
fn worker_resource_statistics_use_nearest_rank_and_sample_variance() {
    let statistics = summarize_microseconds(&[1.0, 2.0, 3.0, 4.0, 100.0]);
    assert_eq!(statistics.samples, 5);
    assert_eq!(statistics.p95_us, 100.0);
    assert_eq!(statistics.variance_us2, 1_902.5);
}

#[test]
fn worker_resource_baseline_manifest_is_honest_and_schema_valid() {
    let baseline: Value = serde_json::from_str(include_str!(
        "../../../../docs/benchmarks/results/js-worker-baseline.json"
    ))
    .expect("checked-in worker baseline is JSON");
    validate_checked_in_baseline(&baseline).expect("checked-in worker baseline is truthful");
}

fn validate_checked_in_baseline(value: &Value) -> Result<(), String> {
    validate_report(value)?;
    let report: BenchmarkReport = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid checked-in baseline: {error}"))?;
    match (
        report.evidence_state.as_str(),
        report.platform_evidence.len(),
    ) {
        ("pending_external_runs", 0) | ("complete_for_recorded_platforms", 3) => Ok(()),
        _ => Err("checked-in baseline must be truthful pending evidence or a complete reviewed aggregate".into()),
    }
}

#[test]
fn worker_resource_comparison_records_variance_without_becoming_a_latency_gate() {
    let comparison = compare_statistics(
        &summarize_microseconds(&[100.0; 100]),
        &summarize_microseconds(&[111.0; 100]),
    );
    assert!((comparison.p95_relative_delta - 0.11).abs() < f64::EPSILON);
    assert!(comparison.within_documented_variance);

    let noisy = compare_statistics(
        &summarize_microseconds(&[100.0; 100]),
        &summarize_microseconds(&[140.0; 100]),
    );
    assert!(!noisy.within_documented_variance);

    let serialized_delta = 0.422_649_046_144_946_36;
    let recomputed_delta = (1_892.292_f64 - 3_277.542_f64).abs() / 3_277.542_f64;
    assert_ne!(serialized_delta, recomputed_delta);
    assert!(derived_float_matches(serialized_delta, recomputed_delta));
}

#[tokio::test]
async fn worker_resource_cancellation_observes_running_js_and_recovers_on_a_new_generation() {
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
        TestWorkerLauncher::internal_worker_process(),
    ));
    pure_call(&supervisor).await.unwrap();
    let initial_generation = supervisor.generation_for_test().await.unwrap();

    cancel_and_recover(supervisor.clone()).await.unwrap();

    assert!(supervisor.generation_for_test().await.unwrap() > initial_generation);
    supervisor.shutdown_for_test().await.unwrap();
}

#[test]
fn worker_resource_memory_units_are_parsed_deterministically() {
    assert_eq!(parse_scaled_bytes("12.5M"), Some(13_107_200));
    assert_eq!(parse_scaled_bytes(" 1024 K "), Some(1_048_576));
    assert_eq!(parse_scaled_bytes("unavailable"), None);
}

// mini-agent-hdl3i: pure classification over already-parsed `memory.events` content and
// closed exit-signal classes, with no filesystem or process access, so these run identically
// on every platform's `cargo test` regardless of whether cgroup v2 is mounted on this host.

#[test]
fn cgroup_memory_events_are_parsed_from_the_real_kernel_file_format() {
    let contents = "low 0\nhigh 0\nmax 0\noom 2\noom_kill 1\noom_group_kill 0\n";
    assert_eq!(
        parse_cgroup_memory_events(contents),
        CgroupMemoryEvents {
            oom: 2,
            oom_kill: 1
        }
    );
}

#[test]
fn cgroup_memory_events_parsing_ignores_unknown_counters_and_malformed_lines() {
    let contents = "oom_kill 3\ngarbage\nsome_future_counter not-a-number\n";
    assert_eq!(
        parse_cgroup_memory_events(contents),
        CgroupMemoryEvents {
            oom: 0,
            oom_kill: 3
        }
    );
    assert_eq!(
        parse_cgroup_memory_events(""),
        CgroupMemoryEvents::default()
    );
}

#[test]
fn worker_kill_classification_requires_cgroup_corroborated_oom_kill_for_a_plain_signal_kill() {
    let host_resource_exhaustion = WorkerKillEvidence {
        exit_class: Some("signal_kill"),
        cgroup_available: true,
        oom_kill_delta: 1,
        parent_timeout: false,
        host_cpu_contention: None,
    };
    assert_eq!(
        classify_worker_kill(&host_resource_exhaustion),
        WorkerKillCause::HostResourceExhaustion
    );
}

#[test]
fn worker_kill_classification_rejects_an_uncorroborated_plain_signal_kill() {
    let kill_without_oom_evidence = WorkerKillEvidence {
        exit_class: Some("signal_kill"),
        cgroup_available: true,
        oom_kill_delta: 0,
        parent_timeout: false,
        host_cpu_contention: None,
    };
    assert_eq!(
        classify_worker_kill(&kill_without_oom_evidence),
        WorkerKillCause::ContainmentRegression
    );

    let kill_without_cgroup_accounting = WorkerKillEvidence {
        exit_class: Some("signal_kill"),
        cgroup_available: false,
        oom_kill_delta: 0,
        parent_timeout: false,
        host_cpu_contention: None,
    };
    assert_eq!(
        classify_worker_kill(&kill_without_cgroup_accounting),
        WorkerKillCause::ContainmentRegression
    );
}

#[test]
fn worker_kill_classification_rejects_crash_signals_even_with_oom_evidence() {
    // A real crash (abort/segv/bus) must stay a containment regression even if the host
    // happened to be under memory pressure at the same time: only a plain SIGKILL is ever
    // ambiguous between "the host OOM-killer did this" and "containment let something escape".
    for exit_class in ["signal_abort", "signal_segv", "signal_bus", "other_signal"] {
        let evidence = WorkerKillEvidence {
            exit_class: Some(exit_class),
            cgroup_available: true,
            oom_kill_delta: 1,
            parent_timeout: false,
            host_cpu_contention: None,
        };
        assert_eq!(
            classify_worker_kill(&evidence),
            WorkerKillCause::ContainmentRegression,
            "{exit_class} must not be classified as host resource exhaustion"
        );
    }
}

#[test]
fn worker_kill_classification_rejects_a_failure_with_no_observed_exit() {
    let no_observed_exit = WorkerKillEvidence {
        exit_class: None,
        cgroup_available: true,
        oom_kill_delta: 1,
        parent_timeout: false,
        host_cpu_contention: None,
    };
    assert_eq!(
        classify_worker_kill(&no_observed_exit),
        WorkerKillCause::ContainmentRegression
    );
}

// mini-agent-hdl3i: pure threshold/classification logic for distinguishing a parent-initiated
// launch/handshake timeout under corroborated host CPU contention from an unexplained one. No
// filesystem or process access, so these run identically on every host.

#[test]
fn host_cpu_contention_requires_load_average_above_logical_cpus() {
    assert!(!host_cpu_is_contended(&HostCpuContention {
        load_average_1m: 2.0,
        logical_cpus: 4,
    }));
    // Exactly at the CPU count is not oversubscribed: every runnable task can have a core.
    assert!(!host_cpu_is_contended(&HostCpuContention {
        load_average_1m: 4.0,
        logical_cpus: 4,
    }));
    assert!(host_cpu_is_contended(&HostCpuContention {
        load_average_1m: 4.1,
        logical_cpus: 4,
    }));
    // An unreadable CPU count (0) can never corroborate contention, however high the load.
    assert!(!host_cpu_is_contended(&HostCpuContention {
        load_average_1m: 50.0,
        logical_cpus: 0,
    }));
}

#[test]
fn worker_kill_classification_treats_a_corroborated_parent_timeout_as_resampleable() {
    let evidence = WorkerKillEvidence {
        exit_class: None,
        cgroup_available: false,
        oom_kill_delta: 0,
        parent_timeout: true,
        host_cpu_contention: Some(HostCpuContention {
            load_average_1m: 50.0,
            logical_cpus: 4,
        }),
    };
    assert_eq!(
        classify_worker_kill(&evidence),
        WorkerKillCause::ParentTimeoutUnderContention
    );
}

#[test]
fn worker_kill_classification_rejects_an_uncorroborated_parent_timeout() {
    let timeout_without_contention = WorkerKillEvidence {
        exit_class: None,
        cgroup_available: false,
        oom_kill_delta: 0,
        parent_timeout: true,
        host_cpu_contention: Some(HostCpuContention {
            load_average_1m: 1.0,
            logical_cpus: 4,
        }),
    };
    assert_eq!(
        classify_worker_kill(&timeout_without_contention),
        WorkerKillCause::ContainmentRegression,
        "a parent timeout on an otherwise-idle host must not be resampled: it could be a hang"
    );

    let timeout_with_unreadable_load = WorkerKillEvidence {
        exit_class: None,
        cgroup_available: false,
        oom_kill_delta: 0,
        parent_timeout: true,
        host_cpu_contention: None,
    };
    assert_eq!(
        classify_worker_kill(&timeout_with_unreadable_load),
        WorkerKillCause::ContainmentRegression,
        "unreadable load-average evidence must never corroborate a parent timeout"
    );
}

#[test]
fn worker_kill_classification_prefers_parent_timeout_classification_over_exit_class() {
    // `parent_timeout` and a populated `exit_class` are mutually exclusive by construction
    // (classify_benchmark_exit never observes an exit status once `WorkerError::TimedOut` fired
    // first), but the classifier must still check `parent_timeout` first defensively.
    let evidence = WorkerKillEvidence {
        exit_class: Some("signal_kill"),
        cgroup_available: true,
        oom_kill_delta: 1,
        parent_timeout: true,
        host_cpu_contention: Some(HostCpuContention {
            load_average_1m: 1.0,
            logical_cpus: 4,
        }),
    };
    assert_eq!(
        classify_worker_kill(&evidence),
        WorkerKillCause::ContainmentRegression,
        "parent_timeout must gate on contention, not fall through to the oom_kill check"
    );
}

// mini-agent-hdl3i: pure parsing over an already-read `/proc/<pid>/stat` line, and pure
// context-attachment logic over already-classified errors. Neither touches the filesystem or
// a real process, so both run identically on every host regardless of Linux specifics.

#[cfg(target_os = "linux")]
#[test]
fn proc_stat_cpu_ticks_are_parsed_from_the_utime_and_stime_fields() {
    let stat = "123 (mini-agent) R 1 123 1 0 -1 0 0 0 0 0 4200 1300 0 0 20 0 1 0 987 0";
    assert_eq!(parse_proc_stat_cpu_ticks(stat), Some((4200, 1300)));
    // A `comm` field containing a literal ") " must not desynchronize the field split; anchor
    // on the LAST ") ", matching crate::tests::process_state's own stat parser.
    let tricky_comm = "41 (name with ) spaces) S 1 41 1 0 -1 0 0 0 0 0 10 5 0 0 0 0 0 0 0 0";
    assert_eq!(parse_proc_stat_cpu_ticks(tricky_comm), Some((10, 5)));
    assert_eq!(parse_proc_stat_cpu_ticks("too short"), None);
    assert_eq!(
        parse_proc_stat_cpu_ticks("41 (p) R 1 41 1 0 -1 0 0 0 0 0 notanumber 5 0 0 0 0 0 0 0 0"),
        None
    );
}

#[cfg(target_os = "linux")]
#[test]
fn sample_loop_failure_annotation_preserves_host_resource_exhaustion_for_the_existing_skip_path() {
    let exhausted = HostResourceExhausted {
        last_error: WorkerError::Transport,
        exit_class: Some("signal_kill"),
        oom_kill_delta: 1,
        attempts: 3,
    };
    let annotated = annotate_sample_loop_failure(
        Box::new(exhausted),
        "warm_pure_call",
        5,
        Duration::from_secs(12),
        Some(3.5),
    );
    assert!(
        annotated.downcast_ref::<HostResourceExhausted>().is_some(),
        "the top-level skip-and-record match must still see the exact HostResourceExhausted type"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn sample_loop_failure_annotation_wraps_every_other_failure_with_diagnostic_context() {
    let regression = ContainmentRegressionKill {
        source: WorkerError::Transport,
        exit_class: Some("signal_kill"),
        parent_timeout: false,
        host_cpu_contention: None,
        attempts: 1,
        memory_events_before: Some(CgroupMemoryEvents {
            oom: 0,
            oom_kill: 0,
        }),
        memory_events_after: Some(CgroupMemoryEvents {
            oom: 0,
            oom_kill: 0,
        }),
    };
    let annotated = annotate_sample_loop_failure(
        Box::new(regression),
        "broker_ipc_4kib",
        42,
        Duration::from_secs(30),
        Some(33.2),
    );
    let failure = annotated
        .downcast_ref::<SampleLoopFailure>()
        .expect("non-host-resource failures must gain sample-loop diagnostic context");
    assert_eq!(failure.phase, "broker_ipc_4kib");
    assert_eq!(failure.iteration, 42);
    assert_eq!(failure.total_elapsed_seconds, 30.0);
    assert_eq!(failure.worker_cpu_seconds_before_attempt, Some(33.2));
    assert_eq!(
        failure.worker_cpu_limit_seconds,
        crate::sandbox::worker::linux_worker_cpu_limit_seconds_for_test()
    );
    assert!(
        failure
            .source
            .downcast_ref::<ContainmentRegressionKill>()
            .is_some()
    );
}

#[test]
fn worker_resource_macos_process_proof_requires_one_guardian_helper() {
    let counts = CountMeasurements {
        maximum_observed_worker_processes: 1,
        maximum_observed_containment_helper_processes: 1,
        idle_worker_processes: 1,
        worker_process_observation_kind: MACOS_PROCESS_OBSERVATION_KIND.into(),
        worker_process_observation: MACOS_PROCESS_PROOF.into(),
        idle_runtimes: 0,
        idle_runtime_observation_kind: IDLE_RUNTIME_OBSERVATION_KIND.into(),
        idle_runtime_observation: IDLE_RUNTIME_PROOF.into(),
    };
    assert!(process_proof_matches("macos", &counts));
    assert!(helper_count_is_sane("macos", &counts));

    let mut missing_guardian = counts;
    missing_guardian.maximum_observed_containment_helper_processes = 0;
    assert!(!helper_count_is_sane("macos", &missing_guardian));
}

#[test]
fn worker_resource_windows_kernel_identity_requires_version_and_build() {
    assert_eq!(
        parse_windows_kernel_identity("Microsoft Windows Server 2025 Datacenter|10.0.26100|26100"),
        Some("Microsoft Windows Server 2025 Datacenter 10.0.26100 build 26100".into())
    );
    assert_eq!(parse_windows_kernel_identity("Windows_NT||26100"), None);
    assert_eq!(parse_windows_kernel_identity("Windows_NT"), None);
    assert!(windows_kernel_identity_is_specific(
        "Microsoft Windows Server 2025 Datacenter 10.0.26100 build 26100"
    ));
    assert!(!windows_kernel_identity_is_specific("Windows_NT"));
}

#[test]
fn worker_resource_schema_rejects_malformed_statistics_and_memory() {
    let valid = summarize_microseconds(&[10.0; SAMPLES]);
    validate_statistics(&valid).unwrap();

    let mut wrong_samples = valid.clone();
    wrong_samples.samples += 1;
    assert!(validate_statistics(&wrong_samples).is_err());

    let mut unordered = valid.clone();
    unordered.p95_us = unordered.maximum_us + 1.0;
    assert!(validate_statistics(&unordered).is_err());

    let mut inconsistent = valid;
    inconsistent.standard_deviation_us = 1.0;
    assert!(validate_statistics(&inconsistent).is_err());

    let mut impossible_variance = summarize_microseconds(&[10.0; SAMPLES]);
    impossible_variance.variance_us2 = 1.0;
    impossible_variance.standard_deviation_us = 1.0;
    assert!(validate_statistics(&impossible_variance).is_err());

    let valid_memory = MemoryMeasurement {
        samples: SAMPLES,
        mean_bytes: 1024.0,
        maximum_bytes: 2048,
        measurement: LINUX_MEMORY_MEASUREMENT.into(),
    };
    validate_memory_measurement("linux", &valid_memory).unwrap();

    let mut valid_macos_memory = valid_memory.clone();
    valid_macos_memory.measurement = MACOS_MEMORY_MEASUREMENT.into();
    validate_memory_measurement("macos", &valid_macos_memory).unwrap();

    let mut wrong_method = valid_memory.clone();
    wrong_method.measurement = WINDOWS_MEMORY_MEASUREMENT.into();
    assert!(validate_memory_measurement("linux", &wrong_method).is_err());

    let mut impossible_mean = valid_memory;
    impossible_mean.mean_bytes = 4096.0;
    assert!(validate_memory_measurement("linux", &impossible_mean).is_err());
}

fn unavailable_report_for_test(os: &str) -> BenchmarkReport {
    let mut report = benchmark_report_template();
    report.evidence_state = "single_platform_record".into();
    report
        .platform_evidence
        .push(PlatformEvidence::ContainmentUnavailable {
            machine: Box::new(Machine {
                host: "reference".into(),
                os: os.into(),
                arch: "x86_64".into(),
                kernel: "reference-kernel".into(),
                cpu: "reference-cpu".into(),
                logical_cpus: 4,
                memory_bytes: 8 * 1024 * 1024 * 1024,
                binary_profile: "debug".into(),
                package_version: env!("CARGO_PKG_VERSION").into(),
                build_identity: crate::extras::js::protocol::BuildIdentity::current()
                    .as_str()
                    .into(),
            }),
            containment: Box::new(Containment {
                backend: match os {
                    "linux" => "bubblewrap",
                    "macos" => "seatbelt",
                    "windows" => "windows-lpac",
                    _ => "invalid",
                }
                .into(),
                assurance: if os == "macos" {
                    "deprecated-best-effort"
                } else {
                    "enforced"
                }
                .into(),
            }),
            reason_code: "containment_unavailable".into(),
        });
    report
}

#[test]
fn worker_resource_aggregation_requires_one_truthful_record_per_os() {
    let aggregate = aggregate_reports(vec![
        unavailable_report_for_test("windows"),
        unavailable_report_for_test("linux"),
        unavailable_report_for_test("macos"),
    ])
    .expect("three unique platform records aggregate");
    assert_eq!(aggregate.evidence_state, "complete_for_recorded_platforms");
    assert_eq!(
        aggregate
            .platform_evidence
            .iter()
            .map(PlatformEvidence::operating_system)
            .collect::<Vec<_>>(),
        ["linux", "macos", "windows"]
    );
    let aggregate_value = serde_json::to_value(aggregate).unwrap();
    validate_report(&aggregate_value).unwrap();
    validate_checked_in_baseline(&aggregate_value)
        .expect("a complete three-platform aggregate is an admissible checked-in baseline");

    let duplicate = aggregate_reports(vec![
        unavailable_report_for_test("linux"),
        unavailable_report_for_test("linux"),
        unavailable_report_for_test("windows"),
    ]);
    assert!(duplicate.is_err());
}

#[test]
fn worker_resource_schema_rejects_policy_drift_and_unavailable_measurements() {
    let baseline: Value = serde_json::from_str(include_str!(
        "../../../../docs/benchmarks/results/js-worker-baseline.json"
    ))
    .unwrap();
    let mut target_drift = baseline.clone();
    target_drift["targets"]["warm_pure_call_us"] = Value::from(20_000.0);
    assert!(validate_report(&target_drift).is_err());

    let mut sampling_drift = baseline;
    sampling_drift["sampling"]["samples"] = Value::from(101);
    assert!(validate_report(&sampling_drift).is_err());

    let mut unavailable = serde_json::to_value(unavailable_report_for_test("macos")).unwrap();
    unavailable["platform_evidence"][0]["latency"] = serde_json::json!({"p95_us": 1.0});
    assert!(
        validate_report(&unavailable).is_err(),
        "status-only unavailable evidence must reject fabricated measurements"
    );
}
