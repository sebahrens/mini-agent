//! Bounded read-only projection. Imported session estimates are displayed apart
//! from trusted newly observed model receipts; they never become ledger rows.
use super::{Ledger, LedgerSummary};
use crate::session::Session;
const MAX_DISPLAY_MODELS: usize = 32;

fn write_tiers(total: u64, split: Option<rig::completion::CacheCreationUsage>) -> String {
    match split.filter(|split| split.total() == Some(total)) {
        Some(split) => format!(
            " (5m {} / 1h {})",
            split.ephemeral_5m_input_tokens, split.ephemeral_1h_input_tokens
        ),
        None if total > 0 => " (TTL breakdown unreported)".into(),
        None => String::new(),
    }
}

fn identity(value: &str) -> String {
    crate::ui::events::sanitize_output(value)
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect()
}
fn total(label: &str, totals: &super::ledger::Totals) -> String {
    let completeness =
        if totals.unpriced_records > 0 || totals.unreported_records > 0 || totals.missing_usage {
            " (incomplete: unpriced or unreported usage)"
        } else {
            ""
        };
    format!(
        "{label}: ${:.6}; input {} / cached {} / cache write {}{} / output {}{completeness}",
        totals.known_cost,
        totals.input_tokens,
        totals.cached_input_tokens,
        totals.cache_creation_input_tokens,
        write_tiers(totals.cache_creation_input_tokens, totals.cache_creation),
        totals.output_tokens
    )
}
pub(crate) async fn cost_lines(session: &Session, ledger: Ledger) -> anyhow::Result<Vec<String>> {
    let session_id = session.id.to_string();
    let uninitialised = session.spend_account.is_uninitialised();
    let legacy_cost = session.total_cost;
    let (_, pending_before, revision_before) = session.spend_account.report_snapshot();
    let scope = crate::agent::runner::AgentWorkScope::new();
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let summary: LedgerSummary = scope
        .run(async move {
            crate::agent::runner::spawn_blocking_scoped(move || {
                ledger.summary_for_session(&session_id)
            })
            .await
        })
        .await
        .map_err(anyhow::Error::from)??;
    cleanup.settle().await;
    let (snapshot, pending_after, revision_after) = session.spend_account.report_snapshot();
    let mut lines = vec![format!(
        "Session recorded spend: ${:.6}",
        if uninitialised {
            legacy_cost
        } else {
            snapshot.recorded_spend()
        }
    )];
    let estimate = if uninitialised {
        legacy_cost
    } else {
        snapshot.legacy_estimate
    };
    if estimate > 0.0 {
        lines.push(format!(
            "Legacy/imported estimate: ${estimate:.6} (excluded from daily ledger)"
        ));
    }
    if let Some(cap) = snapshot.cap {
        lines.push(format!(
            "Session USD ceiling: ${cap:.6} (reported usage estimate; not a provider billing limit)"
        ));
    }
    if snapshot.tracking_error || snapshot.unpriced_tokens > 0 {
        lines.push("Cost tracking incomplete; capped requests/effects are blocked".into());
    }
    if pending_after {
        lines.push("Observed receipts awaiting persistence; ledger totals below exclude unpersisted observations".into());
    } else if pending_before {
        lines.push("Receipts were awaiting persistence when this query began; ledger totals below are a persistence snapshot and may lag recorded spend".into());
    } else if revision_before != revision_after {
        lines.push("New usage was observed during this query; ledger totals below are an earlier persistence snapshot".into());
    }
    lines.push(total("Session ledger", &summary.session));
    if let Some(request) = &summary.last_session_request {
        let usage = request.usage;
        let uncached = if request.anthropic_native {
            usage.input_tokens
        } else {
            usage.input_tokens.saturating_sub(usage.cached_input_tokens)
        };
        lines.push(format!("Last recorded request {:?} {}/{}: uncached input {} / cached input {} / cache write {}{} / output {}",
            request.kind, identity(&request.provider), identity(&request.model), uncached,
            usage.cached_input_tokens, usage.cache_creation_input_tokens,
            write_tiers(usage.cache_creation_input_tokens, usage.cache_creation), usage.output_tokens));
    }
    for ((provider, model), totals) in summary.session_by_model.iter().take(MAX_DISPLAY_MODELS) {
        lines.push(total(
            &format!("  {}/{}", identity(provider), identity(model)),
            totals,
        ));
    }
    if summary.session_model_details_capped || summary.session_by_model.len() > MAX_DISPLAY_MODELS {
        lines.push("Additional model detail omitted; aggregate totals above remain complete where prices/usage are known".into());
    }
    lines.push(total("Today (UTC)", &summary.today));
    lines.push(total("All time", &summary.all_time));
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::UsageDelta;
    use crate::spend::{Account, PriceSnapshot, Receipt, RequestKind};

    #[tokio::test]
    async fn cache71_cost_report_projects_latest_request_and_keeps_unknown_tiers_unknown() {
        let root = crate::tests::OwnedTestDirectory::new("cache71-report");
        let state = root.path().join("state");
        crate::fs::ensure_private_directory(&state).unwrap();
        let ledger = Ledger::new(&state);
        let mut session = Session::new("anthropic", "cache71", 100_000, "");
        session.spend_account = Account::new(&session.id, None, 0.0).unwrap();
        let prices = PriceSnapshot::capture(Some((2.0, 10.0)), Default::default(), true);
        let mut usage = UsageDelta {
            input_tokens: 8,
            output_tokens: 5,
            cached_input_tokens: 4,
            cache_creation_input_tokens: 10,
            cache_creation: Some(rig::completion::CacheCreationUsage {
                ephemeral_5m_input_tokens: 3,
                ephemeral_1h_input_tokens: 7,
            }),
            ..Default::default()
        };
        let receipt = Receipt::new(
            &session.id,
            "anthropic",
            "cache71",
            RequestKind::Turn,
            usage,
            prices,
        );
        session.spend_account.observe(&receipt).unwrap();
        ledger.flush(&session.spend_account).unwrap();
        let stored = std::fs::read(state.join("usage.jsonl")).unwrap();
        let lines = cost_lines(&session, Ledger::new(&state)).await.unwrap();
        assert!(lines.iter().any(|line| line.starts_with("Session ledger:")
            && line.contains("cache write 10 (5m 3 / 1h 7)")));
        assert!(lines.iter().any(|line| {
            line.starts_with("Last recorded request Turn anthropic/cache71:")
                && line.contains(
                    "uncached input 8 / cached input 4 / cache write 10 (5m 3 / 1h 7) / output 5",
                )
        }));
        assert_eq!(
            std::fs::read(state.join("usage.jsonl")).unwrap(),
            stored,
            "report changed durable receipts"
        );

        usage.cache_creation = None;
        usage.cache_creation_input_tokens = 1;
        let missing = Receipt::new(
            &session.id,
            "anthropic",
            "cache71",
            RequestKind::Title,
            usage,
            prices,
        );
        session.spend_account.observe(&missing).unwrap();
        ledger.flush(&session.spend_account).unwrap();
        let lines = cost_lines(&session, Ledger::new(&state)).await.unwrap();
        assert!(lines.iter().any(|line| line.starts_with("Session ledger:")
            && line.contains("cache write 11 (TTL breakdown unreported)")));
        assert!(lines.iter().any(|line| {
            line.starts_with("Last recorded request Title anthropic/cache71:")
                && line.contains("cache write 1 (TTL breakdown unreported)")
        }));
        let routed = UsageDelta {
            input_tokens: 20,
            cached_input_tokens: 4,
            output_tokens: 2,
            ..Default::default()
        };
        let receipt = Receipt::new(
            &session.id,
            "openrouter",
            "routed",
            RequestKind::Turn,
            routed,
            PriceSnapshot::capture(Some((2.0, 10.0)), Default::default(), false),
        );
        session.spend_account.observe(&receipt).unwrap();
        ledger.flush(&session.spend_account).unwrap();
        let lines = cost_lines(&session, Ledger::new(&state)).await.unwrap();
        assert!(
            lines.iter().any(|line| line
                .starts_with("Last recorded request Turn openrouter/routed:")
                && line.contains("uncached input 16 / cached input 4")),
            "routed input already includes its cached subset"
        );
    }
}
