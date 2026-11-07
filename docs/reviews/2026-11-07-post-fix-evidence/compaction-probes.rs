// Review-only probes. Append this module to src/ui/slash/mod.rs, then run:
// cargo test --bin mini-agent -- rereview_token_boundary --test-threads=1
// The control must PASS; the regression probe is expected to FAIL on 282e1e1b.
// Production regression coverage should additionally drive App::mid_turn_compact.
#[cfg(test)]
mod rereview_token_boundary_probes {
    use super::{TuiCompactionGate, plan_tui_compaction};
    use crate::session::{MessageRole, Session};

    fn history() -> Session {
        let mut session = Session::new("openai", "model", 100_000, "");
        session.add_message(MessageRole::User, &"x".repeat(240_000));
        session.add_message(MessageRole::Assistant, "old completed work");
        session.add_message(MessageRole::User, &"continue ".repeat(1_000));
        session
    }

    #[test]
    fn rereview_token_boundary_control_has_a_legal_compaction_cut() {
        let session = history();
        assert!(session.effective_context_tokens_after_tool_result_pruning(usize::MAX) < 90_000);
        assert!(matches!(
            plan_tui_compaction(&session, false, false, 90_000, 1_000, usize::MAX),
            TuiCompactionGate::Cut(_)
        ));
        // An ordinary between-turn check has no real-pressure boundary and
        // correctly remains within its 90k local token budget.
        assert_eq!(
            plan_tui_compaction(&session, true, false, 90_000, 1_000, usize::MAX),
            TuiCompactionGate::WithinBudget
        );
    }

    #[test]
    fn rereview_token_boundary_real_pressure_must_not_be_vetoed_by_local_gate() {
        let session = history();
        let real_input_tokens = 81_000u64;
        let threshold = 0.80f64;
        let pressure = Some(real_input_tokens as f64 / session.context_window as f64);
        assert!(pressure.unwrap() > threshold, "App requests a compaction boundary");
        // These are compact_and_respawn's actual argument choices:
        // auto=true and force=pressure.is_none() (ui/mod.rs:1919-1922).
        // handle_compress_unfloored then passes auto && !force to the planner.
        let auto = true;
        let force = pressure.is_none();
        let got = plan_tui_compaction(
            &session, auto && !force, false, 90_000, 1_000, usize::MAX,
        );
        assert!(matches!(got, TuiCompactionGate::Cut(_)),
            "A real provider boundary has a legal old prefix, but the TUI performed no compaction: {got:?}");
    }
}
