//! Bounded read-only projection. Imported session estimates are displayed apart
//! from trusted newly observed model receipts; they never become ledger rows.
use super::{Ledger, LedgerSummary};
use crate::session::Session;
const MAX_DISPLAY_MODELS: usize = 32;

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
        "{label}: ${:.6}; input {} / cached {} / cache write {} / output {}{completeness}",
        totals.known_cost,
        totals.input_tokens,
        totals.cached_input_tokens,
        totals.cache_creation_input_tokens,
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
