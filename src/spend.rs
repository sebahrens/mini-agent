//! Parent-observed spend for one logical session. Imported totals are estimates,
//! never newly minted receipts, and live clones share monotonic accounting.

use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crate::event::UsageDelta;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod ledger;
mod report;
#[cfg(test)]
mod request_tests;
pub(crate) use report::cost_lines;
pub(crate) mod run;
pub(crate) use ledger::{Ledger, LedgerSummary};
const MAX_PENDING_RECEIPTS: usize = 1024;
const MAX_IDENTITY_BYTES: usize = 512;
const MAX_RECEIPT_TOKENS: u64 = 1_000_000_000_000_000;
const MAX_RECEIPT_USD: f64 = 1_000_000_000_000_000_000.0;

pub(crate) fn validate_cap(cap: Option<f64>) -> anyhow::Result<Option<f64>> {
    anyhow::ensure!(
        cap.is_none_or(|cap| cap.is_finite() && cap >= 0.0),
        "USD budget must be finite and nonnegative"
    );
    Ok(cap)
}

pub(crate) fn deserialize_cap<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<f64>, D::Error> {
    validate_cap(Option::<f64>::deserialize(deserializer)?)
        .map_err(|_| serde::de::Error::custom("USD budget must be finite and nonnegative"))
}

/// Effective cache rates are captured at observation time, including the
/// existing fallback policy. A future catalogue/fallback change cannot reprice
/// an old row. Version 1 keeps the existing equivalent-input-token rounding.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PriceSnapshot {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub anthropic_native: bool,
}

impl PriceSnapshot {
    pub(crate) fn capture(
        prices: Option<(f64, f64)>,
        cache: crate::pricing::CachePrices,
        anthropic_native: bool,
    ) -> Self {
        let (input, output) =
            prices.map_or((None, None), |(input, output)| (Some(input), Some(output)));
        let (cache_read, cache_write) = if anthropic_native {
            if let Some(input) = input.filter(|input| input.is_finite() && *input > 0.0) {
                let ratios = cache.multipliers(input);
                (Some(input * ratios.read), Some(input * ratios.write))
            } else if input == Some(0.0) {
                (
                    Some(cache.read.unwrap_or(0.0)),
                    Some(cache.write.unwrap_or(0.0)),
                )
            } else {
                (cache.read, cache.write)
            }
        } else {
            (None, None)
        };
        Self {
            input,
            output,
            cache_read,
            cache_write,
            anthropic_native,
        }
    }
    pub(crate) fn from_session(
        session: &crate::session::Session,
        cfg: &crate::config::Config,
    ) -> Self {
        Self::capture(
            session.known_input_output_prices(),
            session.cache_prices(),
            cfg.is_anthropic_native(&session.provider),
        )
    }
    fn valid(self) -> bool {
        [self.input, self.output, self.cache_read, self.cache_write]
            .into_iter()
            .flatten()
            .all(|rate| rate.is_finite() && rate >= 0.0)
    }
    pub(crate) fn cost(self, usage: UsageDelta) -> Option<f64> {
        if !self.valid() {
            return None;
        }
        let input = self.input?;
        let output = self.output?;
        let cost = crate::pricing::CachePrices {
            read: self.cache_read,
            write: self.cache_write,
        }
        .estimate_cost(self.anthropic_native, usage, input, output);
        (cost.is_finite() && cost >= 0.0).then_some(cost)
    }
    pub(crate) fn is_known(self) -> bool {
        self.cost(UsageDelta::default()).is_some()
    }
    fn is_free(self) -> bool {
        self.is_known()
            && [self.input, self.output]
                .into_iter()
                .all(|rate| rate == Some(0.0))
            && (!self.anthropic_native
                || [self.cache_read, self.cache_write]
                    .into_iter()
                    .all(|rate| rate == Some(0.0)))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequestKind {
    Turn,
    Title,
    Repair,
    Btw,
    Subagent,
    Advisor,
    Judge,
    Compaction,
    GitProposal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Row {
    version: u8,
    id: Uuid,
    ts: chrono::DateTime<chrono::Utc>,
    session_id: String,
    provider: String,
    model: String,
    kind: RequestKind,
    usage: UsageDelta,
    usage_reported: bool,
    prices: PriceSnapshot,
    cost: Option<f64>,
}

impl Row {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1, "unsupported usage ledger version");
        anyhow::ensure!(
            [&self.session_id, &self.provider, &self.model]
                .into_iter()
                .all(|identity| !identity.is_empty() && identity.len() <= MAX_IDENTITY_BYTES),
            "usage receipt identity exceeds limits"
        );
        let usage = self.usage;
        anyhow::ensure!(
            [
                usage.input_tokens,
                usage.output_tokens,
                usage.total_tokens,
                usage.cached_input_tokens,
                usage.cache_creation_input_tokens,
                usage.tool_use_prompt_tokens,
                usage.reasoning_tokens
            ]
            .into_iter()
            .all(|count| count <= MAX_RECEIPT_TOKENS),
            "usage receipt token count exceeds limits"
        );
        anyhow::ensure!(
            self.usage_reported == self.usage.has_values(),
            "usage receipt metrics mismatch"
        );
        anyhow::ensure!(self.prices.valid(), "invalid captured usage price");
        let expected = (self.usage_reported || self.prices.is_free())
            .then(|| self.prices.cost(usage))
            .flatten();
        anyhow::ensure!(
            self.cost == expected && self.cost.is_none_or(|cost| cost <= MAX_RECEIPT_USD),
            "usage receipt price/cost mismatch"
        );
        Ok(())
    }
}

/// The charge-once marker is parent-owned and never serialized/deserialized.
/// Retrying a ledger append or cloning this handle cannot charge usage twice;
/// queue exhaustion still preserves the observation without an unbounded ID map.
#[derive(Debug, Clone)]
pub(crate) struct Receipt {
    row: Arc<Row>,
    charged: Arc<AtomicBool>,
}

impl Receipt {
    pub(crate) fn new(
        session_id: &str,
        provider: &str,
        model: &str,
        kind: RequestKind,
        usage: UsageDelta,
        prices: PriceSnapshot,
    ) -> Self {
        let usage_reported = usage.has_values();
        let cost = (usage_reported || prices.is_free())
            .then(|| prices.cost(usage))
            .flatten();
        Self {
            row: Arc::new(Row {
                version: 1,
                id: Uuid::new_v4(),
                ts: chrono::Utc::now(),
                session_id: session_id.into(),
                provider: provider.into(),
                model: model.into(),
                kind,
                usage,
                usage_reported,
                prices,
                cost,
            }),
            charged: Arc::new(AtomicBool::new(false)),
        }
    }
    #[cfg(test)]
    pub(crate) fn id(&self) -> Uuid {
        self.row.id
    }
}

#[derive(Debug, Default)]
struct State {
    session_id: String,
    cap: Option<f64>,
    legacy_estimate: f64,
    historical_observed_cost: f64,
    observed_cost: f64,
    unpriced_tokens: u64,
    tracking_error: bool,
    pending: BTreeMap<Uuid, Arc<Row>>,
    runtime_ledger: Option<Arc<Ledger>>,
    ledger_ready: bool,
    notice_shown: bool,
    observation_revision: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Account(Arc<Mutex<State>>);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct AccountSnapshot {
    pub cap: Option<f64>,
    pub legacy_estimate: f64,
    pub historical_observed_cost: f64,
    pub observed_cost: f64,
    pub unpriced_tokens: u64,
    pub tracking_error: bool,
}

impl AccountSnapshot {
    pub(crate) fn recorded_spend(self) -> f64 {
        self.legacy_estimate + self.historical_observed_cost + self.observed_cost
    }
    pub(crate) fn exhausted(self) -> bool {
        self.cap.is_some_and(|cap| self.recorded_spend() >= cap)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedAccount {
    version: u8,
    session_id: String,
    legacy_estimate: f64,
    observed_cost: f64,
    unpriced_tokens: u64,
    tracking_error: bool,
}

impl Serialize for Account {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        SavedAccount {
            version: 1,
            session_id: state.session_id.clone(),
            legacy_estimate: state.legacy_estimate,
            observed_cost: state.historical_observed_cost + state.observed_cost,
            unpriced_tokens: state.unpriced_tokens,
            tracking_error: state.tracking_error || !state.pending.is_empty(),
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Account {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let saved = SavedAccount::deserialize(deserializer)?;
        if saved.version != 1
            || saved.session_id.is_empty()
            || saved.session_id.len() > MAX_IDENTITY_BYTES
            || [saved.legacy_estimate, saved.observed_cost]
                .into_iter()
                .any(|cost| !cost.is_finite() || cost < 0.0)
            || !(saved.legacy_estimate + saved.observed_cost).is_finite()
        {
            return Err(serde::de::Error::custom(
                "invalid saved observed-spend checkpoint",
            ));
        }
        Ok(Self(Arc::new(Mutex::new(State {
            session_id: saved.session_id,
            legacy_estimate: saved.legacy_estimate,
            historical_observed_cost: saved.observed_cost,
            unpriced_tokens: saved.unpriced_tokens,
            tracking_error: saved.tracking_error,
            ..Default::default()
        }))))
    }
}

impl Account {
    pub(crate) fn take_budget_notice(&self) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let show = !state.notice_shown;
        state.notice_shown = true;
        show
    }

    pub(crate) fn runtime_ledger(&self, candidate: Arc<Ledger>) -> Arc<Ledger> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.runtime_ledger.get_or_insert(candidate).clone()
    }

    pub(crate) fn new(
        session_id: &str,
        cap: Option<f64>,
        legacy_estimate: f64,
    ) -> anyhow::Result<Self> {
        let cap = validate_cap(cap)?;
        anyhow::ensure!(
            !session_id.is_empty() && session_id.len() <= MAX_IDENTITY_BYTES,
            "invalid session cost identity"
        );
        anyhow::ensure!(
            legacy_estimate.is_finite() && legacy_estimate >= 0.0,
            "saved session cost estimate is invalid"
        );
        Ok(Self(Arc::new(Mutex::new(State {
            session_id: session_id.into(),
            cap,
            legacy_estimate,
            ..Default::default()
        }))))
    }
    pub(crate) fn set_cap(&self, cap: Option<f64>) -> anyhow::Result<()> {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).cap = validate_cap(cap)?;
        Ok(())
    }
    pub(crate) fn inherit_uncertainty(&self, prior: AccountSnapshot) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.tracking_error |= prior.tracking_error;
        state.unpriced_tokens = state.unpriced_tokens.max(prior.unpriced_tokens);
    }
    pub(crate) fn is_uninitialised(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .session_id
            .is_empty()
    }
    pub(crate) fn matches_session(&self, session_id: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .session_id
            == session_id
    }
    /// Keep a loaded checkpoint or live clone monotonic. The ledger may contain
    /// a charge appended after the last successful session snapshot save.
    pub(crate) fn reconcile(&self, cap: Option<f64>, prior: &ledger::Totals) -> anyhow::Result<()> {
        let cap = validate_cap(cap)?;
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.cap = cap;
        state.ledger_ready = true;
        state.historical_observed_cost = state
            .historical_observed_cost
            .max((prior.known_cost - state.observed_cost).max(0.0));
        state.unpriced_tokens = state.unpriced_tokens.max(prior.unpriced_tokens);
        state.tracking_error |= prior.missing_usage || prior.unpriced_records > 0;
        Ok(())
    }
    pub(crate) fn snapshot(&self) -> AccountSnapshot {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        AccountSnapshot {
            cap: state.cap,
            legacy_estimate: state.legacy_estimate,
            historical_observed_cost: state.historical_observed_cost,
            observed_cost: state.observed_cost,
            unpriced_tokens: state.unpriced_tokens,
            tracking_error: state.tracking_error,
        }
    }
    /// One atomic read for reports. Revision is runtime-only and tracks even
    /// known-free receipts; it never creates or replays persisted observations.
    pub(crate) fn report_snapshot(&self) -> (AccountSnapshot, bool, u64) {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        (
            AccountSnapshot {
                cap: state.cap,
                legacy_estimate: state.legacy_estimate,
                historical_observed_cost: state.historical_observed_cost,
                observed_cost: state.observed_cost,
                unpriced_tokens: state.unpriced_tokens,
                tracking_error: state.tracking_error,
            },
            !state.pending.is_empty(),
            state.observation_revision,
        )
    }
    pub(crate) fn has_pending(&self) -> bool {
        !self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .is_empty()
    }
    pub(crate) fn ledger_ready(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .ledger_ready
    }
    pub(crate) fn admit_effect(&self) -> anyhow::Result<()> {
        let state = self.snapshot();
        if state.cap.is_none() {
            return Ok(());
        }
        anyhow::ensure!(!self.has_pending(), "usage ledger persistence is pending");
        anyhow::ensure!(!state.exhausted(), "session USD budget exhausted");
        anyhow::ensure!(
            !state.tracking_error && state.unpriced_tokens == 0,
            "cost cap cannot continue while usage tracking is incomplete"
        );
        Ok(())
    }
    pub(crate) fn admit(&self, prices: PriceSnapshot) -> anyhow::Result<()> {
        self.admit_effect()?;
        anyhow::ensure!(
            self.snapshot().cap.is_none() || prices.is_known(),
            "cost cap requires known model pricing"
        );
        Ok(())
    }
    pub(crate) fn observe(&self, receipt: &Receipt) -> anyhow::Result<bool> {
        let row = &receipt.row;
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            row.session_id == state.session_id,
            "usage receipt belongs to another session"
        );
        if receipt.charged.swap(true, Ordering::AcqRel) {
            return Ok(false);
        }
        state.observation_revision = state.observation_revision.saturating_add(1);
        if let Some(cost) = row.cost {
            let sum = state.observed_cost + cost;
            if sum.is_finite() {
                state.observed_cost = sum;
            } else {
                state.tracking_error = true;
            }
        } else {
            let usage = row.usage;
            let input = crate::session::Session::real_input_tokens(
                row.prices.anthropic_native,
                usage.input_tokens,
                usage.total_tokens,
                usage.output_tokens,
                usage.cached_input_tokens,
                usage.cache_creation_input_tokens,
            );
            state.unpriced_tokens = state
                .unpriced_tokens
                .saturating_add(input)
                .saturating_add(usage.output_tokens);
            // A reported auxiliary metric can coexist with zero inferred
            // input/output. Unknown prices still cannot prove a free request.
            state.tracking_error = true;
        }
        if let Err(error) = row.validate() {
            state.tracking_error = true;
            return Err(error);
        }
        if state.pending.len() == MAX_PENDING_RECEIPTS {
            state.tracking_error = true;
            anyhow::bail!("usage receipt queue capacity reached");
        }
        state.pending.insert(row.id, row.clone());
        Ok(true)
    }
    fn pending(&self) -> Vec<Arc<Row>> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .values()
            .cloned()
            .collect()
    }
    fn acknowledge(&self, id: Uuid) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .remove(&id);
    }
    pub(crate) fn tracking_failed(&self) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .tracking_error = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn prices(input: Option<f64>) -> PriceSnapshot {
        PriceSnapshot::capture(
            input.map(|input| (input, input)),
            crate::pricing::CachePrices::default(),
            false,
        )
    }
    #[test]
    fn spend_cap_validation_and_unknown_zero_prices_are_distinct() {
        for invalid in [f64::NAN, f64::INFINITY, -1.0] {
            assert!(validate_cap(Some(invalid)).is_err());
        }
        let account = Account::new("s", Some(1.0), 0.0).unwrap();
        assert!(account.admit(prices(None)).is_err());
        assert!(account.admit(prices(Some(0.0))).is_ok());
        assert!(
            Account::new("s", Some(0.0), 0.0)
                .unwrap()
                .admit(prices(Some(0.0)))
                .is_err()
        );
    }
    #[test]
    fn spend_receipts_charge_once_across_clones_acknowledgement_and_rewind() {
        let account = Account::new("s", Some(2.0), 1.0).unwrap();
        let live_rewind_clone = account.clone();
        let receipt = Receipt::new(
            "s",
            "p",
            "m",
            RequestKind::Turn,
            UsageDelta {
                input_tokens: 1_000_000,
                ..Default::default()
            },
            prices(Some(1.0)),
        );
        assert!(account.observe(&receipt).unwrap());
        account.acknowledge(receipt.id());
        assert!(!live_rewind_clone.observe(&receipt.clone()).unwrap());
        assert_eq!(live_rewind_clone.snapshot().recorded_spend(), 2.0);
        assert!(live_rewind_clone.admit_effect().is_err());
        assert!(
            account.pending().is_empty(),
            "legacy estimates never become rows"
        );
    }
    #[test]
    fn spend_cache_prices_capture_effective_fallback_and_paid_free_base_tiers() {
        let prices = PriceSnapshot::capture(
            Some((0.0, 0.0)),
            crate::pricing::CachePrices {
                read: Some(0.3),
                write: Some(4.0),
            },
            true,
        );
        assert_eq!(
            prices.cost(UsageDelta {
                cached_input_tokens: 1_000_000,
                cache_creation_input_tokens: 1_000_000,
                ..Default::default()
            }),
            Some(4.3)
        );
        let fallback = PriceSnapshot::capture(
            Some((2.0, 10.0)),
            crate::pricing::CachePrices::default(),
            true,
        );
        assert_eq!(fallback.cache_read, Some(0.2));
        assert_eq!(fallback.cache_write, Some(2.5));
    }
    #[test]
    fn spend_unknown_prices_with_only_auxiliary_metrics_cannot_seed_a_free_cap() {
        for usage in [
            UsageDelta {
                reasoning_tokens: 1,
                ..Default::default()
            },
            UsageDelta {
                tool_use_prompt_tokens: 1,
                ..Default::default()
            },
        ] {
            let account = Account::new("s", None, 0.0).unwrap();
            account
                .observe(&Receipt::new(
                    "s",
                    "p",
                    "m",
                    RequestKind::Turn,
                    usage,
                    prices(None),
                ))
                .unwrap();
            assert_eq!(account.snapshot().unpriced_tokens, 0);
            assert!(account.snapshot().tracking_error);
            account.set_cap(Some(1.0)).unwrap();
            assert!(account.admit(prices(Some(1.0))).is_err());
        }
        let restored = Account::new("s", Some(1.0), 0.0).unwrap();
        restored
            .reconcile(
                Some(1.0),
                &ledger::Totals {
                    unpriced_records: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(restored.admit(prices(Some(1.0))).is_err());
    }
    #[test]
    fn spend_queue_exhaustion_retains_incurred_cost_and_charge_once_identity() {
        let account = Account::new("s", Some(1.0), 0.0).unwrap();
        for _ in 0..MAX_PENDING_RECEIPTS {
            account
                .observe(&Receipt::new(
                    "s",
                    "p",
                    "m",
                    RequestKind::Turn,
                    UsageDelta {
                        input_tokens: 1,
                        ..Default::default()
                    },
                    prices(Some(1.0)),
                ))
                .unwrap();
        }
        let overflow = Receipt::new(
            "s",
            "p",
            "m",
            RequestKind::Turn,
            UsageDelta {
                input_tokens: 1_000_000,
                ..Default::default()
            },
            prices(Some(1.0)),
        );
        assert!(account.observe(&overflow).is_err());
        let cost = account.snapshot().observed_cost;
        assert!(cost > 1.0);
        assert!(!account.observe(&overflow).unwrap());
        assert_eq!(account.snapshot().observed_cost, cost);
        assert!(account.admit_effect().is_err());
    }
    #[test]
    fn spend_paid_missing_metrics_and_unknown_usage_fail_closed_without_fabrication() {
        let account = Account::new("s", Some(1.0), 0.0).unwrap();
        account
            .observe(&Receipt::new(
                "s",
                "p",
                "m",
                RequestKind::Turn,
                UsageDelta::default(),
                prices(Some(1.0)),
            ))
            .unwrap();
        assert!(account.snapshot().tracking_error);
        assert_eq!(account.snapshot().observed_cost, 0.0);
        assert!(account.admit_effect().is_err());
        let free = Account::new("s", Some(1.0), 0.0).unwrap();
        free.observe(&Receipt::new(
            "s",
            "p",
            "m",
            RequestKind::Turn,
            UsageDelta::default(),
            prices(Some(0.0)),
        ))
        .unwrap();
        assert!(!free.snapshot().tracking_error);
        assert!(
            free.admit_effect().is_err(),
            "even known free receipts await persistence"
        );
        let root = crate::tests::OwnedTestDirectory::new("cost-free-persistence");
        Ledger::new(root.path()).flush(&free).unwrap();
        assert!(free.admit_effect().is_ok());
    }
}
