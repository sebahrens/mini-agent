//! Per-child delegation telemetry (mini-agent-964ao).
//!
//! The repository had no measurement of how often a child's report passes the
//! response contract, how often the repair turn is needed or the host replaces
//! a report, or what a delegation costs in turns and tokens. Each finished
//! child records one [`ChildTelemetry`] row under its session id; a headless
//! result aggregates them as `subagent_delegation`, next to `subagent_usage`.
//! The delegated-versus-direct evaluation lane in `tests/harness_eval/` reads
//! this output.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use serde::Serialize;

/// Rows kept per session; later children are only counted.
const MAX_ROWS: usize = 256;

/// How a child's final text stood against the response contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContractOutcome {
    /// Passed validation on the first try.
    Pass,
    /// Failed, then passed after the single repair turn.
    Repaired,
    /// Failed, and the host normalized the child's own Markdown into the contract.
    Normalized,
    /// Failed twice (or repair was skipped); delivered as quoted, flagged data.
    Unstructured,
    /// A plain-text call or `contract: none` persona: no validation.
    Plain,
    /// The child failed or was cut off before a report existed.
    #[default]
    None,
}

/// A short fingerprint of a child's system prompt and tool names: equal
/// fingerprints mean a byte-identical cacheable prefix.
pub(crate) fn prefix_fingerprint(preamble: &str, tool_names: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(preamble.as_bytes());
    for name in tool_names {
        digest.update([0]);
        digest.update(name.as_bytes());
    }
    crate::hex::encode_lower(digest.finalize())[..16].to_owned()
}

/// What one finished child did and cost.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ChildTelemetry {
    pub agent: String,
    /// Id shared by the children of one `task` call.
    pub call: String,
    /// [`prefix_fingerprint`] of the child's system prompt and tool names
    /// (empty when the child never built an agent).
    pub prefix: String,
    pub contract: ContractOutcome,
    /// The contract repair turn ran for this child.
    pub repair_used: bool,
    /// The host replaced or rewrote the child's text (normalized or unstructured).
    pub host_replaced: bool,
    /// The tool-free write-up turn ran (turn budget or deadline).
    pub writeup_used: bool,
    /// `completed`, `turn_budget`, `deadline` or `failed`.
    pub stop: &'static str,
    pub turns: u32,
    pub tool_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub total_tokens: u64,
    pub cost_units: u64,
}

static ROWS: LazyLock<Mutex<HashMap<String, (Vec<ChildTelemetry>, usize)>>> =
    LazyLock::new(Default::default);

/// Record a finished child for `session_id`.
pub(crate) fn record(session_id: &str, row: ChildTelemetry) {
    let mut map = ROWS.lock().unwrap_or_else(|e| e.into_inner());
    let (rows, total) = map.entry(session_id.to_owned()).or_default();
    *total += 1;
    if rows.len() < MAX_ROWS {
        rows.push(row);
    }
}

/// The recorded rows of `session_id` and how many children finished in all.
pub(crate) fn report(session_id: &str) -> (Vec<ChildTelemetry>, usize) {
    ROWS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .cloned()
        .unwrap_or_default()
}

/// The headless result's `subagent_delegation`: per-session counters and one
/// row per finished child. `None` when no child ran.
pub(crate) fn delegation_json(
    rows: &[ChildTelemetry],
    finished: usize,
) -> Option<serde_json::Value> {
    if finished == 0 {
        return None;
    }
    let count =
        |keep: &dyn Fn(&ChildTelemetry) -> bool| rows.iter().filter(|row| keep(row)).count();
    let sum = |field: &dyn Fn(&ChildTelemetry) -> u64| rows.iter().map(field).sum::<u64>();
    Some(serde_json::json!({
        "children": finished,
        "contract_pass": count(&|row| row.contract == ContractOutcome::Pass),
        "contract_repaired": count(&|row| row.contract == ContractOutcome::Repaired),
        "contract_normalized": count(&|row| row.contract == ContractOutcome::Normalized),
        "contract_unstructured": count(&|row| row.contract == ContractOutcome::Unstructured),
        "repair_used": count(&|row| row.repair_used),
        "host_replaced": count(&|row| row.host_replaced),
        "writeup_used": count(&|row| row.writeup_used),
        "stopped_turn_budget": count(&|row| row.stop == "turn_budget"),
        "stopped_deadline": count(&|row| row.stop == "deadline"),
        "failed": count(&|row| row.stop == "failed"),
        "turns": sum(&|row| u64::from(row.turns)),
        "tool_calls": sum(&|row| u64::from(row.tool_calls)),
        "total_tokens": sum(&|row| row.total_tokens),
        "cached_input_tokens": sum(&|row| row.cached_input_tokens),
        "cost_units": sum(&|row| row.cost_units),
        "calls_with_differing_prefixes": differing_prefix_calls(rows),
        "rows_truncated": finished > rows.len(),
        "rows": rows,
    }))
}

/// How many `task` calls had children whose cacheable prefix differed. Zero
/// is expected for siblings of one persona; isolated worktree children differ
/// by working directory by design.
fn differing_prefix_calls(rows: &[ChildTelemetry]) -> usize {
    let mut calls: HashMap<&str, std::collections::BTreeSet<&str>> = HashMap::new();
    for row in rows.iter().filter(|row| !row.prefix.is_empty()) {
        calls.entry(&row.call).or_default().insert(&row.prefix);
    }
    calls.values().filter(|prefixes| prefixes.len() > 1).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(contract: ContractOutcome, repair: bool, stop: &'static str) -> ChildTelemetry {
        ChildTelemetry {
            agent: "explore".into(),
            contract,
            repair_used: repair,
            host_replaced: matches!(
                contract,
                ContractOutcome::Normalized | ContractOutcome::Unstructured
            ),
            stop,
            turns: 3,
            tool_calls: 5,
            total_tokens: 100,
            cached_input_tokens: 40,
            cost_units: 7,
            ..Default::default()
        }
    }

    #[test]
    fn counters_aggregate_contract_repair_and_stop_reasons() {
        let session = "telemetry-test-session";
        record(session, row(ContractOutcome::Pass, false, "completed"));
        record(session, row(ContractOutcome::Repaired, true, "completed"));
        record(
            session,
            row(ContractOutcome::Unstructured, true, "deadline"),
        );
        record(session, row(ContractOutcome::None, false, "failed"));
        let (rows, finished) = report(session);
        assert_eq!((rows.len(), finished), (4, 4));
        let json = delegation_json(&rows, finished).unwrap();
        assert_eq!(json["children"], 4);
        assert_eq!(json["contract_pass"], 1);
        assert_eq!(json["contract_repaired"], 1);
        assert_eq!(json["contract_unstructured"], 1);
        assert_eq!(json["repair_used"], 2);
        assert_eq!(json["host_replaced"], 1);
        assert_eq!(json["stopped_deadline"], 1);
        assert_eq!(json["failed"], 1);
        assert_eq!(json["turns"], 12);
        assert_eq!(json["total_tokens"], 400);
        assert_eq!(json["cost_units"], 28);
        assert_eq!(json["rows"][1]["contract"], "repaired");
        assert!(delegation_json(&[], 0).is_none());
        assert!(report("another-session").0.is_empty());
    }

    #[test]
    fn siblings_with_one_prefix_are_not_flagged_and_a_differing_one_is() {
        let child = |call: &str, prefix: &str| ChildTelemetry {
            call: call.into(),
            prefix: prefix.into(),
            ..Default::default()
        };
        let same = vec![child("a", "p1"), child("a", "p1"), child("b", "p2")];
        assert_eq!(differing_prefix_calls(&same), 0);
        let differing = vec![
            child("a", "p1"),
            child("a", "p9"),
            child("b", "p2"),
            child("b", ""),
        ];
        assert_eq!(differing_prefix_calls(&differing), 1);
        let one = prefix_fingerprint("prompt", &["read".into(), "grep".into()]);
        assert_eq!(
            one,
            prefix_fingerprint("prompt", &["read".into(), "grep".into()])
        );
        assert_ne!(
            one,
            prefix_fingerprint("prompt", &["grep".into(), "read".into()])
        );
        assert_eq!(one.len(), 16);
    }

    #[test]
    fn rows_are_bounded_but_the_total_keeps_counting() {
        let session = "telemetry-bound-session";
        for _ in 0..MAX_ROWS + 5 {
            record(session, row(ContractOutcome::Pass, false, "completed"));
        }
        let (rows, finished) = report(session);
        assert_eq!(rows.len(), MAX_ROWS);
        assert_eq!(finished, MAX_ROWS + 5);
        assert_eq!(
            delegation_json(&rows, finished).unwrap()["rows_truncated"],
            true
        );
    }
}
