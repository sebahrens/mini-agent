use crate::session::{MessageRole, Session};

#[test]
fn estimate_tokens_empty() {
    // Empty string returns min of 1
    assert_eq!(Session::estimate_tokens(""), 1);
}

#[test]
fn estimate_tokens_short() {
    // 3 chars → 3/4 = 0, but min 1
    assert_eq!(Session::estimate_tokens("abc"), 1);
}

#[test]
fn estimate_tokens_exact_divisible() {
    assert_eq!(Session::estimate_tokens("abcd"), 1);
}

#[test]
fn estimate_tokens_rounds_down() {
    assert_eq!(Session::estimate_tokens("abcde"), 1);
}

#[test]
fn estimate_tokens_long() {
    assert_eq!(Session::estimate_tokens(&"x".repeat(100)), 30);
}

#[test]
fn estimate_tokens_cjk_not_undercounted_like_chars_div4() {
    let text = "今天天氣很好真開心"; // 9 chars
    let est = Session::estimate_tokens(text);
    assert_eq!(est, 8); // 9 * 9 / 10 = 8
    assert!(est > (text.chars().count() as u64 / 4));
}

#[test]
fn estimate_tokens_mixed_cjk_and_latin() {
    let text = "請幫我 refactor this function 好嗎";
    let wide = text
        .chars()
        .filter(|c| {
            let o = *c as u32;
            (0x2E80..=0x9FFF).contains(&o)
        })
        .count() as u64;
    let est = Session::estimate_tokens(text);
    assert!(est >= wide * 9 / 10);
}

#[test]
fn estimate_tokens_ascii_uses_code_safe_divisor() {
    let text = "the quick brown fox jumps over the lazy dog";
    assert_eq!(Session::estimate_tokens(text), text.len() as u64 * 4 / 13);
}

#[test]
fn estimate_tokens_code_fixture_has_documented_pre_calibration_margin() {
    let code = r#"{"users":[{"id":1,"active":true},{"id":2,"active":false}],"next":null}"#;
    let old_chars_over_four = code.chars().count() as u64 / 4;

    assert!(Session::estimate_tokens(code) > old_chars_over_four);
    assert_eq!(
        Session::estimate_tokens(code),
        code.chars().count() as u64 * 4 / 13
    );
}

#[test]
fn effective_context_falls_back_without_calibration() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    s.add_message(MessageRole::User, "hello world this is a test message");
    assert_eq!(s.effective_context_tokens(), s.total_estimated_tokens);
}

#[test]
fn effective_context_uses_calibration_anchor_plus_delta() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    s.add_message(MessageRole::User, "first user message");
    s.add_message(MessageRole::Assistant, "assistant reply");
    s.set_calibration(5000, 200); // anchor = 5200, covers 2 messages
    assert_eq!(s.calibrated_msg_count, 2);

    s.add_message(MessageRole::User, "a follow up question");
    let delta = Session::estimate_tokens("a follow up question");
    assert_eq!(s.effective_context_tokens(), 5200 + delta);
}

#[test]
fn calibration_ignores_zero_usage() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    s.add_message(MessageRole::User, "msg");
    s.set_calibration(0, 0);
    assert_eq!(s.calibrated_tokens, 0);
    assert_eq!(s.effective_context_tokens(), s.total_estimated_tokens);
}

#[test]
fn terminal_message_can_be_reanchored_without_double_counting_reported_output() {
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.add_message(MessageRole::User, "prompt");
    session.set_calibration(100, 20);
    session.add_message(MessageRole::Assistant, "reported output");
    assert!(session.effective_context_tokens() > 120);

    session.reanchor_calibration_to_current_messages();

    assert_eq!(session.effective_context_tokens(), 120);
    assert_eq!(session.calibrated_msg_count, session.messages.len());
}

#[test]
fn real_input_tokens_native_route_adds_cache_fields() {
    // The Anthropic-native route reports input_tokens excluding cached/
    // cache-creation tokens, so the real prompt size is the sum of all three.
    // A cache hit (input ~0, cached large) must NOT collapse the measured context.
    assert_eq!(Session::real_input_tokens(true, 10, 0, 0, 7000, 0), 7010);
    assert_eq!(Session::real_input_tokens(true, 0, 0, 0, 7000, 0), 7000);
    assert_eq!(Session::real_input_tokens(true, 50, 0, 0, 0, 6000), 6050);
}

#[test]
fn real_input_tokens_non_native_uses_cache_inclusive_input_only() {
    // OpenAI/Gemini/OpenRouter fold the cached subset into input_tokens and
    // report no cache-creation; adding the cache fields would double-count.
    assert_eq!(
        Session::real_input_tokens(false, 7000, 7010, 10, 5600, 0),
        7000
    );
}

#[test]
fn real_input_tokens_non_native_recovers_separately_reported_cache_from_total() {
    // Some compatible gateways expose uncached input and cached components
    // separately while retaining their sum in total_tokens.
    assert_eq!(
        Session::real_input_tokens(false, 400, 1110, 10, 600, 100),
        1100
    );
}

#[test]
fn billable_input_tokens_native_prices_cache_tiers() {
    use crate::pricing::billable_input_tokens;
    // Cost sibling of real_input_tokens: Anthropic bills cache writes at 1.25×
    // and cache reads at 0.10× the base input rate, on top of raw input_tokens.
    // write only: 100 + 6000*1.25 = 7600
    assert_eq!(billable_input_tokens(true, 100, 0, 6000), 7600);
    // read only: 100 + 7000*0.10 = 800
    assert_eq!(billable_input_tokens(true, 100, 7000, 0), 800);
    // both, with rounding: 4 + 6003*0.10 + 6089*1.25 = 4 + 600.3 + 7611.25 = 8215.55 -> 8216
    assert_eq!(billable_input_tokens(true, 4, 6003, 6089), 8216);
}

#[test]
fn billable_input_tokens_non_native_uses_input_only() {
    use crate::pricing::billable_input_tokens;
    // Non-Anthropic providers have no separate cache tiers to price — input_tokens
    // is already the full billable amount, so the cache fields are ignored.
    assert_eq!(billable_input_tokens(false, 7000, 5600, 4000), 7000);
}

#[test]
fn charge_usage_delta_updates_persisted_token_cache_and_cost_totals_together() {
    let mut session = Session::new("anthropic", "claude-sonnet", 200_000, "");
    session.input_token_cost = 2.0;
    session.output_token_cost = 10.0;
    let delta = crate::event::UsageDelta {
        input_tokens: 100,
        output_tokens: 25,
        cached_input_tokens: 700,
        cache_creation_input_tokens: 80,
        ..crate::event::UsageDelta::default()
    };

    session.charge_usage_delta(delta, true);

    assert_eq!(session.total_input_tokens, 100);
    assert_eq!(session.total_output_tokens, 25);
    assert_eq!(session.total_cached_input_tokens, 700);
    assert_eq!(session.total_cache_creation_input_tokens, 80);
    assert_eq!(session.total_real_input_tokens, 880);
    let billable_input = crate::pricing::billable_input_tokens(true, 100, 700, 80);
    let expected = crate::pricing::estimate_cost(billable_input, 25, 2.0, 10.0);
    assert!((session.total_cost - expected).abs() < f64::EPSILON);
}

#[test]
fn charge_usage_delta_is_additive_without_hidden_terminal_charge() {
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.input_token_cost = 1.0;
    session.output_token_cost = 2.0;

    for delta in [
        crate::event::UsageDelta {
            input_tokens: 10,
            output_tokens: 2,
            ..crate::event::UsageDelta::default()
        },
        crate::event::UsageDelta {
            input_tokens: 20,
            output_tokens: 4,
            ..crate::event::UsageDelta::default()
        },
    ] {
        session.charge_usage_delta(delta, false);
    }

    assert_eq!(session.total_input_tokens, 30);
    assert_eq!(session.total_output_tokens, 6);
    assert_eq!(session.total_real_input_tokens, 30);
    assert_eq!(
        session.total_cost,
        crate::pricing::estimate_cost(30, 6, 1.0, 2.0)
    );
}

#[test]
fn charge_usage_delta_saturates_persisted_token_totals() {
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.total_input_tokens = u64::MAX - 1;
    session.total_output_tokens = u64::MAX - 1;
    session.total_cached_input_tokens = u64::MAX - 1;
    session.total_cache_creation_input_tokens = u64::MAX - 1;
    session.total_real_input_tokens = u64::MAX - 1;

    session.charge_usage_delta(
        crate::event::UsageDelta {
            input_tokens: 10,
            output_tokens: 10,
            cached_input_tokens: 10,
            cache_creation_input_tokens: 10,
            ..crate::event::UsageDelta::default()
        },
        false,
    );

    assert_eq!(session.total_input_tokens, u64::MAX);
    assert_eq!(session.total_output_tokens, u64::MAX);
    assert_eq!(session.total_cached_input_tokens, u64::MAX);
    assert_eq!(session.total_cache_creation_input_tokens, u64::MAX);
    assert_eq!(session.total_real_input_tokens, u64::MAX);
}

// Helper: a session with `n` ASCII messages of `len` chars each, so every
// message has the same predictable narrow-text estimate.
fn session_with_messages(n: usize, len: usize) -> Session {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    for _ in 0..n {
        s.add_message(MessageRole::User, &"x".repeat(len));
    }
    s
}

#[test]
fn compaction_cut_keeps_recent_within_budget() {
    // 4 messages × 12 tokens = 48 total. keep_recent=15 reaches back across
    // the last two (24 tokens), so the first two are summarized.
    let s = session_with_messages(4, 40);
    assert_eq!(
        s.messages[0].estimated_tokens,
        Session::estimate_tokens(&"x".repeat(40))
    );
    assert_eq!(Session::select_compaction_cut(&s.messages, 15), 2);
}

#[test]
fn compaction_cut_oversized_keep_recent_summarizes_nothing() {
    // Regression: keep_recent (100) larger than the whole history must
    // keep the recent messages, NOT summarize everything (cut == 0, which the
    // caller treats as "entire context is recent").
    let s = session_with_messages(4, 40);
    assert_eq!(Session::select_compaction_cut(&s.messages, 100), 0);
}

#[test]
fn compaction_cut_zero_keep_recent_summarizes_all() {
    let s = session_with_messages(4, 40);
    assert_eq!(Session::select_compaction_cut(&s.messages, 0), 4);
}

#[test]
fn compaction_cut_single_message_is_kept() {
    let s = session_with_messages(1, 40);
    assert_eq!(Session::select_compaction_cut(&s.messages, 5), 0);
}

#[test]
fn compaction_cut_never_orphans_a_tool_result() {
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.add_message(MessageRole::User, "question");
    session.add_message(MessageRole::Assistant, "I will inspect it");
    session.add_tool_call("read", &serde_json::json!({"path": "large"}));
    session.add_tool_result("read", &"x".repeat(500));
    session.add_message(MessageRole::Assistant, "answer");

    let cut = Session::select_compaction_cut(&session.messages, 45);
    assert!(matches!(
        session.messages[cut].role,
        MessageRole::User | MessageRole::Assistant
    ));
    assert_ne!(session.messages[cut].role, MessageRole::ToolResult);
}

#[test]
fn new_session_has_id() {
    let s = Session::new("openai", "gpt-4", 128000, "");
    assert!(!s.id.is_empty());
}

#[test]
fn new_session_sets_provider_and_model() {
    let s = Session::new("anthropic", "claude-sonnet", 200000, "");
    assert_eq!(s.provider.as_str(), "anthropic");
    assert_eq!(s.model.as_str(), "claude-sonnet");
}

#[test]
fn new_session_sets_context_window() {
    let s = Session::new("openai", "gpt-4", 128000, "");
    assert_eq!(s.context_window, 128000);
}

#[test]
fn new_session_sets_working_dir() {
    let s = Session::new("openai", "gpt-4", 128000, "");
    assert!(!s.working_dir.is_empty());
}

#[test]
fn new_session_has_timestamps() {
    let s = Session::new("openai", "gpt-4", 128000, "");
    assert!(!s.created_at.is_empty());
    assert!(!s.updated_at.is_empty());
}

#[test]
fn new_session_starts_empty() {
    let s = Session::new("openai", "gpt-4", 128000, "");
    assert!(s.messages.is_empty());
    assert!(s.compactions.is_empty());
    assert_eq!(s.total_estimated_tokens, 0);
    assert_eq!(s.total_input_tokens, 0);
    assert_eq!(s.total_output_tokens, 0);
    assert_eq!(s.total_cost, 0.0);
}

#[test]
fn add_message_appends() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    s.add_message(MessageRole::User, "hello");
    assert_eq!(s.messages.len(), 1);
    assert_eq!(s.messages[0].role, MessageRole::User);
    assert_eq!(s.messages[0].content, "hello");
}

#[test]
fn add_message_increments_estimated_tokens() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    let before = s.total_estimated_tokens;
    s.add_message(MessageRole::Assistant, "hello world, this is a test");
    assert!(s.total_estimated_tokens > before);
}

#[test]
fn tool_history_preserves_matching_internal_call_identity() {
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.add_tool_call_with_id(
        "call-42",
        "read",
        &serde_json::json!({"path": "src/main.rs"}),
    );
    session.add_tool_result_with_id("call-42", "read", "contents");

    assert_eq!(session.messages[0].tool_call_id.as_deref(), Some("call-42"));
    assert_eq!(session.messages[1].tool_call_id.as_deref(), Some("call-42"));
    assert_eq!(session.messages[0].role, MessageRole::ToolCall);
    assert_eq!(session.messages[1].role, MessageRole::ToolResult);
}

#[test]
fn add_message_updates_updated_at() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    let before = s.updated_at.clone();
    // Brief sleep to ensure timestamp changes
    std::thread::sleep(std::time::Duration::from_millis(1));
    s.add_message(MessageRole::User, "hi");
    assert!(s.updated_at != before);
}

#[test]
fn update_context_window_changes_value() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    s.update_context_window(256000);
    assert_eq!(s.context_window, 256000);
}

#[test]
fn compacted_context_returns_none_without_compactions() {
    let s = Session::new("openai", "gpt-4", 128000, "");
    let (summary, index) = s.compacted_context();
    assert!(summary.is_none());
    assert_eq!(index, 0);
}

#[test]
fn compress_adds_compaction_entry() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    s.add_message(MessageRole::User, "msg1");
    s.add_message(MessageRole::Assistant, "msg2");
    s.add_message(MessageRole::User, "msg3");
    s.add_message(MessageRole::Assistant, "msg4");

    let _before_count = s.messages.len();
    s.compress("summary text".to_string(), 2, 50);
    assert!(s.compactions.len() == 1);
    assert_eq!(s.compactions[0].summary, "summary text");
}

#[test]
fn compress_inserts_summary_as_system_message() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    s.add_message(MessageRole::User, "msg1");
    s.add_message(MessageRole::Assistant, "msg2");
    s.add_message(MessageRole::User, "msg3");

    s.compress("compressed summary".to_string(), 2, 30);
    // First message should now be the summary as System
    assert_eq!(s.messages[0].role, MessageRole::System);
    assert_eq!(s.messages[0].content, "compressed summary");
}

#[test]
fn compress_drains_messages_before_first_kept_index() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    s.add_message(MessageRole::User, "msg1");
    s.add_message(MessageRole::Assistant, "msg2");
    s.add_message(MessageRole::User, "msg3");
    s.add_message(MessageRole::Assistant, "msg4");

    s.compress("summary".to_string(), 2, 30);
    // Messages before index 2 (0,1) should be removed, replaced by summary
    // After compression: summary + msg3 + msg4 (plus summary takes index 0)
    assert_eq!(s.messages.len(), 3);
    assert_eq!(s.messages[0].role, MessageRole::System);
    assert_eq!(s.messages[1].content, "msg3");
    assert_eq!(s.messages[2].content, "msg4");
}

#[test]
fn compaction_drain_len_covers_whole_cut_with_full_coverage() {
    assert_eq!(Session::compaction_drain_len(5, 5).unwrap(), 5);
}

#[test]
fn compaction_drain_len_keeps_unsummarized_tail_for_partial_coverage() {
    // Only the oldest three of five cut messages were summarized: drain
    // exactly those, never the unsummarized newer ones.
    assert_eq!(Session::compaction_drain_len(5, 3).unwrap(), 3);
    // A summarizer can never have covered more than the cut slice.
    assert_eq!(Session::compaction_drain_len(5, 9).unwrap(), 5);
}

#[test]
fn compaction_drain_len_rejects_empty_coverage() {
    // Draining nothing would insert a summary without shrinking the session
    // and re-trigger auto-compaction on every turn.
    assert!(Session::compaction_drain_len(5, 0).is_err());
}

#[test]
fn compacted_context_returns_summary_after_compress() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    s.add_message(MessageRole::User, "msg1");
    s.add_message(MessageRole::Assistant, "msg2");
    s.compress("the summary".to_string(), 1, 20);

    let (summary, index) = s.compacted_context();
    assert_eq!(summary, Some("the summary"));
    assert_eq!(index, 1);
}

#[test]
fn detect_git_branch_in_repo_returns_nonempty() {
    let b = Session::detect_git_branch(env!("CARGO_MANIFEST_DIR"));
    assert!(b.is_some(), "repo root should resolve a branch or commit");
    assert!(!b.unwrap().is_empty());
}

#[test]
fn detect_git_branch_outside_repo_is_none() {
    let p = std::env::temp_dir().join("mini-agent-definitely-not-a-repo-xyz123");
    assert!(Session::detect_git_branch(p.to_str().unwrap()).is_none());
}

#[tokio::test]
async fn detect_git_status_uses_the_bounded_async_runner() {
    let status = Session::detect_git_status(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).await;
    assert!(status.is_some(), "repository status should be available");
}

#[tokio::test]
async fn detect_git_status_outside_repo_is_none() {
    let directory = std::env::temp_dir().join(format!(
        "mini-agent-git-status-non-repo-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&directory).unwrap();
    assert!(Session::detect_git_status(&directory).await.is_none());
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn parse_porcelain_counts_changes_and_sync() {
    let out = "\
# branch.oid abc123
# branch.head main
# branch.upstream origin/main
# branch.ab +2 -1
1 M. N... 100644 100644 100644 aaa bbb staged.rs
1 .M N... 100644 100644 100644 aaa bbb modified.rs
1 .D N... 100644 100644 000000 aaa bbb deleted.rs
? untracked.rs
";
    let g = crate::session::Session::parse_porcelain(out);
    assert_eq!(g.staged, 1);
    assert_eq!(g.modified, 1);
    assert_eq!(g.deleted, 1);
    assert_eq!(g.untracked, 1);
    assert_eq!(g.ahead, 2);
    assert_eq!(g.behind, 1);
    assert!(g.is_dirty());
}

#[test]
fn rewind_to_truncates_and_records_restore_point() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    s.add_message(MessageRole::User, "first");
    s.add_message(MessageRole::Assistant, "reply");
    s.add_message(MessageRole::User, "second");
    s.add_message(MessageRole::Assistant, "reply2");

    let removed = s.rewind_to(2);

    assert_eq!(removed, 2);
    assert_eq!(s.messages.len(), 2);
    assert_eq!(s.messages[0].content, "first");
    assert_eq!(s.messages[1].content, "reply");
    assert!(s.rewind_undo.is_some());
    assert_eq!(
        s.total_estimated_tokens,
        s.messages.iter().map(|m| m.estimated_tokens).sum::<u64>()
    );
}

#[test]
fn rewind_and_redo_restore_compaction_metadata_after_restart_without_refunding_costs() {
    let mut session = Session::new("provider", "model", 128_000, "");
    session.add_message(MessageRole::User, "old question");
    session.add_message(MessageRole::Assistant, "old answer");
    session.compress("retained recap".into(), 2, 15);
    session.add_message(MessageRole::User, "new question");
    session.add_message(MessageRole::Assistant, "new answer");
    session.total_cost = 0.42;
    session.calibrated_tokens = 1200;
    session.calibrated_msg_count = session.messages.len();
    let records = serde_json::to_value(&session.compactions).unwrap();
    let removed = session.messages.len();
    assert_eq!(session.rewind_to(0), removed);
    assert!(session.compactions.is_empty());
    assert_eq!(session.compacted_context(), (None, 0));
    assert_eq!(session.total_cost, 0.42);
    let mut restored: Session =
        serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
    assert!(restored.redo());
    assert_eq!(
        serde_json::to_value(&restored.compactions).unwrap(),
        records
    );
    assert_eq!(restored.compacted_context(), (Some("retained recap"), 1));
    assert_eq!(restored.total_cost, 0.42);
    assert_eq!(restored.calibrated_tokens, 1200);
    assert_eq!(restored.calibrated_msg_count, restored.messages.len());
}

#[test]
fn rewind_to_at_or_past_end_is_a_noop() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    s.add_message(MessageRole::User, "only");

    assert_eq!(s.rewind_to(1), 0);
    assert_eq!(s.rewind_to(5), 0);
    assert_eq!(s.messages.len(), 1);
    assert!(s.rewind_undo.is_none());
}

#[test]
fn redo_restores_the_messages_a_rewind_removed() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    s.add_message(MessageRole::User, "first");
    s.add_message(MessageRole::Assistant, "reply");
    s.add_message(MessageRole::User, "second");
    let before: Vec<_> = s.messages.iter().map(|m| m.content.clone()).collect();
    let est_before = s.total_estimated_tokens;

    s.rewind_to(1);
    assert_eq!(s.messages.len(), 1);

    assert!(s.redo());
    let after: Vec<_> = s.messages.iter().map(|m| m.content.clone()).collect();
    assert_eq!(before, after);
    assert_eq!(s.total_estimated_tokens, est_before);
    // The restore point is consumed, so a second redo finds nothing.
    assert!(!s.redo());
}

#[test]
fn persisted_rewind_stores_only_the_removed_tail() {
    let mut session = session_with_messages(100, 2_000);
    let original_bytes = serde_json::to_vec(&session).unwrap().len();

    assert_eq!(session.rewind_to(90), 10);
    let rewound_bytes = serde_json::to_vec(&session).unwrap().len();

    assert!(
        rewound_bytes < original_bytes + original_bytes / 4,
        "redo state unexpectedly duplicated the retained prefix: {rewound_bytes} vs {original_bytes}"
    );
    assert!(session.redo());
    assert_eq!(session.messages.len(), 100);
}

#[test]
fn history_mutations_reset_repeated_read_tracking() {
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.initialize_read_tracker(true);
    session.add_message(MessageRole::User, "first");
    session.add_message(MessageRole::Assistant, "reply");

    assert!(
        session
            .read_tracker
            .track_read("src/lib.rs", 0, 50)
            .is_none()
    );
    assert!(
        session
            .read_tracker
            .track_read("src/lib.rs", 0, 50)
            .is_some()
    );
    session.compress("summary".to_string(), 1, 1);
    assert!(
        session
            .read_tracker
            .track_read("src/lib.rs", 0, 50)
            .is_none()
    );

    session.add_message(MessageRole::User, "second");
    assert!(
        session
            .read_tracker
            .track_read("src/lib.rs", 0, 50)
            .is_some()
    );
    session.rewind_to(1);
    assert!(
        session
            .read_tracker
            .track_read("src/lib.rs", 0, 50)
            .is_none()
    );
    assert!(
        session
            .read_tracker
            .track_read("src/lib.rs", 0, 50)
            .is_some()
    );
    assert!(session.redo());
    assert!(
        session
            .read_tracker
            .track_read("src/lib.rs", 0, 50)
            .is_none()
    );
}

#[test]
fn adding_a_message_invalidates_the_redo_point() {
    let mut s = Session::new("openai", "gpt-4", 128000, "");
    s.add_message(MessageRole::User, "first");
    s.add_message(MessageRole::Assistant, "reply");
    s.add_message(MessageRole::User, "second");

    s.rewind_to(1);
    assert!(s.rewind_undo.is_some());
    // Moving the conversation forward must drop the stale restore point.
    s.add_message(MessageRole::User, "a fresh direction");
    assert!(s.rewind_undo.is_none());
    assert!(!s.redo());
}

#[cfg(feature = "js")]
#[test]
fn javascript_scratch_is_process_local_and_never_serialized_with_a_session() {
    let session = Session::new("openai", "gpt-4", 128_000, "scratch-persistence");
    let scratch = session
        .js_session_state
        .for_workspace(std::path::Path::new("/workspace"));
    scratch
        .put(
            "private:key".into(),
            "{\"secret\":\"not-persisted\"}".into(),
        )
        .unwrap();

    let encoded = serde_json::to_string(&session).unwrap();
    assert!(!encoded.contains("private:key"));
    assert!(!encoded.contains("not-persisted"));

    let restored: Session = serde_json::from_str(&encoded).unwrap();
    assert_eq!(
        restored
            .js_session_state
            .for_workspace(std::path::Path::new("/workspace"))
            .get("private:key")
            .unwrap(),
        None
    );
}

#[test]
fn compaction_cut_may_stop_at_a_completed_tool_batch() {
    // mini-agent-sskdu.227: a model that never narrates leaves only
    // User, ToolCall, ToolResult, ... and must still have a legal cut.
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.add_message(MessageRole::User, "do the work");
    for i in 0..6 {
        let id = format!("a{i}");
        session.add_tool_call_with_id(&id, "read", &serde_json::json!({"path": "f"}));
        session.add_tool_result_with_id(&id, "read", &"x".repeat(400));
    }
    // A parallel batch: both calls precede both results.
    session.add_tool_call_with_id("p1", "read", &serde_json::json!({"path": "p1"}));
    session.add_tool_call_with_id("p2", "read", &serde_json::json!({"path": "p2"}));
    session.add_tool_result_with_id("p1", "read", &"x".repeat(400));
    session.add_tool_result_with_id("p2", "read", &"x".repeat(400));
    for keep in [1, 50, 150, 300, 500, 900] {
        let cut = Session::select_compaction_cut(&session.messages, keep);
        if cut == 0 {
            continue;
        }
        let mut open = std::collections::HashSet::new();
        for message in &session.messages[..cut] {
            let id = message.tool_call_id.as_deref().unwrap_or_default();
            match message.role {
                MessageRole::ToolCall => {
                    open.insert(id.to_string());
                }
                MessageRole::ToolResult => {
                    open.remove(id);
                }
                _ => {}
            }
        }
        assert!(open.is_empty(), "cut {cut} splits a call from its result");
    }
    let cut = Session::select_compaction_cut(&session.messages, 300);
    assert!(cut > 1, "a tool-only stretch must have a cut, got {cut}");
    assert_eq!(session.messages[cut].role, MessageRole::ToolCall);
}

#[test]
fn tui_compaction_plan_never_resummarises_only_the_previous_summary() {
    // mini-agent-sskdu.227: [summary, User, tool pairs...] must not cut at 1.
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.add_message(MessageRole::User, "old");
    session.add_message(MessageRole::Assistant, "old answer");
    session.compress("summary of old work".to_string(), 2, 0);
    session.add_message(MessageRole::User, &"y".repeat(400));
    for i in 0..4 {
        let id = format!("b{i}");
        session.add_tool_call_with_id(&id, "read", &serde_json::json!({"path": "f"}));
        session.add_tool_result_with_id(&id, "read", &"x".repeat(400));
    }
    for keep in [0, 10, 100, 250, 450] {
        let gate = crate::ui::slash::plan_tui_compaction(&session, false, false, 0, keep, 0);
        assert_ne!(
            gate,
            crate::ui::slash::TuiCompactionGate::Cut(1),
            "keep {keep}"
        );
    }
}

#[test]
fn compaction_count_keeps_growing_after_records_are_pruned() {
    // mini-agent-sskdu.228: callers detecting "a compaction happened" must use
    // compaction_count(); compactions.len() plateaus at the retention cap.
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    let mut last = 0;
    for round in 0..7 {
        session.add_message(MessageRole::User, &format!("q{round}"));
        session.add_message(MessageRole::Assistant, &format!("a{round}"));
        let cut = session.messages.len();
        session.compress(format!("summary {round}"), cut, 0);
        assert!(session.compaction_count() > last, "round {round}");
        last = session.compaction_count();
    }
    assert_eq!(session.compaction_count(), 7);
    assert!(session.compactions.len() < 7);
}

#[test]
fn recorded_token_savings_are_net_of_the_inserted_summary() {
    // mini-agent-sskdu.242
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.add_message(MessageRole::User, "tiny");
    session.add_message(MessageRole::Assistant, "tiny");
    session.add_message(MessageRole::User, "keep");
    let removed: u64 = session.messages[..2]
        .iter()
        .map(|m| m.estimated_tokens)
        .sum();
    session.compress("s".repeat(4000), 2, removed);
    assert_eq!(session.compactions[0].token_savings, 0);
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.add_message(MessageRole::User, &"w".repeat(4000));
    session.add_message(MessageRole::User, "keep");
    let removed = session.messages[0].estimated_tokens;
    session.compress("short".into(), 1, removed);
    let summary_tokens = session.messages[0].estimated_tokens;
    assert_eq!(
        session.compactions[0].token_savings,
        removed - summary_tokens
    );
}

#[test]
fn session_title_survives_compaction() {
    // mini-agent-sskdu.238
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.add_message(MessageRole::User, "Fix the parser race");
    session.add_message(MessageRole::Assistant, "ok");
    session.add_message(MessageRole::User, "yes");
    let before = session.list_title();
    session.compress("summary".into(), 2, 0);
    assert_eq!(session.list_title(), before);
    session.add_message(MessageRole::Assistant, "done");
    session.compress("summary 2".into(), session.messages.len() - 1, 0);
    assert_eq!(session.list_title(), "Fix the parser race");
}

#[test]
fn relief_continuation_prompt_is_not_the_users_turn() {
    // mini-agent-sskdu.237: undo and the title skip the host's continuation.
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.add_message(MessageRole::User, "Fix the parser race");
    session.add_message(MessageRole::Assistant, "working");
    session.add_message(
        MessageRole::User,
        crate::agent::runner::HEADLESS_CONTEXT_RELIEF_PROMPT,
    );
    session.add_message(MessageRole::Assistant, "done");
    session.compress("summary".into(), 2, 0);
    assert_eq!(session.list_title(), "Fix the parser race");
    // The only user-authored record was drained; undo finds none to rewind to
    // and must not treat the continuation as a turn.
    assert_eq!(crate::ui::slash::undo_last(&mut session), 0);
}

#[test]
fn previous_summary_for_the_next_compaction_is_the_prose_only() {
    // mini-agent-sskdu.235: machine-appended blocks are not recycled.
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.add_message(MessageRole::User, "look at a.rs");
    session.add_tool_call_with_id("r1", "read", &serde_json::json!({"path": "src/a.rs"}));
    session.add_tool_result_with_id("r1", "read", "1: fn a() {}");
    session.add_message(MessageRole::Assistant, "done");
    session.add_message(MessageRole::User, "next");
    session.compress("MODEL PROSE".into(), 4, 0);
    let stored = session.compactions.last().unwrap().summary.to_string();
    assert!(stored.len() > "MODEL PROSE".len(), "{stored}");
    assert_eq!(
        session.previous_summary_prose().as_deref(),
        Some("MODEL PROSE")
    );
}

#[test]
fn compaction_queues_the_replaced_transcript_for_a_sidecar() {
    // mini-agent-sskdu.244
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.add_message(MessageRole::User, "first");
    session.add_message(MessageRole::Assistant, "second");
    session.add_message(MessageRole::User, "kept");
    session.compress("summary".into(), 2, 0);
    let (number, messages) = session.pending_precompact.take().expect("queued");
    assert_eq!(number, 1);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content, "first");
}

/// The commit step shared by the TUI, headless and ACP compaction drivers:
/// only the summarized prefix is drained, the removed tokens are recorded, and
/// a leading summary record is skipped as summarizer input but still replaced.
#[test]
fn shared_compaction_commit_drains_only_the_summarized_prefix() {
    for (included, expect_kept) in [(4usize, 4usize), (2, 2), (9, 4)] {
        let mut s = session_with_messages(6, 40);
        let (skipped, input) = s.compaction_input(4);
        assert_eq!((skipped, input.len()), (0, 4));
        let per_message = Session::estimate_tokens(&"x".repeat(40));
        let mut staged = None;
        let (first_kept, tokens_before) = s
            .commit_summarized_cut(
                skipped,
                4,
                "SUMMARY".to_string(),
                4096,
                included,
                0,
                |summary, drained| staged = Some((summary.to_string(), drained)),
            )
            .unwrap();
        assert_eq!(first_kept, expect_kept, "included={included}");
        assert_eq!(tokens_before, per_message * expect_kept as u64);
        assert_eq!(staged, Some(("SUMMARY".to_string(), expect_kept)));
        // Everything past the drained prefix is untouched.
        assert!(s.messages.len() >= 6 - expect_kept);
        assert_eq!(s.compactions.len(), 1);
    }
    let mut s = session_with_messages(4, 40);
    assert!(
        s.commit_summarized_cut(0, 3, "S".into(), 4096, 0, 0, |_, _| {})
            .is_err(),
        "a summarizer that covered nothing drains nothing"
    );
    assert!(s.compactions.is_empty());
    assert_eq!(s.messages.len(), 4);
}

/// Cut invariants for every compaction gate: whatever the keep-recent budget,
/// a cut never lands inside a parallel tool batch (between a call and any of
/// its results) and never splits a result from the assistant turn before it.
#[cfg(feature = "acp")]
#[test]
fn compaction_cut_never_splits_a_parallel_tool_batch() {
    let mut session = Session::new("openai", "gpt-4", 128_000, "");
    session.add_message(MessageRole::User, "first question");
    session.add_message(MessageRole::Assistant, "plan");
    for round in 0..3 {
        session.add_tool_call("read", &serde_json::json!({"path": format!("a{round}")}));
        session.add_tool_call("read", &serde_json::json!({"path": format!("b{round}")}));
        session.add_tool_result("read", &"x".repeat(300));
        session.add_tool_result("read", &"y".repeat(300));
    }
    session.add_message(MessageRole::Assistant, "answer");
    session.add_message(MessageRole::User, "second question");
    for keep in [0u64, 1, 50, 100, 200, 400, 800, 100_000] {
        let cut = Session::select_compaction_cut(&session.messages, keep);
        assert!(cut <= session.messages.len(), "keep={keep}");
        if cut > 0 && cut < session.messages.len() {
            assert!(
                Session::is_legal_cut(&session.messages, cut),
                "keep={keep} cut={cut} lands inside a tool batch"
            );
            assert_ne!(session.messages[cut].role, MessageRole::ToolResult);
        }
    }
}
