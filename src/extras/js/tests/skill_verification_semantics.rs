//! Comprehensive test suite for skill verification semantics.
//!
//! Tests cover:
//! - Exact boolean acceptance/rejection
//! - All JS value types
//! - Error, timeout, OOM, and job-limit conditions
//! - Capability tier matrices
//! - Export validation
//! - Mutation detection per export
//! - Fresh state between verifications

#[cfg(test)]
mod tests {
    use crate::extras::js::skills::verify::{
        MutationOutcome, TestResult, VerificationError, verify_skill, worker_error,
    };
    use crate::extras::js::skills::{
        CapabilityManifest, CapabilityTier, HostCapability, SkillArtifact, SkillExport,
        test_manifest,
    };
    use crate::extras::js::supervisor::WorkerError;

    // Helper to create a skill.
    fn skill(
        source: &str,
        tests: Vec<&str>,
        exports: Vec<(&str, &str)>,
        tier: CapabilityTier,
        capabilities: Vec<HostCapability>,
    ) -> SkillArtifact {
        let capability = test_manifest(tier, capabilities).unwrap();
        let exports_vec = exports
            .into_iter()
            .map(|(name, sig)| SkillExport {
                name: name.to_string(),
                signature: sig.to_string(),
            })
            .collect();
        SkillArtifact::new(
            source.to_string(),
            "test skill".to_string(),
            vec![],
            exports_vec,
            tests.into_iter().map(|t| t.to_string()).collect(),
            capability,
        )
        .unwrap()
    }

    #[test]
    fn test_exact_boolean_true_passes() {
        let s = skill(
            "function test() { return true; }",
            vec!["test()"],
            vec![("test", "(): boolean")],
            CapabilityTier::Pure,
            vec![],
        );
        let report = verify_skill(&s).unwrap();
        assert_eq!(report.test_results.len(), 1);
        assert_eq!(report.test_results[0], TestResult::Passed);
    }

    #[test]
    fn tests_require_exact_boolean_true_and_report_the_rejection() {
        for (name, expression, expected) in [
            ("false", "false", TestResult::ReturnedFalse),
            ("number", "1", TestResult::ReturnedFalse),
            ("string", "'true'", TestResult::ReturnedFalse),
            ("object", "{}", TestResult::ReturnedFalse),
            ("array", "[]", TestResult::ReturnedFalse),
            ("undefined", "undefined", TestResult::ReturnedFalse),
            ("null", "null", TestResult::ReturnedFalse),
            (
                "exception",
                "(() => { throw new Error('must stay private'); })()",
                TestResult::Threw("Exception/Evaluation/EmbeddedTest".into()),
            ),
        ] {
            // Test the expression's value directly: undefined returned across the
            // export ABI would fail cloning before reaching boolean enforcement.
            let script = format!("test(); ({expression})");
            let s = skill(
                "function test() { return true; }",
                vec![&script],
                vec![("test", "(): boolean")],
                CapabilityTier::Pure,
                vec![],
            );
            match verify_skill(&s) {
                Err(VerificationError::TestFailed { index, outcome }) => {
                    assert_eq!(index, 0, "{name}");
                    assert_eq!(outcome, expected, "{name}");
                }
                result => panic!("{name}: expected a test rejection, got {result:?}"),
            }
        }
    }

    #[test]
    fn test_syntax_error_in_source() {
        let s = skill(
            "function test() { return ",
            vec!["test()"],
            vec![("test", "(): boolean")],
            CapabilityTier::Pure,
            vec![],
        );
        let result = verify_skill(&s);
        assert!(result.is_err());
        match result {
            Err(VerificationError::SourceEvaluationFailed(_)) => {}
            _ => panic!("expected SourceEvaluationFailed"),
        }
    }

    #[test]
    fn test_no_tests_error() {
        let s = SkillArtifact::new(
            "function test() { return true; }".to_string(),
            "test skill".to_string(),
            vec![],
            vec![SkillExport {
                name: "test".to_string(),
                signature: "(): boolean".to_string(),
            }],
            vec![], // No tests
            CapabilityManifest::pure(),
        )
        .unwrap();
        let result = verify_skill(&s);
        assert!(result.is_err());
        match result {
            Err(VerificationError::NoTests) => {}
            _ => panic!("expected NoTests"),
        }
    }

    #[test]
    fn test_no_exports_error() {
        let s = SkillArtifact::new(
            "function test() { return true; }".to_string(),
            "test skill".to_string(),
            vec![],
            vec![], // No exports
            vec!["test()".to_string()],
            CapabilityManifest::pure(),
        )
        .unwrap();
        let result = verify_skill(&s);
        assert!(result.is_err());
        match result {
            Err(VerificationError::NoExports) => {}
            _ => panic!("expected NoExports"),
        }
    }

    #[test]
    fn declared_exports_must_exist_and_be_functions() {
        for (name, source) in [
            ("missing", "function present() { return true; }"),
            ("non_function", "var test = 42;"),
        ] {
            let s = skill(
                source,
                vec!["true"],
                vec![("test", "(): boolean")],
                CapabilityTier::Pure,
                vec![],
            );
            let result = verify_skill(&s);
            assert!(
                matches!(result, Err(VerificationError::SourceEvaluationFailed(_))),
                "{name}: expected source rejection before tests run, got {result:?}"
            );
        }
    }

    #[test]
    fn test_multiple_tests_in_order() {
        let s = skill(
            "function add(_cap, a, b) { return a + b; }",
            vec!["add(1, 1) === 2", "add(2, 3) === 5", "add(0, 0) === 0"],
            vec![("add", "(a, b): number")],
            CapabilityTier::Pure,
            vec![],
        );
        let report = verify_skill(&s).unwrap();
        assert_eq!(report.test_results.len(), 3);
        assert!(report.test_results.iter().all(|r| *r == TestResult::Passed));
    }

    #[test]
    fn time_and_randomness_are_identically_unavailable_to_skills() {
        let s = skill(
            "function inspect() { return [typeof Date, typeof Math.random, typeof performance].join(','); }",
            vec!["inspect() === 'undefined,undefined,undefined'"],
            vec![("inspect", "(): string")],
            CapabilityTier::Pure,
            vec![],
        );
        let report = verify_skill(&s).expect("hardened deterministic globals verify");
        assert_eq!(report.test_results, vec![TestResult::Passed]);
    }

    #[test]
    fn verification_cases_and_requests_receive_fresh_skill_state() {
        let s = skill(
            "let counter = 0; function increment() { counter++; return counter; }",
            vec!["increment() === 1", "increment() === 1"],
            vec![("increment", "(): boolean")],
            CapabilityTier::Pure,
            vec![],
        );
        for request in 0..2 {
            let report = verify_skill(&s).expect("each request starts with fresh state");
            assert_eq!(
                report.test_results,
                vec![TestResult::Passed, TestResult::Passed],
                "request {request}"
            );
        }
    }

    #[test]
    fn verifier_rejects_source_that_only_a_generated_function_wrapper_accepts() {
        let s = skill(
            "return; function test() { return true; }",
            vec!["test()"],
            vec![("test", "(): boolean")],
            CapabilityTier::Pure,
            vec![],
        );

        let result = verify_skill(&s);
        assert!(
            matches!(result, Err(VerificationError::SourceEvaluationFailed(_))),
            "unexpected infinite-source result: {result:?}"
        );
    }

    #[test]
    fn verifier_rejects_top_level_effects_before_capability_construction() {
        let s = skill(
            "write_file('tmp/a', 'x'); function test(_cap) { return true; }",
            vec!["test()"],
            vec![("test", "(): boolean")],
            CapabilityTier::SideEffecting,
            vec![HostCapability::WriteFile],
        );

        let result = verify_skill(&s);
        assert!(
            matches!(result, Err(VerificationError::SourceEvaluationFailed(_))),
            "unexpected infinite-source result: {result:?}"
        );
    }

    #[test]
    fn verifier_exposes_declared_effects_only_on_the_hidden_capability_object() {
        let s = skill(
            "const ambient = typeof read_file; function inspect(cap) { return ambient === 'undefined' && typeof cap.read_file === 'function'; }",
            vec!["inspect()"],
            vec![("inspect", "(): boolean")],
            CapabilityTier::ReadOnly,
            vec![HostCapability::ReadFile],
        );

        assert!(verify_skill(&s).is_ok());
    }

    #[test]
    fn test_mutation_detected_when_export_is_used() {
        let s = skill(
            "function getValue() { return 42; }",
            vec!["getValue() === 42"],
            vec![("getValue", "(): number")],
            CapabilityTier::Pure,
            vec![],
        );
        let report = verify_skill(&s).unwrap();
        assert_eq!(report.mutation_outcomes.len(), 1);
        assert_eq!(report.mutation_outcomes[0], MutationOutcome::Detected);
    }

    #[test]
    fn assertion_free_export_call_does_not_satisfy_mutation_coverage() {
        let s = skill(
            "function run() { return 42; }",
            vec!["(run(), true)"],
            vec![("run", "(): number")],
            CapabilityTier::Pure,
            vec![],
        );

        let result = verify_skill(&s);
        assert!(
            matches!(
                &result,
                Err(VerificationError::MutationPassFailed { export, .. }) if export == "run"
            ),
            "unexpected verification result: {result:?}"
        );
    }

    #[test]
    fn mutation_coverage_observes_calls_through_another_export() {
        let s = skill(
            "function inner(value) { return value + 1; } function outer(_cap, value) { return inner(value); }",
            vec!["outer(1) === 2"],
            vec![
                ("inner", "(value: number): number"),
                ("outer", "(value: number): number"),
            ],
            CapabilityTier::Pure,
            vec![],
        );

        let report = verify_skill(&s).expect("indirectly called export should be covered");
        assert_eq!(
            report.mutation_outcomes,
            vec![MutationOutcome::Detected, MutationOutcome::Detected]
        );
    }

    #[test]
    fn test_mutation_undetected_when_export_unused() {
        let s = skill(
            "function unused() { return 42; }",
            vec!["true"],
            vec![("unused", "(): number")],
            CapabilityTier::Pure,
            vec![],
        );
        let result = verify_skill(&s);
        assert!(
            matches!(
                &result,
                Err(VerificationError::MutationPassFailed { export, .. }) if export == "unused"
            ),
            "unexpected verification result: {result:?}"
        );
    }

    #[test]
    fn test_mutation_context_is_fresh_for_each_export() {
        let s = skill(
            "function used() { return 1; } function unused() { return 2; }",
            vec!["used() === 1"],
            vec![("used", "(): number"), ("unused", "(): number")],
            CapabilityTier::Pure,
            vec![],
        );
        assert!(matches!(
            verify_skill(&s),
            Err(VerificationError::MutationPassFailed { export, .. }) if export == "unused"
        ));
    }

    #[test]
    fn test_tier_0_pure_has_no_capability() {
        let s = skill(
            "function test() { return typeof read_file === 'undefined'; }",
            vec!["test()"],
            vec![("test", "(): boolean")],
            CapabilityTier::Pure,
            vec![],
        );
        let report = verify_skill(&s).unwrap();
        assert_eq!(report.test_results.len(), 1);
        assert_eq!(report.test_results[0], TestResult::Passed);
    }

    #[test]
    fn test_report_contains_metadata() {
        let s = skill(
            "function test() { return true; }",
            vec!["test()"],
            vec![("test", "(): boolean")],
            CapabilityTier::Pure,
            vec![],
        );
        let report = verify_skill(&s).unwrap();
        assert_eq!(report.skill_id, s.id);
        assert_eq!(report.identity_version, s.identity_version);
        assert_eq!(report.capability, s.capability);
        assert!(report.verifier_version > 0);
        assert_eq!(report.fakes_version, 4);
        assert!(report.memory_limit > 0);
        assert!(report.stack_limit > 0);
    }

    #[test]
    fn test_multiple_exports() {
        let s = skill(
            "function foo() { return true; } function bar() { return true; }",
            vec!["foo()", "bar()", "foo() && bar()"],
            vec![("foo", "(): boolean"), ("bar", "(): boolean")],
            CapabilityTier::Pure,
            vec![],
        );
        let report = verify_skill(&s).unwrap();
        assert_eq!(report.test_results.len(), 3);
        assert!(report.test_results.iter().all(|r| *r == TestResult::Passed));
        assert_eq!(report.mutation_outcomes.len(), 2);
        assert!(
            report
                .mutation_outcomes
                .iter()
                .all(|o| *o == MutationOutcome::Detected)
        );
    }

    #[test]
    fn test_separate_script_locations() {
        // Each test should be a separate script location in the context.
        // This test verifies they can have independent line numbers.
        let s = skill(
            "function check() { return true; }",
            vec!["check()", "check()", "check()"],
            vec![("check", "(): boolean")],
            CapabilityTier::Pure,
            vec![],
        );
        let report = verify_skill(&s).unwrap();
        assert_eq!(report.test_results.len(), 3);
        assert!(report.test_results.iter().all(|r| *r == TestResult::Passed));
    }

    #[test]
    fn test_export_with_complex_logic() {
        let s = skill(
            r#"
            function isPrime(_cap, n) {
                if (n < 2) return false;
                for (let i = 2; i * i <= n; i++) {
                    if (n % i === 0) return false;
                }
                return true;
            }
            "#,
            vec![
                "isPrime(2)",
                "isPrime(3)",
                "isPrime(4) === false",
                "isPrime(5)",
            ],
            vec![("isPrime", "(n: number): boolean")],
            CapabilityTier::Pure,
            vec![],
        );
        let report = verify_skill(&s).unwrap();
        assert_eq!(report.test_results.len(), 4);
        assert!(report.test_results.iter().all(|r| *r == TestResult::Passed));
        assert_eq!(report.mutation_outcomes.len(), 1);
        assert_eq!(report.mutation_outcomes[0], MutationOutcome::Detected);
    }

    #[test]
    fn test_mutation_multiple_exports() {
        let s = skill(
            "function foo() { return 1; } function bar() { return 2; }",
            vec!["foo() === 1 && bar() === 2"],
            vec![("foo", "(): number"), ("bar", "(): number")],
            CapabilityTier::Pure,
            vec![],
        );
        let report = verify_skill(&s).unwrap();
        assert_eq!(report.test_results.len(), 1);
        assert_eq!(report.test_results[0], TestResult::Passed);
        assert_eq!(report.mutation_outcomes.len(), 2);
        // Both should be detected since the test uses both.
        assert!(
            report
                .mutation_outcomes
                .iter()
                .all(|o| *o == MutationOutcome::Detected)
        );
    }

    #[test]
    fn test_multi_export_vacuity_is_rejected() {
        let s = skill(
            "function covered() { return true; } function unused() { return true; }",
            vec!["covered()"],
            vec![("covered", "(): boolean"), ("unused", "(): boolean")],
            CapabilityTier::Pure,
            vec![],
        );
        assert!(matches!(
            verify_skill(&s),
            Err(VerificationError::MutationPassFailed { export, .. }) if export == "unused"
        ));
    }

    #[test]
    fn worker_internal_diagnostic_is_verification_infrastructure() {
        use crate::extras::js::protocol::{
            Diagnostic, DiagnosticClass, DiagnosticStage, ScriptRole,
        };

        let internal = Diagnostic {
            class: DiagnosticClass::Internal,
            stage: DiagnosticStage::Initialization,
            script_role: ScriptRole::SkillSource,
            exception_class: None,
            line: None,
            column: None,
        };
        let error = crate::extras::js::skills::verify::test_result(Some(&internal))
            .expect_err("the worker's own internal failure is not a candidate test outcome");
        assert!(
            error.is_infrastructure(),
            "worker host-side failures must not be attributed to the skill: {error:?}"
        );

        let contract = Diagnostic {
            class: DiagnosticClass::Contract,
            stage: DiagnosticStage::Evaluation,
            script_role: ScriptRole::EmbeddedTest,
            exception_class: None,
            line: None,
            column: None,
        };
        assert_eq!(
            crate::extras::js::skills::verify::test_result(Some(&contract))
                .expect("a contract violation is a genuine candidate outcome"),
            TestResult::ReturnedFalse
        );
    }

    #[test]
    fn worker_failures_preserve_closed_infrastructure_reasons() {
        // These reasons are parent-defined: no worker text, source, or OS error
        // is needed to distinguish a launch failure from a lost transport.
        for (worker, expected) in [
            (
                WorkerError::ContainmentUnavailable,
                "JavaScript worker containment is unavailable",
            ),
            (WorkerError::Launch, "JavaScript worker launch failed"),
            (WorkerError::Transport, "JavaScript worker transport failed"),
            (
                WorkerError::Protocol,
                "JavaScript worker violated its protocol",
            ),
            (
                WorkerError::BuildMismatch,
                "JavaScript worker build identity differs from the parent",
            ),
            (
                WorkerError::PermissionPromptTimedOut,
                "JavaScript permission prompt was not answered before the invocation deadline",
            ),
            (
                WorkerError::EffectOutcomeUnknown,
                "JavaScript effect completed with an unknown outcome",
            ),
            (
                WorkerError::StaleGeneration,
                "JavaScript worker returned a stale process generation",
            ),
            (
                WorkerError::IdentityExhausted,
                "JavaScript worker supervisor identity space is exhausted",
            ),
            (
                WorkerError::BlockingVerifyInAsyncRuntime,
                "blocking JavaScript verification cannot run inside a Tokio runtime",
            ),
            (
                WorkerError::TimedOut,
                "worker verification deadline expired before the worker reported a result",
            ),
            (
                WorkerError::NativeCpuLimit,
                "worker process exhausted its cumulative native CPU budget",
            ),
        ] {
            let error = worker_error(worker);
            assert!(error.is_infrastructure(), "{worker:?}: {error:?}");
            let VerificationError::InfrastructureUnavailable(reason) = error else {
                panic!("{worker:?}: wrong verification failure class");
            };
            assert_eq!(reason, expected, "{worker:?}");
        }
    }

    #[test]
    fn test_transcript_empty_for_tier_0() {
        let s = skill(
            "function test() { return true; }",
            vec!["test()"],
            vec![("test", "(): boolean")],
            CapabilityTier::Pure,
            vec![],
        );
        let report = verify_skill(&s).unwrap();
        assert!(report.transcript.is_empty());
    }
}

/// Gap probes for requirements bead y8n lists as mandatory. These assert the
/// specified behaviour, not the behaviour that happens to be implemented.
#[cfg(test)]
mod required_behaviour_probes {
    use crate::extras::js::skills::verify::verify_skill;
    use crate::extras::js::skills::{
        CapabilityTier, HostCapability, SkillArtifact, SkillExport, test_manifest,
    };

    fn artifact(
        source: &str,
        tests: Vec<&str>,
        exports: Vec<(&str, &str)>,
        tier: CapabilityTier,
        hosts: Vec<HostCapability>,
    ) -> SkillArtifact {
        SkillArtifact::new(
            source.to_string(),
            "probe".to_string(),
            vec![],
            exports
                .into_iter()
                .map(|(name, signature)| SkillExport {
                    name: name.to_string(),
                    signature: signature.to_string(),
                })
                .collect(),
            tests.into_iter().map(str::to_string).collect(),
            test_manifest(tier, hosts).expect("valid manifest"),
        )
        .expect("valid artifact")
    }

    #[test]
    fn probe_tier0_genuinely_lacks_hosts_while_tier1_has_them() {
        // This is only meaningful if some tier DOES get globals; otherwise the
        // Tier 0 assertion is vacuous.
        let tier0 = artifact(
            "function f(cap) { return typeof cap.read_file; }",
            vec!["f() === 'undefined'"],
            vec![("f", "f(): boolean")],
            CapabilityTier::Pure,
            vec![],
        );
        assert!(verify_skill(&tier0).is_ok(), "Tier 0 must see no read_file");

        let tier1 = artifact(
            "function f(cap) { return typeof cap.read_file; }",
            vec!["f() === 'function'"],
            vec![("f", "f(): boolean")],
            CapabilityTier::ReadOnly,
            vec![HostCapability::ReadFile],
        );
        assert!(
            verify_skill(&tier1).is_ok(),
            "Tier 1 declaring read_file must see it as a function"
        );
    }

    #[test]
    fn probe_undeclared_host_is_unavailable_to_tier2() {
        // Declares Spawn only; fetch must not appear.
        let skill = artifact(
            "function f(cap) { return typeof cap.spawn === 'function' && typeof cap.fetch === 'undefined'; }",
            vec!["f()"],
            vec![("f", "f(): boolean")],
            CapabilityTier::SideEffecting,
            vec![HostCapability::Spawn],
        );
        assert!(
            verify_skill(&skill).is_ok(),
            "only declared hosts may be present"
        );
    }
}

/// Second-round probes: the fakes must be real host-backed fakes, not JS stubs.
#[cfg(test)]
mod fake_integrity_probes {
    use crate::extras::js::skills::verify::verify_skill;
    use crate::extras::js::skills::{
        CapabilityManifest, CapabilityScope, CapabilityTier, HostCapability, SkillArtifact,
        SkillExport,
    };

    fn artifact(source: &str, tests: Vec<&str>, hosts: Vec<HostCapability>) -> SkillArtifact {
        SkillArtifact::new(
            source.to_string(),
            "probe".to_string(),
            vec![],
            vec![SkillExport {
                name: "f".to_string(),
                signature: "f(): boolean".to_string(),
            }],
            tests.into_iter().map(str::to_string).collect(),
            CapabilityManifest::new(
                CapabilityTier::SideEffecting,
                hosts
                    .into_iter()
                    .map(|host| match host {
                        HostCapability::ReadFile => CapabilityScope::ReadFile {
                            workspace_prefixes: vec!["virtual".into()],
                        },
                        HostCapability::WriteFile => CapabilityScope::WriteFile {
                            workspace_prefixes: vec!["virtual".into()],
                        },
                        HostCapability::Spawn => CapabilityScope::Spawn {
                            programs: vec!["printf".into()],
                        },
                        HostCapability::Fetch => CapabilityScope::Fetch {
                            origins: vec!["https://example.com".into()],
                            methods: vec![crate::extras::js::skills::HttpMethod::Get],
                        },
                    })
                    .collect(),
            )
            .expect("manifest"),
        )
        .expect("artifact")
    }

    #[test]
    fn probe_fake_write_then_read_round_trips_through_virtual_state() {
        // A real record/replay fake keeps virtual state; a hardcoded JS stub does not.
        let skill = artifact(
            "function f(cap) { cap.write_file('virtual/a.txt', 'hello'); return cap.read_file('virtual/a.txt') === 'hello'; }",
            vec!["f() === true"],
            vec![HostCapability::ReadFile, HostCapability::WriteFile],
        );
        let result = verify_skill(&skill);
        assert!(
            result.is_ok(),
            "fakes must maintain virtual state across calls, got {result:?}"
        );
    }

    #[test]
    fn probe_fake_calls_are_recorded_in_the_transcript() {
        let skill = artifact(
            "function f(cap) { cap.write_file('virtual/b.txt', 'x'); return true; }",
            vec!["f() === true"],
            vec![HostCapability::WriteFile],
        );
        let report = verify_skill(&skill).expect("should verify");
        assert!(
            !report.transcript.writes.is_empty(),
            "a declared write_file call must be recorded in the transcript, got {:?}",
            report.transcript
        );
    }

    #[test]
    fn probe_embedded_test_cannot_replace_a_fake() {
        // The skill tries to monkey-patch the fake so its own assertion would
        // trivially succeed. The fake is sealed non-writable, so QuickJS throws on
        // assignment and verification fails — the tampering attempt cannot yield a
        // passing verification.
        let skill = artifact(
            "function f(cap) { cap.write_file = function() { return 'pwned'; }; return cap.write_file('virtual/c', 'x') !== 'pwned'; }",
            vec!["f()"],
            vec![HostCapability::WriteFile],
        );
        let result = verify_skill(&skill);
        assert!(
            result.is_err(),
            "replacing a verifier-owned fake must not produce a passing verification, got {result:?}"
        );

        // And the genuine fake still works for a skill that does not tamper.
        let honest = artifact(
            "function f(cap) { cap.write_file('virtual/d', 'x'); return true; }",
            vec!["f() === true"],
            vec![HostCapability::WriteFile],
        );
        assert!(
            verify_skill(&honest).is_ok(),
            "sealing the fakes must not break ordinary declared use"
        );
    }
}

/// Attribution of verification failures to the candidate versus the
/// infrastructure.
///
/// A proposal identity that is rejected can never be re-proposed
/// (`SkillStore::enqueue_proposal` returns `Rejected` forever for it), so a
/// failure that is not caused by the proposed source must always stay
/// retryable.
#[cfg(test)]
mod failure_attribution {
    use crate::extras::js::skills::admission::{
        AdmissionError, AdmissionEvaluator, AuthenticatedHumanDecision, HumanReviewer,
        ReviewDecision, ReviewOutcome, ReviewPacket,
    };
    use crate::extras::js::skills::embed::Embedder;
    use crate::extras::js::skills::held_out::{
        ExpectedJsValue, HeldOutCase, HeldOutSelector, HeldOutSuiteDraft, TranscriptExpectation,
    };
    use crate::extras::js::skills::store::{AdminIdentity, ProposalStatus, SkillStore};
    use crate::extras::js::skills::verify::{VerificationError, worker_error};
    use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
    use crate::extras::js::supervisor::WorkerError;
    use crate::paths::{AppPaths, PathEnvironment, PathPlatform};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn paths() -> (PathBuf, AppPaths) {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "verification_attribution_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let environment = PathEnvironment {
            platform: if cfg!(target_os = "macos") {
                PathPlatform::MacOs
            } else if cfg!(target_os = "windows") {
                PathPlatform::Windows
            } else {
                PathPlatform::Linux
            },
            home_dir: None,
            config_base: Some(root.join("config")),
            data_base: Some(root.join("data")),
            local_data_base: Some(root.join("local")),
            state_base: Some(root.join("state")),
            cache_base: Some(root.join("cache")),
            workspace_root: None,
            overrides: Default::default(),
        };
        (root, AppPaths::resolve(&environment).expect("paths"))
    }

    fn candidate() -> SkillArtifact {
        SkillArtifact::new(
            "function normalize(_cap, v) { return String(v).trim(); }".to_string(),
            "Normalize a value.".to_string(),
            vec!["normalize".to_string()],
            vec![SkillExport {
                name: "normalize".to_string(),
                signature: "normalize(value: unknown): string".to_string(),
            }],
            vec!["normalize(' x ') === 'x'".to_string()],
            CapabilityManifest::pure(),
        )
        .expect("artifact")
    }

    fn suite() -> HeldOutSuiteDraft {
        HeldOutSuiteDraft {
            selector: HeldOutSelector {
                tags: vec!["normalize".to_string()],
                exports: vec![SkillExport {
                    name: "normalize".to_string(),
                    signature: "normalize(value: unknown): string".to_string(),
                }],
                capability_tier: Some("pure".to_string()),
            },
            cases: vec![HeldOutCase {
                expression: "normalize('\\tvalue\\n')".to_string(),
                expected: ExpectedJsValue::String("value".to_string()),
                fake_files: BTreeMap::new(),
                fake_spawns: vec![],
                fake_fetches: vec![],
                transcript: TranscriptExpectation::default(),
            }],
        }
    }

    fn evaluator() -> (PathBuf, AdmissionEvaluator, SkillArtifact) {
        let (root, paths) = paths();
        let mut store = SkillStore::open_at(&paths).expect("store");
        suite()
            .import(
                &mut store,
                &AdminIdentity::authenticated("suite-admin").unwrap(),
                5,
            )
            .expect("suite");
        let artifact = candidate();
        store
            .enqueue_proposal(&artifact, None, 10)
            .expect("proposal");
        let evaluator = AdmissionEvaluator::new(
            store,
            std::sync::Arc::new(Embedder::new().unwrap()),
            "worker-1",
        )
        .expect("evaluator");
        (root, evaluator, artifact)
    }

    struct Approver {
        now: i64,
    }

    impl HumanReviewer for Approver {
        fn review(&self, _packet: &ReviewPacket) -> ReviewDecision {
            ReviewDecision::Approve(AuthenticatedHumanDecision::verified(
                format!("decision-{}", self.now),
                "human-reviewer",
                self.now,
            ))
        }
    }

    struct Denier;

    impl HumanReviewer for Denier {
        fn review(&self, _packet: &ReviewPacket) -> ReviewDecision {
            ReviewDecision::Deny {
                reason_code: "local_owner_rejected".to_string(),
            }
        }
    }

    /// The proposal must survive the failure, still queued and unrejected.
    fn assert_identity_survives(evaluator: &AdmissionEvaluator, artifact: &SkillArtifact) {
        let proposal = evaluator
            .store()
            .get_proposal(&artifact.id)
            .unwrap()
            .expect("proposal must remain queued");
        assert_eq!(
            proposal.status,
            ProposalStatus::Pending,
            "an infrastructure failure must not permanently reject the identity"
        );
        assert_eq!(proposal.reason_code, None);
        assert_eq!(proposal.attempt_count, 0);
        assert_eq!(proposal.infrastructure_attempt_count, 1);
        assert_eq!(
            evaluator.store().revision_status(&artifact.id).unwrap(),
            Some("pending".to_string())
        );
    }

    fn assert_not_rejected_by(error: VerificationError) {
        let (root, mut evaluator, artifact) = evaluator();
        evaluator.fail_next_verification_for_test(error);
        let outcome = evaluator
            .evaluate_next(20)
            .expect_err("an infrastructure failure must be retryable");
        assert!(
            matches!(outcome, AdmissionError::Retryable(_)),
            "unexpected admission outcome: {outcome:?}"
        );
        assert_identity_survives(&evaluator, &artifact);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parent_side_verification_deadline_does_not_reject_the_identity() {
        assert_not_rejected_by(worker_error(WorkerError::TimedOut));
    }

    #[test]
    fn native_cpu_exhaustion_does_not_reject_the_identity() {
        assert_not_rejected_by(worker_error(WorkerError::NativeCpuLimit));
    }

    #[test]
    fn worker_internal_failure_does_not_reject_the_identity() {
        use crate::extras::js::protocol::{
            Diagnostic, DiagnosticClass, DiagnosticStage, ScriptRole,
        };

        let error = crate::extras::js::skills::verify::test_result(Some(&Diagnostic {
            class: DiagnosticClass::Internal,
            stage: DiagnosticStage::Initialization,
            script_role: ScriptRole::SkillSource,
            exception_class: None,
            line: None,
            column: None,
        }))
        .expect_err("the worker's own internal failure is not a candidate outcome");
        assert_not_rejected_by(error);
    }

    #[test]
    fn worker_contract_mismatch_does_not_reject_the_identity() {
        assert_not_rejected_by(worker_error(WorkerError::Protocol));
    }

    #[test]
    fn approval_gate_outage_is_reported_as_infrastructure_and_is_retryable() {
        let (root, mut evaluator, artifact) = evaluator();
        evaluator.evaluate_next(20).unwrap().unwrap();

        evaluator.fail_next_review_gate_for_test(worker_error(WorkerError::ContainmentUnavailable));
        let error = evaluator
            .review_and_admit(&artifact.id, &Approver { now: 21 }, 21)
            .expect_err("a contained worker outage must fail the approval");
        assert!(
            matches!(error, AdmissionError::Infrastructure(_)),
            "a transient worker outage must not be reported as a changed review: {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("verification infrastructure"),
            "operator-facing message must name the cause: {message}"
        );
        assert!(
            message.contains("unavailable"),
            "operator-facing message must name the inner error: {message}"
        );
        assert_ne!(
            evaluator.store().revision_status(&artifact.id).unwrap(),
            Some("canary".to_string())
        );

        // The same record approves once the infrastructure recovers, proving
        // the failure was retryable rather than terminal.
        let outcome = evaluator
            .review_and_admit(&artifact.id, &Approver { now: 22 }, 22)
            .expect("approval must succeed once the worker is back");
        assert!(matches!(outcome, ReviewOutcome::Canary(_)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn approval_runs_the_contained_held_out_gate_exactly_once() {
        let (root, mut evaluator, artifact) = evaluator();
        evaluator.evaluate_next(20).unwrap().unwrap();
        assert_eq!(evaluator.review_gate_runs_for_test(), 0);

        let outcome = evaluator
            .review_and_admit(&artifact.id, &Approver { now: 21 }, 21)
            .expect("approval");
        assert!(matches!(outcome, ReviewOutcome::Canary(_)));
        assert_eq!(
            evaluator.review_gate_runs_for_test(),
            1,
            "the contained held-out gate must run exactly once per approval"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn denial_does_not_require_a_passing_approval_gate() {
        let (root, mut evaluator, artifact) = evaluator();
        evaluator.evaluate_next(20).unwrap().unwrap();
        // Break the gate: no held-out suite is selectable any more.
        evaluator
            .store()
            .conn()
            .execute("UPDATE held_out_suites SET enabled = 0", [])
            .expect("disable suites");

        assert_eq!(
            evaluator
                .review_and_admit(&artifact.id, &Denier, 21)
                .expect("a rejection must not depend on the verification worker"),
            ReviewOutcome::Denied
        );
        assert_eq!(
            evaluator.store().revision_status(&artifact.id).unwrap(),
            Some("rejected".to_string())
        );
        assert_eq!(
            evaluator.review_gate_runs_for_test(),
            0,
            "a denial must never run the contained gate"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn approval_still_requires_a_passing_gate() {
        let (root, mut evaluator, artifact) = evaluator();
        evaluator.evaluate_next(20).unwrap().unwrap();
        evaluator
            .store()
            .conn()
            .execute("UPDATE held_out_suites SET enabled = 0", [])
            .expect("disable suites");

        let error = evaluator
            .review_and_admit(&artifact.id, &Approver { now: 21 }, 21)
            .expect_err("approval must not be granted when the gate no longer passes");
        assert!(
            matches!(error, AdmissionError::StaleReview),
            "a changed held-out suite selection is a genuine semantic change: {error:?}"
        );
        assert_ne!(
            evaluator.store().revision_status(&artifact.id).unwrap(),
            Some("canary".to_string())
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Every effect a case exercises must be declared as a fixture.
///
/// Before fakes v4 an unfixtured `spawn` answered `simulated <program> completed` with
/// exit code 0 and an unfixtured `fetch` answered HTTP 200 with a synthetic JSON body,
/// while an unseeded `read_file` failed with `File not found:`. A candidate that shelled
/// out to a real executable or depended on a real HTTP response therefore passed its
/// embedded tests and its held-out suite against a response the harness invented, and was
/// admitted on evidence that proved nothing. Both undeclared effects now fail the way
/// `read_file` always did, the attempt is still recorded in the transcript, and a declared
/// fixture still replays unchanged.
#[cfg(test)]
mod undeclared_effect_fixtures {
    use crate::extras::js::skills::fakes::{
        FakeFetchFixture, FakeFetchResponse, FakeSpawnFixture, FakeSpawnResponse,
    };
    use crate::extras::js::skills::held_out::ExpectedJsValue;
    use crate::extras::js::skills::verify::verify_held_out_case;
    use crate::extras::js::skills::{
        CapabilityTier, HostCapability, SkillArtifact, SkillExport, test_manifest,
    };
    use std::collections::BTreeMap;

    /// The embedded test is never executed here: `verify_held_out_case` sends exactly one
    /// held-out case, which is the only kind of case that can carry fixtures.
    fn probe_artifact(source: &str, capability: HostCapability) -> SkillArtifact {
        SkillArtifact::new(
            source.to_string(),
            "undeclared effect probe".to_string(),
            vec![],
            vec![SkillExport {
                name: "probe".to_string(),
                signature: "probe(): string".to_string(),
            }],
            vec!["typeof probe === 'function'".to_string()],
            test_manifest(CapabilityTier::SideEffecting, vec![capability]).expect("manifest"),
        )
        .expect("artifact")
    }

    #[test]
    fn an_unfixtured_spawn_fails_and_a_declared_fixture_still_replays() {
        let skill = probe_artifact(
            "function probe(cap) { try { const r = cap.spawn('printf', ['check']); return 'ok:' + r.code + ':' + r.stdout; } catch (_) { return 'threw'; } }",
            HostCapability::Spawn,
        );

        let undeclared = verify_held_out_case(
            &skill,
            "probe()",
            &ExpectedJsValue::String("threw".to_string()),
            &BTreeMap::new(),
            &[],
            &[],
        )
        .expect("an unfixtured spawn must fail inside the skill instead of fabricating a success");
        assert_eq!(
            undeclared.spawns.len(),
            1,
            "the unfixtured attempt must still be recorded: {undeclared:?}"
        );
        assert_eq!(undeclared.spawns[0].program, "printf");
        assert_eq!(undeclared.spawns[0].args.as_slice(), ["check"]);
        assert_eq!(
            undeclared.spawns[0].result,
            Err(r#"Spawn fixture not found: printf ["check"]"#.to_string()),
            "the failure must name the program and arguments that had no fixture"
        );

        let declared = verify_held_out_case(
            &skill,
            "probe()",
            &ExpectedJsValue::String("ok:7:hello\n".to_string()),
            &BTreeMap::new(),
            &[FakeSpawnFixture {
                program: "printf".to_string(),
                args: vec!["check".to_string()],
                response: FakeSpawnResponse {
                    stdout: "hello\n".to_string(),
                    stderr: String::new(),
                    code: 7,
                    timed_out: false,
                    stdout_truncated: false,
                    stderr_truncated: false,
                },
            }],
            &[],
        )
        .expect("a declared spawn fixture must still replay unchanged");
        assert_eq!(declared.spawns.len(), 1);
        assert!(
            declared.spawns[0].result.is_ok(),
            "a fixtured spawn must record a successful replay: {declared:?}"
        );
    }

    #[test]
    fn an_unfixtured_fetch_fails_and_a_declared_fixture_still_replays() {
        let skill = probe_artifact(
            "function probe(cap) { try { const r = cap.fetch('https://example.com'); return 'ok:' + r.status + ':' + r.body; } catch (_) { return 'threw'; } }",
            HostCapability::Fetch,
        );

        let undeclared = verify_held_out_case(
            &skill,
            "probe()",
            &ExpectedJsValue::String("threw".to_string()),
            &BTreeMap::new(),
            &[],
            &[],
        )
        .expect("an unfixtured fetch must fail inside the skill instead of fabricating a 200");
        assert_eq!(
            undeclared.fetches.len(),
            1,
            "the unfixtured attempt must still be recorded: {undeclared:?}"
        );
        assert_eq!(undeclared.fetches[0].url, "https://example.com");
        assert_eq!(
            undeclared.fetches[0].result,
            Err("Fetch fixture not found: GET https://example.com".to_string()),
            "the failure must name the method and URL that had no fixture"
        );

        let declared = verify_held_out_case(
            &skill,
            "probe()",
            &ExpectedJsValue::String("ok:204:seeded".to_string()),
            &BTreeMap::new(),
            &[],
            &[FakeFetchFixture {
                url: "https://example.com".to_string(),
                method: "GET".to_string(),
                response: FakeFetchResponse {
                    status: 204,
                    body: "seeded".to_string(),
                },
            }],
        )
        .expect("a declared fetch fixture must still replay unchanged");
        assert_eq!(declared.fetches.len(), 1);
        assert!(
            declared.fetches[0].result.is_ok(),
            "a fixtured fetch must record a successful replay: {declared:?}"
        );
    }
}
