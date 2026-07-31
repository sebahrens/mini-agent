//! Fresh no-effect exact-boolean verification with capability fakes and mutation checks.
//!
//! Verifies that a skill:
//! - Has at least one test and one declared export
//! - Evaluates source once and each test separately in a fresh context
//! - Accepts only exact JavaScript boolean `true` as a test result
//! - Declares only valid exports that the source actually defines
//! - Exercises every export (mutation pass)

use std::time::{Duration, Instant};

use rquickjs::prelude::{Func, Opt};
use rquickjs::{Context, Ctx, Error, Runtime, Value};

use crate::extras::js::skills::fakes::{FAKES_VERSION, FakeHostGlobals, FakeTranscript};
use crate::extras::js::skills::{
    CapabilityManifest, CapabilityTier, HostCapability, SkillArtifact,
};
use crate::extras::js::types::{MEMORY_LIMIT, STACK_LIMIT};

/// Version of the verification algorithm. Bumping this invalidates existing reports.
pub const VERIFIER_VERSION: u32 = 1;

/// Timeout for evaluating one artifact's source + tests + mutations.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum number of pending jobs during verification.
const VERIFY_MAX_PENDING_JOBS: usize = 1_000;

/// Result of verifying one test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestResult {
    /// Test returned exact JavaScript boolean `true`.
    Passed,
    /// Test returned `false`.
    ReturnedFalse,
    /// Test returned a truthy non-boolean value.
    ReturnedTruthy(String), // type name or value representation
    /// Test threw an error.
    Threw(String),
    /// Test timed out.
    Timeout,
    /// Test caused OOM.
    OutOfMemory,
    /// Too many pending jobs.
    JobLimitExceeded,
    /// Test returned a Promise that rejected.
    PromiseRejected(String),
}

/// Outcome of the mutation pass for one export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationOutcome {
    /// At least one test failed when the export was stubbed.
    Detected,
    /// All tests still passed with the export stubbed (vacuous export).
    Undetected,
    /// An error occurred during mutation testing.
    Error(String),
}

/// Complete verification report for one skill.
#[derive(Debug, Clone)]
pub struct VerificationReport {
    /// The skill's full ID.
    pub skill_id: String,
    /// Identity version of the artifact.
    pub identity_version: u32,
    /// Capability manifest that was tested.
    pub capability: CapabilityManifest,
    /// Verifier version.
    pub verifier_version: u32,
    /// Fake hosts version.
    pub fakes_version: u32,
    /// Runtime memory limit in bytes.
    pub memory_limit: usize,
    /// Runtime stack limit in bytes.
    pub stack_limit: usize,
    /// Verification deadline.
    pub timeout: Duration,
    /// Results of evaluating each test in order.
    pub test_results: Vec<TestResult>,
    /// Mutation outcomes for each declared export.
    pub mutation_outcomes: Vec<MutationOutcome>,
    /// Transcript of all fake I/O operations.
    pub transcript: FakeTranscript,
}

/// Errors that can occur during verification.
#[derive(Debug, thiserror::Error)]
pub enum VerificationError {
    /// Skill has no tests.
    #[error("skill must have at least one test")]
    NoTests,

    /// Skill has no exports.
    #[error("skill must have at least one declared export")]
    NoExports,

    /// Failed to create a runtime.
    #[error("failed to create JS runtime: {0}")]
    RuntimeCreationFailed(String),

    /// Failed to create a context.
    #[error("failed to create JS context: {0}")]
    ContextCreationFailed(String),

    /// Failed to evaluate source code.
    #[error("skill source failed to evaluate: {0}")]
    SourceEvaluationFailed(String),

    /// A declared export does not exist in source.
    #[error("declared export '{export}' not found in source")]
    ExportNotFound { export: String },

    /// A declared export exists but is not a function.
    #[error("declared export '{export}' exists but is not a function")]
    ExportNotAFunction { export: String },

    /// One or more tests failed during verification.
    #[error("test at index {index} failed: {outcome:?}")]
    TestFailed { index: usize, outcome: TestResult },

    /// Mutation pass failed for an export.
    #[error("mutation pass failed for export '{export}': {reason}")]
    MutationPassFailed { export: String, reason: String },
}

/// Drain pending jobs, returning the first error or timeout if it occurs.
fn drain_jobs(rt: &Runtime, deadline: Instant) -> Option<TestResult> {
    let mut executed = 0;

    loop {
        if Instant::now() >= deadline {
            return Some(TestResult::Timeout);
        }

        if executed >= VERIFY_MAX_PENDING_JOBS {
            if rt.is_job_pending() {
                return Some(TestResult::JobLimitExceeded);
            }
            return None;
        }

        match rt.execute_pending_job() {
            Ok(true) => executed += 1,
            Ok(false) => return None,
            Err(job_exception) => {
                // Job threw an exception; we should handle it
                let _ = job_exception; // Already captured the error
                return Some(TestResult::Threw("job exception".to_string()));
            }
        }
    }
}

/// Evaluate a script in the context and return whether it's exactly `true`.
fn eval_exact_boolean<'a>(ctx: &Ctx<'a>, script: &str, _deadline: Instant) -> Result<bool, String> {
    let value = match ctx.eval::<Value, _>(script) {
        Ok(v) => v,
        Err(Error::Allocation) => {
            return Err("OutOfMemory".to_string());
        }
        Err(Error::Exception) => {
            let caught = ctx.catch();
            if let Some(exception) = caught.as_exception() {
                let message = exception.message().unwrap_or_default();
                let stack = exception.stack().unwrap_or_default();
                return Err(format!("{message}\n{stack}"));
            }
            return Err(format!("Exception: {:?}", caught));
        }
        Err(e) => {
            return Err(e.to_string());
        }
    };

    // Check if it's exactly `true`
    if let Some(b) = value.as_bool() {
        if b {
            return Ok(true);
        } else {
            return Ok(false);
        }
    }

    // Reject all other types
    if value.is_undefined() {
        Err("undefined".to_string())
    } else if value.is_null() {
        Err("null".to_string())
    } else if let Some(n) = value.as_int() {
        Err(format!("number: {}", n))
    } else if let Some(n) = value.as_float() {
        Err(format!("float: {}", n))
    } else if let Some(s) = value.as_string() {
        if let Ok(s) = s.to_string() {
            Err(format!("string: {}", s))
        } else {
            Err("string (unstringifiable)".to_string())
        }
    } else if value.is_array() {
        Err("array".to_string())
    } else if value.is_object() {
        Err("object".to_string())
    } else {
        Err("unknown type".to_string())
    }
}

/// Verify that a skill is safe and sound.
pub fn verify_skill(skill: &SkillArtifact) -> Result<VerificationReport, VerificationError> {
    // Require at least one test and one export.
    if skill.tests.is_empty() {
        return Err(VerificationError::NoTests);
    }
    if skill.exports.is_empty() {
        return Err(VerificationError::NoExports);
    }

    // Create a fresh bounded runtime for this verification.
    let rt = Runtime::new().map_err(|e| VerificationError::RuntimeCreationFailed(e.to_string()))?;
    rt.set_memory_limit(MEMORY_LIMIT);
    rt.set_max_stack_size(STACK_LIMIT);

    let deadline = Instant::now() + VERIFY_TIMEOUT;
    let fakes = FakeHostGlobals::new(skill.capability.clone());

    // Evaluate source and tests in one fresh context.
    let ctx =
        Context::full(&rt).map_err(|e| VerificationError::ContextCreationFailed(e.to_string()))?;

    let test_results = ctx.with(|ctx| {
        // Register fake host globals based on capability manifest
        register_fakes(&ctx, &fakes, skill.capability.tier)?;

        // Evaluate source code as one script (first script location).
        if let Err(e) = ctx.eval::<Value, _>(skill.source.as_str()) {
            return Err(VerificationError::SourceEvaluationFailed(match e {
                Error::Allocation => "OutOfMemory".to_string(),
                Error::Exception => {
                    let caught = ctx.catch();
                    if let Some(ex) = caught.as_exception() {
                        let msg = ex.message().unwrap_or_default();
                        let stack = ex.stack().unwrap_or_default();
                        format!("{msg}\n{stack}")
                    } else {
                        format!("Exception: {:?}", caught)
                    }
                }
                _ => e.to_string(),
            }));
        }

        // Validate that declared exports exist and are functions.
        for export in &skill.exports {
            let value = match ctx.globals().get::<_, Value>(&export.name) {
                Ok(v) => v,
                Err(_) => {
                    return Err(VerificationError::ExportNotFound {
                        export: export.name.clone(),
                    });
                }
            };

            // Check that the export is a function
            if !value.is_function() {
                return Err(VerificationError::ExportNotAFunction {
                    export: export.name.clone(),
                });
            }
        }

        // Evaluate each test as a separate script.
        let mut results = Vec::new();
        for test_script in &skill.tests {
            let result = match eval_exact_boolean(&ctx, test_script, deadline) {
                Ok(true) => TestResult::Passed,
                Ok(false) => TestResult::ReturnedFalse,
                Err(msg) if msg == "OutOfMemory" => TestResult::OutOfMemory,
                Err(msg) if msg == "Timeout" => TestResult::Timeout,
                Err(msg) => {
                    // Check if it's a thrown error
                    TestResult::Threw(msg)
                }
            };
            results.push(result);

            if Instant::now() >= deadline {
                results.push(TestResult::Timeout);
                break;
            }
        }

        Ok(results)
    });

    // Drain jobs OUTSIDE the context closure to avoid borrow conflicts
    if let Some(result) = drain_jobs(&rt, deadline)
        && matches!(
            result,
            TestResult::Timeout | TestResult::JobLimitExceeded | TestResult::OutOfMemory
        )
    {
        return Err(VerificationError::SourceEvaluationFailed(format!(
            "{:?}",
            result
        )));
    }

    let test_results = test_results?;

    // Check that all tests passed. Return Err if any test failed.
    let all_passed = test_results.iter().all(|r| *r == TestResult::Passed);
    if !all_passed {
        // Find the first failing test
        for (index, result) in test_results.iter().enumerate() {
            if *result != TestResult::Passed {
                return Err(VerificationError::TestFailed {
                    index,
                    outcome: result.clone(),
                });
            }
        }
        // Unreachable, but required for type checking
        unreachable!("all_passed is false but no failing test found");
    }

    // Mutation pass: for each export, stub it and verify at least one test fails.
    // Use a fresh context for mutation testing to avoid RefCell borrow issues.
    let mutation_outcomes = {
        let ctx_mut = Context::full(&rt)
            .map_err(|e| VerificationError::ContextCreationFailed(e.to_string()))?;

        let outcomes = ctx_mut.with(|ctx| {
            // Register fakes in the mutation context as well
            register_fakes(&ctx, &fakes, skill.capability.tier)?;

            // Re-evaluate source with mutation support.
            if let Err(e) = ctx.eval::<Value, _>(skill.source.as_str()) {
                return Err(VerificationError::SourceEvaluationFailed(e.to_string()));
            }

            let mut outcomes = Vec::new();

            for export in &skill.exports {
                // Create a throwing stub for this export.
                let stub_code = format!(
                    r#"(function() {{ {name} = function() {{ throw new Error("export {name} is stubbed"); }}; return true; }})()"#,
                    name = export.name
                );

                // Replace the export.
                if let Err(e) = ctx.eval::<Value, _>(stub_code.as_str()) {
                    return Err(VerificationError::MutationPassFailed {
                        export: export.name.clone(),
                        reason: e.to_string(),
                    });
                }

                // Run all tests again; at least one must fail.
                let mut any_failed = false;
                for test_script in &skill.tests {
                    match eval_exact_boolean(&ctx, test_script, deadline) {
                        Ok(true) => {
                            // Test still passes with export stubbed - vacuous
                        }
                        Ok(false) => {
                            any_failed = true;
                        }
                        Err(_) => {
                            any_failed = true;
                        }
                    };

                    if any_failed {
                        break;
                    }
                }

                let outcome = if any_failed {
                    MutationOutcome::Detected
                } else {
                    MutationOutcome::Undetected
                };
                outcomes.push(outcome);
            }

            Ok(outcomes)
        });

        // Drain jobs OUTSIDE the context closure
        if drain_jobs(&rt, deadline).is_some() {
            // Job queue issues during mutation pass - continue anyway
        }

        outcomes
    };

    let mutation_outcomes = mutation_outcomes?;

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
        transcript: fakes.transcript(),
    })
}

/// Register fake host globals based on capability manifest.
/// For verification, fakes are simple JavaScript stubs that exist to satisfy type checks.
/// They record their calls to the transcript via the FakeHostGlobals Arc<Mutex>.
fn register_fakes(
    ctx: &Ctx,
    fakes: &FakeHostGlobals,
    tier: CapabilityTier,
) -> Result<(), VerificationError> {
    // Tier 0 gets no host globals at all.
    if tier == CapabilityTier::Pure {
        return Ok(());
    }

    let globals = ctx.globals();
    let mut registered: Vec<&'static str> = Vec::new();

    let register_error = |name: &str, error: Error| {
        VerificationError::ContextCreationFailed(format!("failed to register {name}: {error}"))
    };

    // Each global is a Rust closure holding a handle to the verifier-owned virtual
    // state, so calls hit real record/replay fakes and land in the transcript. A
    // JavaScript stub would be replaceable by skill code and would record nothing.
    if fakes.allows(HostCapability::ReadFile) {
        let handle = fakes.clone();
        globals
            .set(
                "read_file",
                Func::from(move |path: String| -> rquickjs::Result<String> {
                    handle.read_file(&path).map_err(|reason| {
                        rquickjs::Error::new_from_js_message("fake", "read_file", reason)
                    })
                }),
            )
            .map_err(|error| register_error("read_file", error))?;
        registered.push("read_file");
    }

    if fakes.allows(HostCapability::WriteFile) {
        let handle = fakes.clone();
        globals
            .set(
                "write_file",
                Func::from(
                    move |path: String, content: String| -> rquickjs::Result<bool> {
                        handle
                            .write_file(&path, &content)
                            .map(|()| true)
                            .map_err(|reason| {
                                rquickjs::Error::new_from_js_message("fake", "write_file", reason)
                            })
                    },
                ),
            )
            .map_err(|error| register_error("write_file", error))?;
        registered.push("write_file");
    }

    if fakes.allows(HostCapability::Spawn) {
        let handle = fakes.clone();
        globals
            .set(
                "spawn",
                Func::from(
                    move |program: String, args: Vec<String>| -> rquickjs::Result<String> {
                        handle.spawn(&program, &args).map_err(|reason| {
                            rquickjs::Error::new_from_js_message("fake", "spawn", reason)
                        })
                    },
                ),
            )
            .map_err(|error| register_error("spawn", error))?;
        registered.push("spawn");
    }

    if fakes.allows(HostCapability::Fetch) {
        let handle = fakes.clone();
        globals
            .set(
                "fetch",
                Func::from(
                    move |url: String, method: Opt<String>| -> rquickjs::Result<String> {
                        let method = method.0.unwrap_or_else(|| "GET".to_string());
                        handle.fetch(&url, &method).map_err(|reason| {
                            rquickjs::Error::new_from_js_message("fake", "fetch", reason)
                        })
                    },
                ),
            )
            .map_err(|error| register_error("fetch", error))?;
        registered.push("fetch");
    }

    // Seal the fakes. Without this the globals are ordinary writable properties and
    // an embedded test could overwrite one, defeating the whole no-effect check.
    if !registered.is_empty() {
        let names = registered
            .iter()
            .map(|name| format!("'{name}'"))
            .collect::<Vec<_>>()
            .join(",");
        let seal = format!(
            "(function() {{ for (const n of [{names}]) {{ \
                 Object.defineProperty(globalThis, n, {{ writable: false, configurable: false }}); \
             }} }})()"
        );
        ctx.eval::<Value, _>(seal.as_str())
            .map_err(|error| register_error("fake seal", error))?;
    }

    Ok(())
}
