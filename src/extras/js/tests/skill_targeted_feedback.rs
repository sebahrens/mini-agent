use std::collections::BTreeSet;

use crate::extras::js::skills::feedback::{
    ActorKind, AuthenticatedActor, FeedbackCommand, FeedbackError, FeedbackKind, FeedbackService,
    FeedbackState, RAW_TELEMETRY_RETENTION_DAYS, SEVERE_FEEDBACK_REASON_CODES,
};
use crate::extras::js::skills::privacy::Redactor;
use crate::extras::js::skills::retention::RetentionService;
use crate::extras::js::skills::telemetry::{
    EventBatch, SkillEvent, SkillEventKind, TelemetryIngestor, stable_invocation_id,
};
use crate::extras::js::skills::{
    CapabilityManifest, SkillArtifact, SkillExport, store::SkillStore,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

fn fixture() -> (super::TestTempDir, SkillStore, SkillArtifact, String) {
    let directory = super::TestTempDir::new("feedback");
    let root = directory.path().to_path_buf();
    let env = PathEnvironment {
        platform: if cfg!(target_os = "macos") {
            PathPlatform::MacOs
        } else if cfg!(target_os = "windows") {
            PathPlatform::Windows
        } else {
            PathPlatform::Linux
        },
        home_dir: None,
        config_base: Some(root.clone()),
        data_base: Some(root.clone()),
        local_data_base: Some(root.clone()),
        state_base: Some(root.clone()),
        cache_base: Some(root.clone()),
        workspace_root: None,
        overrides: Default::default(),
    };
    let mut store = SkillStore::open_at(&AppPaths::resolve(&env).unwrap()).unwrap();
    let skill = SkillArtifact::new(
        "function run() { return true; }".into(),
        "Feedback fixture".into(),
        vec![],
        vec![SkillExport {
            name: "run".into(),
            signature: "() => bool".into(),
        }],
        vec!["run()".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    store.insert_verified(&skill).unwrap();
    let invocation = stable_invocation_id("turn", "tool", &skill.id, "run", 0);
    let event = SkillEvent {
        invocation_id: Some(invocation.clone()),
        skill_id: skill.id.clone(),
        turn_id: "turn".into(),
        tool_call_id: Some("tool".into()),
        kind: SkillEventKind::Invoked,
        export_name: Some("run".into()),
        outcome: None,
        latency_us: None,
        retrieval_score: None,
        retrieval_rank: None,
        query_fingerprint: None,
        index_generation: 0,
        evidence_complete: true,
        production: true,
        argument_shape: None,
        created_at: 1,
    };
    TelemetryIngestor::new(&mut store)
        .ingest(&EventBatch::new(vec![event]).unwrap())
        .unwrap();
    (directory, store, skill, invocation)
}

#[test]
fn skill_feedback_authorization_is_idempotent_redacted_and_audited() {
    type AuditRow = (Option<String>, String, String, String, i64, i64);

    fn snapshot(store: &SkillStore, id: &str, skill_id: &str) -> serde_json::Value {
        let row: (String, i64, i64, String) = store
            .conn()
            .query_row(
                "SELECT state, version, updated_at, COALESCE(reason_text, '')
                 FROM skill_feedback WHERE feedback_id = ?",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let audit: Vec<AuditRow> = store
            .conn()
            .prepare(
                "SELECT from_state, to_state, actor_id, reason_code, version, created_at
                 FROM skill_feedback_audit WHERE feedback_id = ? ORDER BY version",
            )
            .unwrap()
            .query_map([id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let counts: (i64, i64) = store
            .conn()
            .query_row(
                "SELECT user_negative_count, user_positive_count FROM skill_stats WHERE skill_id = ?",
                [skill_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        serde_json::json!({"row": row, "audit": audit, "counts": counts})
    }

    // This exercises the existing test-only state machine. The production
    // correction operator remains unimplemented under mini-agent-1bt82.
    for (next, token) in [
        (FeedbackState::Resolved, "resolved"),
        (FeedbackState::Retracted, "retracted"),
    ] {
        let (root, mut store, skill, invocation) = fixture();
        let actor = owner(&skill.id);
        let command = FeedbackCommand {
            idempotency_key: "feedback-1".into(),
            skill_id: skill.id.clone(),
            invocation_id: Some(invocation),
            kind: FeedbackKind::Negative,
            reason_code: "incorrect_result".into(),
            reason_text: Some("token=SECRET-CANARY".into()),
        };
        let redactor = || Redactor::new(vec!["SECRET-CANARY".into()], 512);
        let id = FeedbackService::new(&mut store, redactor())
            .submit(&actor, &command, 2)
            .unwrap();
        let active = snapshot(&store, &id, &skill.id);
        assert_eq!(
            id,
            FeedbackService::new(&mut store, redactor())
                .submit(&actor, &command, 3)
                .unwrap()
        );
        let mut changed = command.clone();
        changed.reason_text = Some("different explanation".into());
        assert!(matches!(
            FeedbackService::new(&mut store, redactor()).submit(&actor, &changed, 3),
            Err(FeedbackError::IdempotencyConflict)
        ));
        assert_eq!(snapshot(&store, &id, &skill.id), active);

        for denied in [
            AuthenticatedActor {
                kind: ActorKind::Model,
                ..actor.clone()
            },
            AuthenticatedActor {
                allowed_skill_ids: Some(BTreeSet::new()),
                ..actor.clone()
            },
        ] {
            assert!(matches!(
                FeedbackService::new(&mut store, redactor())
                    .change_state(&denied, &id, 1, next, "fixed", 4),
                Err(FeedbackError::Unauthorized)
            ));
            assert_eq!(snapshot(&store, &id, &skill.id), active);
        }
        for (version, target) in [(0, next), (1, FeedbackState::Active)] {
            assert!(matches!(
                FeedbackService::new(&mut store, redactor())
                    .change_state(&actor, &id, version, target, "fixed", 4),
                Err(FeedbackError::InvalidStateTransition)
            ));
            assert_eq!(snapshot(&store, &id, &skill.id), active);
        }
        FeedbackService::new(&mut store, redactor())
            .change_state(&actor, &id, 1, next, "fixed", 4)
            .unwrap();
        let terminal = serde_json::json!({
            "row": [token, 2, 4, "token=[REDACTED]"],
            "audit": [
                [null, "active", "owner", "incorrect_result", 1, 2],
                ["active", token, "owner", "fixed", 2, 4]
            ],
            "counts": [1, 0]
        });
        assert_eq!(snapshot(&store, &id, &skill.id), terminal);
        for retry in [FeedbackState::Resolved, FeedbackState::Retracted] {
            assert!(matches!(
                FeedbackService::new(&mut store, redactor())
                    .change_state(&actor, &id, 2, retry, "again", 5),
                Err(FeedbackError::InvalidStateTransition)
            ));
            assert_eq!(snapshot(&store, &id, &skill.id), terminal);
        }
        assert_eq!(
            id,
            FeedbackService::new(&mut store, redactor())
                .submit(&actor, &command, 6)
                .unwrap()
        );
        assert_eq!(snapshot(&store, &id, &skill.id), terminal);
        drop(store);
        let path = root.path().to_path_buf();
        drop(root);
        assert!(!path.exists());
    }
}

#[test]
fn feedback_authorization_and_unknown_targets_leave_no_records_or_counters() {
    fn totals(store: &SkillStore) -> (i64, i64, i64, i64) {
        store
            .conn()
            .query_row(
                "SELECT
                (SELECT COUNT(*) FROM skill_feedback),
                (SELECT COUNT(*) FROM skill_feedback_audit),
                (SELECT COALESCE(SUM(user_positive_count), 0) FROM skill_stats),
                (SELECT COALESCE(SUM(user_negative_count), 0) FROM skill_stats)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap()
    }

    let (root, mut store, skill, invocation) = fixture();
    let baseline = totals(&store);
    let command = negative_command(&skill.id, &invocation, "authorization");
    let wrong_scope = Some(BTreeSet::from(["f".repeat(64)]));
    assert_ne!(skill.id, "f".repeat(64));
    for (case, kind, actor_id, allowed_skill_ids) in [
        ("model", ActorKind::Model, "model", None),
        ("anonymous", ActorKind::Anonymous, "guest", None),
        ("empty owner", ActorKind::Owner, "", None),
        ("empty reviewer", ActorKind::Reviewer, "", None),
        (
            "owner with empty scope",
            ActorKind::Owner,
            "owner",
            Some(BTreeSet::new()),
        ),
        (
            "reviewer with empty scope",
            ActorKind::Reviewer,
            "reviewer",
            Some(BTreeSet::new()),
        ),
        (
            "owner outside scope",
            ActorKind::Owner,
            "owner",
            wrong_scope.clone(),
        ),
        (
            "reviewer outside scope",
            ActorKind::Reviewer,
            "reviewer",
            wrong_scope,
        ),
    ] {
        let actor = AuthenticatedActor {
            actor_id: actor_id.into(),
            kind,
            allowed_skill_ids,
        };
        let error = FeedbackService::new(&mut store, Redactor::new(vec![], 512))
            .submit(&actor, &command, 2)
            .expect_err(case);
        assert!(
            matches!(error, FeedbackError::Unauthorized),
            "{case}: {error}"
        );
        assert_eq!(
            totals(&store),
            baseline,
            "{case} must not write feedback, audit, or stats"
        );
    }

    // A valid actor and well-formed command must reach the unknown-skill
    // check; an invalid invocation or scope must not hide that boundary.
    let unknown = "f".repeat(64);
    let mut unknown_command = command;
    unknown_command.skill_id = unknown.clone();
    unknown_command.invocation_id = None;
    let error = FeedbackService::new(&mut store, Redactor::new(vec![], 512))
        .submit(&owner(&unknown), &unknown_command, 2)
        .expect_err("unknown skill");
    assert!(matches!(error, FeedbackError::UnknownSkill { skill_id } if skill_id == unknown));
    assert_eq!(
        totals(&store),
        baseline,
        "unknown skill must not create orphan records"
    );
    drop(store);
    let path = root.path().to_path_buf();
    drop(root);
    assert!(!path.exists());
}

/// The exact-secret list is only the last line of defence: operators paste
/// credentials that nobody configured. Assert on unconfigured credential
/// shapes, and keep a negative control that must survive redaction so the
/// patterns cannot pass by deleting everything.
#[test]
fn feedback_redaction_removes_unconfigured_credential_shapes() {
    let redactor = Redactor::new(vec![], 4096);
    let control = "the tokenizer emitted 12 tokens and the run returned 200";
    for (leak, secret) in [
        (
            r#"{"password":"json-password-canary"}"#,
            "json-password-canary",
        ),
        (
            r#"{'api_key': 'single-quoted-canary'}"#,
            "single-quoted-canary",
        ),
        (r#"password="first second; tail-canary""#, "tail-canary"),
        (
            r#"{"client_secret":"first\"escaped-tail-canary"}"#,
            "escaped-tail-canary",
        ),
        (
            r#"password='first\'escaped-single-tail-canary'"#,
            "escaped-single-tail-canary",
        ),
        ("password=\"unterminated tail-canary", "tail-canary"),
        (
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345",
            "abcdefghijklmnopqrstuvwxyz012345",
        ),
        (
            "called with Bearer abcdefghijklmnopqrstuvwxyz012345 attached",
            "abcdefghijklmnopqrstuvwxyz012345",
        ),
        (
            "used sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 for the call",
            "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
        ),
        (
            "aws key AKIAIOSFODNN7EXAMPLE was embedded",
            "AKIAIOSFODNN7EXAMPLE",
        ),
        (
            "ghp_0123456789abcdefghijABCDEFGHIJ leaked",
            "ghp_0123456789abcdefghijABCDEFGHIJ",
        ),
        (
            "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAKQ\n-----END RSA PRIVATE KEY-----",
            "MIIBOgIBAAJBAKQ",
        ),
    ] {
        let redacted = redactor.redact(&format!("{control} :: {leak}"));
        assert!(!redacted.contains(secret), "leaked {secret} in {redacted}");
        assert!(redacted.contains("[REDACTED"), "{redacted}");
        // Negative control: ordinary prose that merely mentions tokens must
        // survive untouched.
        assert!(
            redacted.contains("the tokenizer emitted 12 tokens"),
            "{redacted}"
        );
        assert!(redacted.contains("returned 200"), "{redacted}");
    }
    for (input, expected) in [
        (
            r#"before {"password":"first\"second; third","count":2} after"#,
            r#"before {"password":"[REDACTED]","count":2} after"#,
        ),
        (
            "before password='first second' after",
            "before password='[REDACTED]' after",
        ),
        ("password: 'first'''", "password: '[REDACTED]'"),
        ("password: '' after", "password: '[REDACTED]' after"),
        ("password=\"unfinished phrase", "password=\"[REDACTED]\""),
        ("password='unfinished phrase", "password='[REDACTED]'"),
        (r#"password="unfinished\"#, "password=\"[REDACTED]\""),
        (r#"password='unfinished\"#, "password='[REDACTED]'"),
        ("token=one; status=ok", "token=[REDACTED]; status=ok"),
    ] {
        assert_eq!(redactor.redact(input), expected);
    }
    // YAML treats backslashes literally in single-quoted scalars. A run of
    // either parity must not consume half of a doubled-quote escape.
    for backslashes in 0..=4 {
        for ending in ["' after", ""] {
            let input = format!(
                "before password: 'first{}''second; tail{ending}",
                "\\".repeat(backslashes)
            );
            let suffix = if ending.is_empty() { "" } else { " after" };
            assert_eq!(
                redactor.redact(&input),
                format!("before password: '[REDACTED]'{suffix}"),
                "{input}"
            );
        }
    }
    for (limit, expected) in [(0, ""), (1, ""), (2, "é"), (3, "é")] {
        assert_eq!(
            Redactor::new(vec![], limit).redact("éé password='private value'"),
            expected
        );
    }
}

#[test]
fn feedback_redaction_recognizes_escaped_json_credential_names() {
    let redactor = Redactor::new(vec![], 4096);
    for label in [
        "API_KEY",
        "api-key",
        "apikey",
        "access_token",
        "access-token",
        "accessToken",
        "refresh_token",
        "refresh-token",
        "refreshToken",
        "id_token",
        "id-token",
        "idToken",
        "client_secret",
        "client-secret",
        "clientSecret",
        "private_key",
        "private-key",
        "privateKey",
        "token",
        "password",
        "PASSWD",
        "secret",
        "Authorization",
    ] {
        // Escape each position independently, then the whole name. These are
        // equivalent JSON keys, including optional separators and uppercase.
        for escaped_at in (0..label.len()).map(Some).chain(std::iter::once(None)) {
            let encoded: String = label
                .bytes()
                .enumerate()
                .map(|(index, byte)| {
                    if escaped_at.is_none_or(|position| position == index) {
                        format!(r"\u{byte:04X}")
                    } else {
                        char::from(byte).to_string()
                    }
                })
                .collect();
            let input = format!(r#"{{"{encoded}":"key-canary","count":2}}"#);
            let parsed: serde_json::Value = serde_json::from_str(&input).unwrap();
            assert_eq!(
                parsed[label], "key-canary",
                "the fixture must encode {label}"
            );
            assert_eq!(
                redactor.redact(&input),
                format!(r#"{{"{encoded}":"[REDACTED]","count":2}}"#),
                "{input}"
            );
        }
    }
    for (input, expected) in [
        (
            r#"before {"pass\u0077ord":"escaped-key-canary","count":2} after"#,
            r#"before {"pass\u0077ord":"[REDACTED]","count":2} after"#,
        ),
        (
            r#"{"payload":{"\u0070assword":"nested-canary"},"count":2}"#,
            r#"{"payload":{"\u0070assword":"[REDACTED]"},"count":2}"#,
        ),
        (
            r#"{"X-API-Key":"header-canary","X-API-\u004Bey":"escaped-header-canary"}"#,
            r#"{"X-API-Key":"[REDACTED]","X-API-\u004Bey":"[REDACTED]"}"#,
        ),
        (
            r#"{"pass\u0077ord_hint":"safe","tokenizer":"safe","notasecret":"safe","\\u0070assword":"safe"}"#,
            r#"{"pass\u0077ord_hint":"safe","tokenizer":"safe","notasecret":"safe","\\u0070assword":"safe"}"#,
        ),
    ] {
        assert_eq!(redactor.redact(input), expected);
    }
}

#[test]
fn feedback_redaction_keeps_shape_detection_intact_with_configured_secrets() {
    for (secret, input, expected) in [
        (
            "SPLIT",
            "password=beforeSPLITafter; status=ok",
            "password=[REDACTED]; status=ok",
        ),
        (
            "word",
            "password=label-canary; status=ok",
            "pass[REDACTED]=[REDACTED]; status=ok",
        ),
        (
            "sk-",
            "used sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789; status=ok",
            "used [REDACTED]; status=ok",
        ),
        (
            "BEGIN",
            "-----BEGIN PRIVATE KEY-----\nPRIVATE-BODY-CANARY\n-----END PRIVATE KEY-----; status=ok",
            "[REDACTED PRIVATE KEY]; status=ok",
        ),
        (
            "standalone-canary",
            "used standalone-canary; status=ok",
            "used [REDACTED]; status=ok",
        ),
    ] {
        let redactor = Redactor::new(vec![secret.into()], 4096);
        assert_eq!(redactor.redact(input), expected, "configured {secret}");
    }
}

#[test]
fn stored_feedback_text_never_retains_an_unconfigured_credential() {
    let (_root, mut store, skill, invocation) = fixture();
    let actor = owner(&skill.id);
    let mut command = negative_command(&skill.id, &invocation, "text-leak");
    command.reason_text = Some(
        r#"wrong output; repro used {"password":"feedback first; feedback-tail-canary","pass\u0077ord":"ESCAPED-KEY-CANARY"} and password: 'first''YAML-PAST-QUOTE-CANARY' and Authorization: Bearer abcdefghijklmnop012345"#.into(),
    );
    let id = FeedbackService::new(&mut store, Redactor::new(vec![], 512))
        .submit(&actor, &command, 2)
        .unwrap();
    let stored: String = store
        .conn()
        .query_row(
            "SELECT COALESCE(reason_text, '') FROM skill_feedback WHERE feedback_id = ?",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!stored.contains("abcdefghijklmnop012345"), "{stored}");
    assert!(!stored.contains("feedback first"), "{stored}");
    assert!(!stored.contains("feedback-tail-canary"), "{stored}");
    assert!(!stored.contains("YAML-PAST-QUOTE-CANARY"), "{stored}");
    assert!(!stored.contains("ESCAPED-KEY-CANARY"), "{stored}");
    assert!(stored.contains("wrong output"), "{stored}");
}

#[test]
fn severe_feedback_names_the_rejected_code_and_lists_the_accepted_ones() {
    let (_root, mut store, skill, invocation) = fixture();
    let actor = AuthenticatedActor {
        actor_id: "reviewer".into(),
        kind: ActorKind::Reviewer,
        allowed_skill_ids: Some(BTreeSet::from([skill.id.clone()])),
    };
    let mut command = negative_command(&skill.id, &invocation, "severe-invalid");
    command.kind = FeedbackKind::Severe;
    command.reason_code = "incorrect_output".into();
    let error = FeedbackService::new(&mut store, Redactor::new(vec![], 512))
        .submit(&actor, &command, 2)
        .unwrap_err();
    let message = error.to_string();
    assert!(
        matches!(error, FeedbackError::UnsupportedSevereReasonCode { .. }),
        "{message}"
    );
    assert!(message.contains("incorrect_output"), "{message}");
    for accepted in SEVERE_FEEDBACK_REASON_CODES {
        assert!(
            message.contains(accepted),
            "{accepted} missing from {message}"
        );
    }
    // The same code is an ordinary negative report, and must be accepted.
    let ordinary = negative_command(&skill.id, &invocation, "severe-invalid-as-negative");
    FeedbackService::new(&mut store, Redactor::new(vec![], 512))
        .submit(&actor, &ordinary, 2)
        .unwrap();
}

#[test]
fn out_of_bounds_feedback_fields_are_named_with_the_rule_they_broke() {
    let (_root, mut store, skill, invocation) = fixture();
    let actor = owner(&skill.id);
    let base = negative_command(&skill.id, &invocation, "bounds");
    let mut empty_key = base.clone();
    empty_key.idempotency_key = String::new();
    let mut spaced_key = base.clone();
    spaced_key.idempotency_key = "key with spaces".into();
    let mut long_key = base.clone();
    long_key.idempotency_key = "k".repeat(129);
    let mut mixed_case_code = base.clone();
    mixed_case_code.reason_code = "Incorrect-Output".into();
    let mut long_code = base.clone();
    long_code.reason_code = "r".repeat(65);
    let mut long_text = base.clone();
    long_text.reason_text = Some("x".repeat(513));
    for (field, command) in [
        ("idempotency_key", empty_key),
        ("idempotency_key", spaced_key),
        ("idempotency_key", long_key),
        ("reason_code", mixed_case_code),
        ("reason_code", long_code),
        ("reason_text", long_text),
    ] {
        let error = FeedbackService::new(&mut store, Redactor::new(vec![], 512))
            .submit(&actor, &command, 2)
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains(field), "{field}: {message}");
        assert!(
            matches!(error, FeedbackError::InvalidFeedback { field: named, .. } if named == field),
            "{field}: {message}"
        );
    }
    // A key that uses the whole accepted charset and near-maximum length is
    // still accepted, so the bounds are not simply rejecting everything.
    let mut accepted = base.clone();
    accepted.idempotency_key = format!("ops.run:2026-09-07-{}", "a".repeat(100));
    assert_eq!(accepted.idempotency_key.len(), 119);
    FeedbackService::new(&mut store, Redactor::new(vec![], 512))
        .submit(&actor, &accepted, 2)
        .unwrap();
}

#[test]
fn misshapen_skill_and_invocation_ids_are_format_errors_not_missing_targets() {
    let (_root, mut store, skill, invocation) = fixture();
    let actor = AuthenticatedActor {
        actor_id: "owner".into(),
        kind: ActorKind::Owner,
        allowed_skill_ids: None,
    };
    let base = negative_command(&skill.id, &invocation, "shape");
    let mut uppercase_invocation = base.clone();
    uppercase_invocation.invocation_id = Some(invocation.to_uppercase());
    let mut typo_invocation = base.clone();
    typo_invocation.invocation_id = Some("not-a-hex-invocation".into());
    let mut short_skill = base.clone();
    short_skill.skill_id = "abc123".into();
    for (field, command) in [
        ("invocation_id", uppercase_invocation),
        ("invocation_id", typo_invocation),
        ("skill_id", short_skill),
    ] {
        let error = FeedbackService::new(&mut store, Redactor::new(vec![], 512))
            .submit(&actor, &command, 2)
            .unwrap_err();
        let message = error.to_string();
        assert!(
            matches!(error, FeedbackError::MalformedId { field: named, .. } if named == field),
            "{field}: {message}"
        );
        assert!(message.contains(field), "{field}: {message}");
        assert!(message.contains("64 lowercase hexadecimal"), "{message}");
    }
}

#[test]
fn feedback_for_a_compacted_invocation_reports_the_retention_window() {
    let (_root, mut store, skill, invocation) = fixture();
    // The fixture's `invoked` event is at t=1; compaction rolls it into the
    // daily aggregate and deletes the raw row, exactly as the automatic
    // post-ingest compaction does after the raw retention window.
    let report = RetentionService::new(&mut store)
        .compact_before(2, 1, 3)
        .unwrap();
    assert_eq!(report.compacted_events, 1);

    let actor = owner(&skill.id);
    let command = negative_command(&skill.id, &invocation, "after-compaction");
    let error = FeedbackService::new(&mut store, Redactor::new(vec![], 512))
        .submit(&actor, &command, 4)
        .unwrap_err();
    let message = error.to_string();
    assert!(
        matches!(
            error,
            FeedbackError::UnknownInvocation { retention_days, .. }
                if retention_days == RAW_TELEMETRY_RETENTION_DAYS
        ),
        "{message}"
    );
    assert!(message.contains("compacted"), "{message}");
    assert!(message.contains("30 days"), "{message}");
    assert!(!message.contains("different learned skill"), "{message}");
}

#[test]
fn feedback_aimed_at_another_skills_invocation_is_distinguished_from_compaction() {
    let (_root, mut store, skill, _invocation) = fixture();
    let other = SkillArtifact::new(
        "function run() { return 2; }".into(),
        "Other feedback fixture".into(),
        vec![],
        vec![SkillExport {
            name: "run".into(),
            signature: "() => number".into(),
        }],
        // Embedded tests must evaluate to a boolean; a truthy number is
        // rejected by the verifier.
        vec!["run() === 2".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    store.insert_verified(&other).unwrap();
    let other_invocation = stable_invocation_id("turn-2", "tool-2", &other.id, "run", 0);
    TelemetryIngestor::new(&mut store)
        .ingest(
            &EventBatch::new(vec![SkillEvent {
                invocation_id: Some(other_invocation.clone()),
                skill_id: other.id.clone(),
                turn_id: "turn-2".into(),
                tool_call_id: Some("tool-2".into()),
                kind: SkillEventKind::Invoked,
                export_name: Some("run".into()),
                outcome: None,
                latency_us: None,
                retrieval_score: None,
                retrieval_rank: None,
                query_fingerprint: None,
                index_generation: 0,
                evidence_complete: true,
                production: true,
                argument_shape: None,
                created_at: 1,
            }])
            .unwrap(),
        )
        .unwrap();

    let actor = owner(&skill.id);
    let command = negative_command(&skill.id, &other_invocation, "cross-target");
    let error = FeedbackService::new(&mut store, Redactor::new(vec![], 512))
        .submit(&actor, &command, 2)
        .unwrap_err();
    let message = error.to_string();
    assert!(
        matches!(error, FeedbackError::InvocationSkillMismatch { .. }),
        "{message}"
    );
    assert!(message.contains("different learned skill"), "{message}");
    assert!(!message.contains("compacted"), "{message}");
}

#[test]
fn a_reused_idempotency_key_with_a_different_payload_writes_nothing() {
    let (_root, mut store, skill, invocation) = fixture();
    let actor = owner(&skill.id);
    let first = negative_command(&skill.id, &invocation, "duplicate-key");
    FeedbackService::new(&mut store, Redactor::new(vec![], 512))
        .submit(&actor, &first, 2)
        .unwrap();
    let mut second = first.clone();
    second.reason_code = "slow_response".into();
    let error = FeedbackService::new(&mut store, Redactor::new(vec![], 512))
        .submit(&actor, &second, 3)
        .unwrap_err();
    assert!(
        matches!(error, FeedbackError::IdempotencyConflict),
        "{error}"
    );
    let (rows, stored): (i64, String) = store
        .conn()
        .query_row(
            "SELECT COUNT(*), MAX(reason_code) FROM skill_feedback
             WHERE idempotency_key = 'duplicate-key'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((rows, stored.as_str()), (1, "incorrect_output"));
}

#[test]
fn user_feedback_is_counted_in_the_stats_the_store_keeps() {
    let (_root, mut store, skill, invocation) = fixture();
    let actor = owner(&skill.id);
    let negative = negative_command(&skill.id, &invocation, "counted-negative");
    let mut severe = negative_command(&skill.id, &invocation, "counted-severe");
    severe.kind = FeedbackKind::Severe;
    severe.reason_code = "integrity".into();
    let mut positive = negative_command(&skill.id, &invocation, "counted-positive");
    positive.kind = FeedbackKind::Positive;
    for command in [&negative, &severe, &positive] {
        FeedbackService::new(&mut store, Redactor::new(vec![], 512))
            .submit(&actor, command, 5)
            .unwrap();
    }
    let counts: (i64, i64) = store
        .conn()
        .query_row(
            "SELECT user_negative_count, user_positive_count FROM skill_stats
             WHERE skill_id = ?",
            [&skill.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(counts, (2, 1));
}

fn owner(skill_id: &str) -> AuthenticatedActor {
    AuthenticatedActor {
        actor_id: "owner".into(),
        kind: ActorKind::Owner,
        allowed_skill_ids: Some(BTreeSet::from([skill_id.to_string()])),
    }
}

fn negative_command(skill_id: &str, invocation_id: &str, key: &str) -> FeedbackCommand {
    FeedbackCommand {
        idempotency_key: key.to_string(),
        skill_id: skill_id.to_string(),
        invocation_id: Some(invocation_id.to_string()),
        kind: FeedbackKind::Negative,
        reason_code: "incorrect_output".into(),
        reason_text: None,
    }
}

#[test]
fn quarantine_evidence_names_the_feedback_row_it_came_from() {
    use crate::extras::js::skills::quarantine::{
        FeedbackAttribution, QuarantineDecision, QuarantineEvidence, QuarantinePolicy,
        QuarantineReason, evaluate, evaluate_with_attribution,
    };

    let policy = QuarantinePolicy::conservative("phase5-quarantine-v1");
    let evidence = QuarantineEvidence {
        skill_id: "skill".into(),
        reason: QuarantineReason::AuthenticatedActiveIntegrityFeedback,
        qualified_invocations: 0,
        direct_failures: 0,
        evidence_complete: true,
        authenticated_feedback: true,
        feedback_marked_severe: true,
        row_version_current: true,
        generation_current: true,
    };
    let attribution = FeedbackAttribution::new("feedback-abc", "permission_violation");
    let QuarantineDecision::Quarantine { canonical_snapshot } =
        evaluate_with_attribution(&policy, &evidence, Some(&attribution))
    else {
        panic!("attributed feedback quarantine was held");
    };
    let parsed: serde_json::Value = serde_json::from_str(&canonical_snapshot).unwrap();
    // The reason enum is derived from lifecycle status and cannot say what was
    // reported, so the snapshot has to carry the submitted code and the row id.
    assert_eq!(
        parsed["evidence"]["reason"],
        "authenticated_active_integrity_feedback"
    );
    assert_eq!(parsed["feedback"]["feedback_id"], "feedback-abc");
    assert_eq!(parsed["feedback"]["reason_code"], "permission_violation");

    // Non-feedback quarantines keep their existing snapshot bytes, so their
    // evidence ids do not move.
    let QuarantineDecision::Quarantine {
        canonical_snapshot: unattributed,
    } = evaluate(&policy, &evidence)
    else {
        panic!("quarantine was held");
    };
    assert!(!unattributed.contains("feedback_id"), "{unattributed}");
    assert_ne!(unattributed, canonical_snapshot);
}

#[test]
fn a_rejected_quarantine_transition_leaves_no_evidence_behind() {
    use std::sync::Arc;

    use crate::extras::js::skills::coordinator::IndexCoordinator;
    use crate::extras::js::skills::embed::Embedder;
    use crate::extras::js::skills::lifecycle::LifecycleStatus;
    use crate::extras::js::skills::quarantine::{
        FeedbackAttribution, QuarantineEvidence, QuarantineExecutionError, QuarantineExecutor,
        QuarantinePolicy, QuarantineReason,
    };

    let directory = super::TestTempDir::new("quarantine-rollback");
    let root = directory.path().to_path_buf();
    let paths = AppPaths::resolve(&PathEnvironment {
        platform: if cfg!(target_os = "macos") {
            PathPlatform::MacOs
        } else if cfg!(target_os = "windows") {
            PathPlatform::Windows
        } else {
            PathPlatform::Linux
        },
        home_dir: None,
        config_base: Some(root.clone()),
        data_base: Some(root.clone()),
        local_data_base: Some(root.clone()),
        state_base: Some(root.clone()),
        cache_base: Some(root.clone()),
        workspace_root: None,
        overrides: Default::default(),
    })
    .unwrap();
    let skill = SkillArtifact::new(
        "function run() { return 3; }".into(),
        "Quarantine rollback fixture".into(),
        vec![],
        vec![SkillExport {
            name: "run".into(),
            signature: "() => number".into(),
        }],
        vec!["run() === 3".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    let mut store = SkillStore::open_at(&paths).unwrap();
    store.insert_verified(&skill).unwrap();
    drop(store);
    let coordinator = IndexCoordinator::open(&paths, Arc::new(Embedder::new().unwrap())).unwrap();
    let generation = coordinator.rebuild_and_publish().unwrap();

    let evidence = QuarantineEvidence {
        skill_id: skill.id.clone(),
        reason: QuarantineReason::AuthenticatedActiveIntegrityFeedback,
        qualified_invocations: 0,
        direct_failures: 0,
        evidence_complete: true,
        authenticated_feedback: true,
        feedback_marked_severe: true,
        row_version_current: true,
        generation_current: true,
    };
    let attribution = FeedbackAttribution::new("feedback-stale", "integrity");
    // The expected row version is stale, so the lifecycle transition rejects
    // the decision after the evidence row has already been committed by its
    // own autocommit statement.
    let error = QuarantineExecutor::new(&coordinator)
        .apply_with_attribution(
            &QuarantinePolicy::conservative("phase5-quarantine-rollback"),
            &evidence,
            Some(&attribution),
            LifecycleStatus::Active,
            999,
            generation as i64,
            10,
        )
        .unwrap_err();
    assert!(
        matches!(error, QuarantineExecutionError::Lifecycle(_)),
        "{error}"
    );
    let store = SkillStore::open_at(&paths).unwrap();
    let evidence_rows: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM skill_evidence WHERE evidence_kind = 'quarantine'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(evidence_rows, 0);
    let status: String = store
        .conn()
        .query_row(
            "SELECT status FROM skill_revisions WHERE id = ?",
            [&skill.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(status, "quarantined");
    drop(store);
    drop(coordinator);
    drop(directory);
    assert!(!root.exists());
}
