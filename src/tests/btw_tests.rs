//! Tests for the parallel, tool-less `/btw` side question.
//!
//! The behavioural guarantees we care about are about the *snapshot*: a side
//! question forks the committed history (plus an in-flight turn trace when the
//! main agent is busy) and never mutates the session. The network round-trip
//! itself is not exercised here.

use compact_str::CompactString;

use crate::agent::runner::build_btw_snapshot;
use crate::session::{MessageRole, Session};

fn sample_session() -> Session {
    let mut s = Session::new("anthropic", "claude-test", 200_000, "");
    s.add_message(MessageRole::User, "hello");
    s.add_message(MessageRole::Assistant, "hi there");
    s
}

#[test]
fn snapshot_without_trace_matches_history() {
    let session = sample_session();
    let snapshot = build_btw_snapshot(&session, &[], false);
    // Two committed messages, no trace appended.
    assert_eq!(snapshot.len(), 2);
}

#[test]
fn trace_is_appended_only_while_main_is_running() {
    let session = sample_session();
    let trace = [
        CompactString::from("→ bash: npm test"),
        CompactString::from("← running…"),
    ];

    // Mid-task: the in-flight trace is folded in as one extra message.
    let running = build_btw_snapshot(&session, &trace, true);
    assert_eq!(running.len(), 3);

    // Not running: a stale trace must not leak into the snapshot.
    let idle = build_btw_snapshot(&session, &trace, false);
    assert_eq!(idle.len(), 2);
}

#[test]
fn empty_trace_while_running_adds_nothing() {
    let session = sample_session();
    let snapshot = build_btw_snapshot(&session, &[], true);
    assert_eq!(snapshot.len(), 2);
}

#[test]
fn snapshot_does_not_mutate_session() {
    let mut session = sample_session();
    let before_len = session.messages.len();
    let before_in = session.total_input_tokens;
    let before_out = session.total_output_tokens;
    let before_cost = session.total_cost;

    let trace = [CompactString::from("→ read: src/main.rs")];
    let _ = build_btw_snapshot(&session, &trace, true);

    // The forked snapshot is by value; the session is untouched. This is the
    // core safety property that replaces the old "append then roll back" path.
    assert_eq!(session.messages.len(), before_len);
    assert_eq!(session.total_input_tokens, before_in);
    assert_eq!(session.total_output_tokens, before_out);
    assert_eq!(session.total_cost, before_cost);

    // touch `session` mutably so the binding is legitimately `mut`.
    session.add_message(MessageRole::User, "again");
    assert_eq!(session.messages.len(), before_len + 1);
}

mod btw_lifecycle_tests {
    use std::time::Duration;

    #[tokio::test]
    async fn retire_scoped_task_waits_for_owned_blocking_child() {
        let (scope, started_rx, release) =
            crate::agent::runner::AgentWorkScope::new_with_blocking_test_gate();
        let task_scope = scope.clone();
        let task = tokio::spawn(async move {
            task_scope
                .run(async {
                    let _child = crate::agent::runner::spawn_blocking_scoped(|| ());
                    std::future::pending::<()>().await;
                })
                .await;
        });
        tokio::task::spawn_blocking(move || {
            started_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("scoped blocking child should start");
        })
        .await
        .unwrap();

        let mut retirement = tokio::spawn(crate::ui::retire_scoped_task(
            task,
            scope.clone(),
            "test owner",
            Duration::from_secs(1),
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut retirement)
                .await
                .is_err(),
            "retirement must wait for scoped blocking children"
        );
        assert!(scope.is_cancelled());
        assert_eq!(scope.active_children(), 1);
        release.release();
        tokio::time::timeout(Duration::from_secs(1), retirement)
            .await
            .expect("retirement should finish after child release")
            .expect("retirement task should not panic")
            .expect("retirement should succeed");
        assert_eq!(scope.active_children(), 0);
    }
}
