//! Parent adapter for worker-owned production-loader verification.
//!
//! This module owns report construction only. QuickJS runtime, realm loading, capability-object
//! construction, deterministic fake execution, and source evaluation all live in the contained
//! worker and use the same loader as production execution.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::extras::js::protocol::{
    Diagnostic, DiagnosticClass, DiagnosticStage, ScriptRole, VerificationCase,
    VerificationCaseKind, VerificationExpectedValue, VerificationResult, VerifyArtifact,
};
use crate::extras::js::supervisor::{JsWorkerSupervisor, WorkerError};
use crate::extras::js::types::{MEMORY_LIMIT, STACK_LIMIT};

use super::fakes::{FAKES_VERSION, FakeTranscript};
use super::held_out::ExpectedJsValue;
use super::{CapabilityManifest, SkillArtifact};

/// Version of the verification algorithm. Bumping this invalidates existing reports.
pub const VERIFIER_VERSION: u32 = 3;

/// Timeout for one whole worker verification request.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);
const VERIFICATION_LOADER_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestResult {
    Passed,
    ReturnedFalse,
    Threw(String),
    Timeout,
    OutOfMemory,
    JobLimitExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationOutcome {
    Detected,
    Undetected,
}

#[derive(Debug, Clone)]
pub struct VerificationReport {
    pub skill_id: String,
    pub identity_version: u32,
    pub capability: CapabilityManifest,
    pub verifier_version: u32,
    pub fakes_version: u32,
    pub memory_limit: usize,
    pub stack_limit: usize,
    pub timeout: Duration,
    pub test_results: Vec<TestResult>,
    pub mutation_outcomes: Vec<MutationOutcome>,
    pub transcript: FakeTranscript,
}

#[derive(Debug, thiserror::Error)]
pub enum VerificationError {
    #[error("skill must have at least one test")]
    NoTests,
    #[error("skill must have at least one declared export")]
    NoExports,
    #[error("failed to create JS runtime: {0}")]
    RuntimeCreationFailed(String),
    #[error("failed to create JS context: {0}")]
    ContextCreationFailed(String),
    #[error("verification infrastructure is temporarily unavailable: {0}")]
    InfrastructureUnavailable(String),
    #[error("skill source failed to evaluate: {0}")]
    SourceEvaluationFailed(String),
    #[error("declared export '{export}' not found in source")]
    ExportNotFound { export: String },
    #[error("declared export '{export}' exists but is not a function")]
    ExportNotAFunction { export: String },
    #[error("test at index {index} failed: {outcome:?}")]
    TestFailed { index: usize, outcome: TestResult },
    #[error("mutation pass failed for export '{export}': {reason}")]
    MutationPassFailed { export: String, reason: String },
    #[error("held-out expected value mismatch")]
    HeldOutExpectedMismatch,
    #[error("invalid held-out fake fixture: {0}")]
    FakeFixtureInvalid(String),
}

pub fn verify_skill(skill: &SkillArtifact) -> Result<VerificationReport, VerificationError> {
    if skill.tests.is_empty() {
        return Err(VerificationError::NoTests);
    }
    if skill.exports.is_empty() {
        return Err(VerificationError::NoExports);
    }
    let embedded_count = skill.tests.len();
    let mut cases = skill
        .tests
        .iter()
        .enumerate()
        .map(|(index, script)| VerificationCase {
            case_id: format!("embedded-{index}"),
            script: script.clone(),
            kind: VerificationCaseKind::Embedded,
        })
        .collect::<Vec<_>>();
    cases.extend(skill.exports.iter().map(|export| VerificationCase {
        case_id: format!("mutation-{}", export.name),
        script: String::new(),
        kind: VerificationCaseKind::Mutation {
            export_name: export.name.clone(),
        },
    }));
    let result = verify_in_worker(VerifyArtifact {
        artifact: skill.clone(),
        cases,
    })?;
    validate_worker_result(&result, embedded_count + skill.exports.len())?;

    if let Some(source_failure) = result.cases.iter().find(|case| {
        !case.passed
            && case
                .diagnostic
                .as_ref()
                .is_some_and(|diagnostic| diagnostic.script_role == ScriptRole::SkillSource)
    }) {
        return Err(VerificationError::SourceEvaluationFailed(
            closed_diagnostic(source_failure.diagnostic.as_ref().unwrap()),
        ));
    }

    let mut transcript = FakeTranscript::default();
    let mut test_results = Vec::with_capacity(embedded_count);
    for (index, case) in result.cases[..embedded_count].iter().enumerate() {
        transcript.append(case.transcript.clone());
        let outcome = if case.passed {
            TestResult::Passed
        } else {
            test_result(case.diagnostic.as_ref())
        };
        if outcome != TestResult::Passed {
            return Err(VerificationError::TestFailed { index, outcome });
        }
        test_results.push(outcome);
    }

    let mut mutation_outcomes = Vec::with_capacity(skill.exports.len());
    for (export, case) in skill.exports.iter().zip(&result.cases[embedded_count..]) {
        if !case.passed {
            return Err(VerificationError::MutationPassFailed {
                export: export.name.clone(),
                reason: case
                    .diagnostic
                    .as_ref()
                    .map(closed_diagnostic)
                    .unwrap_or_else(|| "mutation was not detected".to_string()),
            });
        }
        mutation_outcomes.push(MutationOutcome::Detected);
    }

    Ok(VerificationReport {
        skill_id: skill.id.clone(),
        identity_version: skill.identity_version,
        capability: skill.capability.clone(),
        verifier_version: VERIFIER_VERSION,
        fakes_version: FAKES_VERSION,
        memory_limit: MEMORY_LIMIT,
        stack_limit: STACK_LIMIT,
        timeout: VERIFY_TIMEOUT,
        test_results,
        mutation_outcomes,
        transcript,
    })
}

pub(crate) fn verify_inherited_cases(
    skill: &SkillArtifact,
    scripts: &[String],
) -> Result<(), VerificationError> {
    let cases = scripts
        .iter()
        .enumerate()
        .map(|(index, script)| VerificationCase {
            case_id: format!("inherited-{index}"),
            script: script.clone(),
            kind: VerificationCaseKind::Inherited,
        })
        .collect::<Vec<_>>();
    let result = verify_in_worker(VerifyArtifact {
        artifact: skill.clone(),
        cases,
    })?;
    validate_worker_result(&result, scripts.len())?;

    for (index, case) in result.cases.iter().enumerate() {
        if case.passed {
            continue;
        }
        if case
            .diagnostic
            .as_ref()
            .is_some_and(|diagnostic| diagnostic.script_role == ScriptRole::SkillSource)
        {
            return Err(VerificationError::SourceEvaluationFailed(
                closed_diagnostic(case.diagnostic.as_ref().unwrap()),
            ));
        }
        return Err(VerificationError::TestFailed {
            index,
            outcome: test_result(case.diagnostic.as_ref()),
        });
    }
    Ok(())
}

pub(crate) fn verify_held_out_case(
    skill: &SkillArtifact,
    expression: &str,
    expected: &ExpectedJsValue,
    fake_files: &BTreeMap<String, String>,
) -> Result<FakeTranscript, VerificationError> {
    let result = verify_in_worker(VerifyArtifact {
        artifact: skill.clone(),
        cases: vec![VerificationCase {
            case_id: "held-out-0".to_string(),
            script: expression.to_string(),
            kind: VerificationCaseKind::HeldOut {
                expected: expected.into(),
                fake_files: fake_files.clone(),
            },
        }],
    })?;
    validate_worker_result(&result, 1)?;
    let case = &result.cases[0];
    if case.passed {
        Ok(case.transcript.clone())
    } else if case
        .diagnostic
        .as_ref()
        .is_some_and(|diagnostic| diagnostic.script_role == ScriptRole::SkillSource)
    {
        Err(VerificationError::SourceEvaluationFailed(
            closed_diagnostic(case.diagnostic.as_ref().unwrap()),
        ))
    } else {
        Err(VerificationError::HeldOutExpectedMismatch)
    }
}

fn verify_in_worker(request: VerifyArtifact) -> Result<VerificationResult, VerificationError> {
    #[cfg(test)]
    let _test_serial = {
        static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        TEST_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    };
    #[cfg(test)]
    let supervisor = {
        static TEST_VERIFICATION_SUPERVISOR: std::sync::OnceLock<
            std::sync::Arc<JsWorkerSupervisor>,
        > = std::sync::OnceLock::new();
        TEST_VERIFICATION_SUPERVISOR
            .get_or_init(|| {
                std::sync::Arc::new(JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
                    crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
                    VERIFY_TIMEOUT,
                ))
            })
            .clone()
    };
    #[cfg(not(test))]
    let supervisor = JsWorkerSupervisor::shared();
    let outcome = if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            scope
                .spawn(|| supervisor.verify_blocking(request))
                .join()
                .map_err(|_| WorkerError::Transport)?
        })
    } else {
        supervisor.verify_blocking(request)
    };
    outcome.map_err(worker_error)
}

pub(crate) fn worker_error(error: WorkerError) -> VerificationError {
    if error.is_retryable_admission_infrastructure() {
        return VerificationError::InfrastructureUnavailable("worker queue unavailable".into());
    }
    match error {
        WorkerError::Cancelled => {
            VerificationError::InfrastructureUnavailable("worker verification cancelled".into())
        }
        WorkerError::ContainmentUnavailable
        | WorkerError::Launch
        | WorkerError::Transport
        | WorkerError::Protocol
        | WorkerError::BuildMismatch
        | WorkerError::EffectOutcomeUnknown
        | WorkerError::StaleGeneration
        | WorkerError::IdentityExhausted
        | WorkerError::BlockingVerifyInAsyncRuntime => {
            VerificationError::InfrastructureUnavailable("worker unavailable".into())
        }
        WorkerError::TimedOut => VerificationError::SourceEvaluationFailed("timeout".to_string()),
        WorkerError::NativeCpuLimit => {
            VerificationError::SourceEvaluationFailed("native CPU resource limit".to_string())
        }
        WorkerError::UnexpectedVerificationEffect => {
            VerificationError::SourceEvaluationFailed("external effect denied".to_string())
        }
        WorkerError::VerificationQueueFull | WorkerError::VerificationQueueClosed => {
            unreachable!("verification queue failures return above")
        }
    }
}

fn validate_worker_result(
    result: &VerificationResult,
    expected_cases: usize,
) -> Result<(), VerificationError> {
    if result.loader_version != VERIFICATION_LOADER_VERSION || result.cases.len() != expected_cases
    {
        return Err(VerificationError::RuntimeCreationFailed(
            "worker verification contract mismatch".to_string(),
        ));
    }
    Ok(())
}

fn test_result(diagnostic: Option<&Diagnostic>) -> TestResult {
    let Some(diagnostic) = diagnostic else {
        return TestResult::Threw("verification failed".to_string());
    };
    if diagnostic.stage == DiagnosticStage::JobDrain {
        TestResult::JobLimitExceeded
    } else if diagnostic.class == DiagnosticClass::ResourceLimit {
        TestResult::Timeout
    } else if diagnostic.class == DiagnosticClass::Contract {
        TestResult::ReturnedFalse
    } else {
        TestResult::Threw(closed_diagnostic(diagnostic))
    }
}

fn closed_diagnostic(diagnostic: &Diagnostic) -> String {
    format!(
        "{:?}/{:?}/{:?}",
        diagnostic.class, diagnostic.stage, diagnostic.script_role
    )
}

impl From<&ExpectedJsValue> for VerificationExpectedValue {
    fn from(value: &ExpectedJsValue) -> Self {
        match value {
            ExpectedJsValue::Boolean(value) => Self::Boolean(*value),
            ExpectedJsValue::String(value) => Self::String(value.clone()),
            ExpectedJsValue::Integer(value) => Self::Integer(*value),
            ExpectedJsValue::Float(value) => Self::Float(*value),
            ExpectedJsValue::Null => Self::Null,
        }
    }
}
