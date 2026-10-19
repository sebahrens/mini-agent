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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
    #[serde(default, skip_serializing_if = "false_value")]
    pub cache_split_required: bool,
    pub anthropic_native: bool,
}

fn false_value(value: &bool) -> bool {
    !*value
}

impl PriceSnapshot {
    pub(crate) fn capture(
        prices: Option<(f64, f64)>,
        cache: crate::pricing::CachePrices,
        anthropic_native: bool,
    ) -> Self {
        let (input, output) =
            prices.map_or((None, None), |(input, output)| (Some(input), Some(output)));
        let cache = if anthropic_native {
            input
                .map(|input| cache.effective_at(input))
                .unwrap_or(cache)
        } else {
            crate::pricing::CachePrices::default()
        };
        Self {
            input,
            output,
            cache_read: cache.read,
            cache_write: cache.write,
            cache_write_1h: cache.write_1h,
            cache_split_required: cache.split_required,
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
        .with_cache_policy(cfg, &session.model)
    }

    pub(crate) fn with_cache_policy(mut self, cfg: &crate::config::Config, model: &str) -> Self {
        if !self.anthropic_native {
            return self;
        }
        self.cache_split_required |= cfg.native_cache_split_required(model);
        self
    }
    fn valid(self) -> bool {
        [
            self.input,
            self.output,
            self.cache_read,
            self.cache_write,
            self.cache_write_1h,
        ]
        .into_iter()
        .flatten()
        .all(|rate| rate.is_finite() && rate >= 0.0)
    }
    pub(crate) fn cost(self, usage: UsageDelta) -> Option<f64> {
        if !self.valid() {
            return None;
        }
        if !self.anthropic_native {
            return self.legacy_cost(usage);
        }
        let Some(split) = usage.cache_creation else {
            if self.cache_split_required && usage.cache_creation_input_tokens != 0 {
                return None;
            }
            return self.legacy_cost(usage);
        };
        if split.total()? != usage.cache_creation_input_tokens {
            return None;
        }
        crate::pricing::CachePrices {
            read: self.cache_read,
            write: self.cache_write,
            write_1h: self.cache_write_1h,
            split_required: false,
        }
        .native_split_cost(usage, self.input?, self.output?)
    }
    fn legacy_cost(self, usage: UsageDelta) -> Option<f64> {
        if !self.valid() {
            return None;
        }
        let input = self.input?;
        let output = self.output?;
        let cost = crate::pricing::CachePrices {
            read: self.cache_read,
            write: self.cache_write,
            write_1h: None,
            split_required: false,
        }
        .estimate_cost_legacy(self.anthropic_native, usage, input, output);
        (cost.is_finite() && cost >= 0.0).then_some(cost)
    }
    pub(crate) fn is_known(self) -> bool {
        self.cost(UsageDelta::default()).is_some()
    }
    fn is_free(self) -> bool {
        self.legacy_is_free() && (!self.anthropic_native || self.cache_write_1h == Some(0.0))
    }
    fn legacy_is_free(self) -> bool {
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
    /// A main turn started by the `/cron` scheduler rather than typed input.
    /// Older readers of the ledger reject the row; this build reads both.
    Scheduled,
}

impl RequestKind {
    /// A main conversation turn, typed or scheduled.
    #[cfg_attr(not(feature = "subagents"), allow(dead_code))]
    pub(crate) fn is_main_turn(self) -> bool {
        matches!(self, Self::Turn | Self::Scheduled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(from = "VersionedRow")]
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

// Decode each version against its exact wire schema. Testing only the decoded
// values would let null/false new fields sneak into a version-one receipt.
// Derived map decoding also keeps duplicate-key rejection (unlike a Value
// intermediate), and both versions deny unknown nested and outer fields.
#[derive(Deserialize)]
#[serde(untagged)]
enum VersionedRow {
    Legacy(RowFields<LegacyPrices, LegacyUsage, 1>),
    Current(RowFields<PriceSnapshot, UsageDelta, 2>),
}

struct ExactVersion<const V: u8>;

impl<'de, const V: u8> Deserialize<'de> for ExactVersion<V> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if u8::deserialize(deserializer)? != V {
            return Err(serde::de::Error::custom("unsupported usage ledger version"));
        }
        Ok(Self)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RowFields<P, U, const V: u8> {
    version: ExactVersion<V>,
    id: Uuid,
    ts: chrono::DateTime<chrono::Utc>,
    session_id: String,
    provider: String,
    model: String,
    kind: RequestKind,
    usage: U,
    usage_reported: bool,
    prices: P,
    cost: Option<f64>,
}

impl<P: Into<PriceSnapshot>, U: Into<UsageDelta>, const V: u8> From<RowFields<P, U, V>> for Row {
    fn from(fields: RowFields<P, U, V>) -> Self {
        let _ = fields.version;
        Self {
            version: V,
            id: fields.id,
            ts: fields.ts,
            session_id: fields.session_id,
            provider: fields.provider,
            model: fields.model,
            kind: fields.kind,
            usage: fields.usage.into(),
            usage_reported: fields.usage_reported,
            prices: fields.prices.into(),
            cost: fields.cost,
        }
    }
}

impl From<VersionedRow> for Row {
    fn from(row: VersionedRow) -> Self {
        match row {
            VersionedRow::Legacy(fields) => fields.into(),
            VersionedRow::Current(fields) => fields.into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPrices {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    anthropic_native: bool,
}

impl From<LegacyPrices> for PriceSnapshot {
    fn from(prices: LegacyPrices) -> Self {
        Self {
            input: prices.input,
            output: prices.output,
            cache_read: prices.cache_read,
            cache_write: prices.cache_write,
            cache_write_1h: None,
            cache_split_required: false,
            anthropic_native: prices.anthropic_native,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
    tool_use_prompt_tokens: u64,
    reasoning_tokens: u64,
}

impl From<LegacyUsage> for UsageDelta {
    fn from(usage: LegacyUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_creation: None,
            tool_use_prompt_tokens: usage.tool_use_prompt_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }
}

impl Row {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(self.version, 1 | 2),
            "unsupported usage ledger version"
        );
        if self.version == 1 {
            anyhow::ensure!(
                self.usage.cache_creation.is_none()
                    && self.prices.cache_write_1h.is_none()
                    && !self.prices.cache_split_required,
                "version one usage receipt contains new cache metadata"
            );
        }
        anyhow::ensure!(
            [&self.session_id, &self.provider, &self.model]
                .into_iter()
                .all(|identity| !identity.is_empty() && identity.len() <= MAX_IDENTITY_BYTES),
            "usage receipt identity exceeds limits"
        );
        let usage = self.usage;
        if let Some(split) = usage.cache_creation {
            anyhow::ensure!(
                split.total() == Some(usage.cache_creation_input_tokens)
                    && split.ephemeral_5m_input_tokens <= MAX_RECEIPT_TOKENS
                    && split.ephemeral_1h_input_tokens <= MAX_RECEIPT_TOKENS,
                "usage receipt cache tiers mismatch"
            );
        }
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
        let free = if self.version == 1 {
            self.prices.legacy_is_free()
        } else {
            self.prices.is_free()
        };
        let expected = (self.usage_reported || free)
            .then(|| {
                if self.version == 1 {
                    self.prices.legacy_cost(usage)
                } else {
                    self.prices.cost(usage)
                }
            })
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
    #[cfg(feature = "subagents")]
    observation: Option<crate::session::background_observation::JobObservation>,
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
                version: 2,
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
            #[cfg(feature = "subagents")]
            observation: None,
        }
    }
    #[cfg(feature = "subagents")]
    pub(crate) fn with_background_observation(
        mut self,
        observation: Option<crate::session::background_observation::JobObservation>,
    ) -> Self {
        self.observation = observation;
        self
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
    token_cap_input: Option<u64>,
    token_cap_total: Option<u64>,
    observed_input_tokens: u64,
    observed_output_tokens: u64,
    historical_input_tokens: u64,
    historical_output_tokens: u64,
    legacy_estimate: f64,
    historical_observed_cost: f64,
    observed_cost: f64,
    unpriced_tokens: u64,
    tracking_error: bool,
    pending: BTreeMap<Uuid, Arc<Row>>,
    #[cfg(feature = "subagents")]
    pending_observations: BTreeMap<Uuid, crate::session::background_observation::JobObservation>,
    #[cfg(feature = "subagents")]
    observation: crate::session::background_observation::ObservationStamp,
    runtime_ledger: Option<Arc<Ledger>>,
    ledger_ready: bool,
    notice_shown: bool,
    observation_revision: u64,
    model_calls: u64,
    call_history_complete: bool,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedAccount {
    version: u8,
    #[serde(default)]
    model_calls: Option<u64>,
    #[serde(default)]
    call_history_complete: bool,
    session_id: String,
    legacy_estimate: f64,
    observed_cost: f64,
    unpriced_tokens: u64,
    tracking_error: bool,
}

impl State {
    fn token_usage(&self) -> (u64, u64) {
        let input = self
            .historical_input_tokens
            .saturating_add(self.observed_input_tokens);
        let output = self
            .historical_output_tokens
            .saturating_add(self.observed_output_tokens);
        (input, input.saturating_add(output))
    }
    fn saved(&self) -> SavedAccount {
        SavedAccount {
            version: 1,
            model_calls: Some(self.model_calls),
            call_history_complete: self.call_history_complete,
            session_id: self.session_id.clone(),
            legacy_estimate: self.legacy_estimate,
            observed_cost: self.historical_observed_cost + self.observed_cost,
            unpriced_tokens: self.unpriced_tokens,
            tracking_error: self.tracking_error || !self.pending.is_empty(),
        }
    }
}
impl Serialize for Account {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .saved()
            .serialize(serializer)
    }
}

// Captures every early-return/error branch under the same existing store lock.
// Only changes to the actual serde data advance the observation stamp.
struct AccountMutation<'a> {
    state: std::sync::MutexGuard<'a, State>,
    #[cfg(feature = "subagents")]
    before: SavedAccount,
    #[cfg(feature = "subagents")]
    observation: Option<crate::session::background_observation::JobObservation>,
}
impl std::ops::Deref for AccountMutation<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        &self.state
    }
}
impl std::ops::DerefMut for AccountMutation<'_> {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.state
    }
}
impl Drop for AccountMutation<'_> {
    fn drop(&mut self) {
        #[cfg(feature = "subagents")]
        if self.before != self.state.saved() {
            self.state.observation.changed(self.observation.as_ref());
        }
    }
}

#[cfg(feature = "subagents")]
#[derive(Debug, Clone)]
pub(crate) struct AccountObservation {
    original: Account,
    saved: SavedAccount,
    stamp: crate::session::background_observation::ObservationStamp,
}
#[cfg(feature = "subagents")]
impl AccountObservation {
    pub(crate) fn frozen(&self) -> Account {
        Account(Arc::new(Mutex::new(State {
            session_id: self.saved.session_id.clone(),
            legacy_estimate: self.saved.legacy_estimate,
            historical_observed_cost: self.saved.observed_cost,
            unpriced_tokens: self.saved.unpriced_tokens,
            tracking_error: self.saved.tracking_error,
            model_calls: self.saved.model_calls.unwrap_or(0),
            call_history_complete: self.saved.call_history_complete,
            ..Default::default()
        })))
    }
    pub(crate) fn validates_since(
        &self,
        previous: &Self,
        owner: &Arc<crate::session::background_observation::ObservationOwner>,
    ) -> bool {
        Arc::ptr_eq(&self.original.0, &previous.original.0)
            && self.stamp.validates_since(&previous.stamp, owner)
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
            model_calls: saved.model_calls.unwrap_or(0),
            call_history_complete: saved.model_calls.is_some()
                && saved.call_history_complete
                && !saved.tracking_error,
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
    #[cfg(feature = "subagents")]
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    fn edit(&self) -> AccountMutation<'_> {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        AccountMutation {
            #[cfg(feature = "subagents")]
            before: state.saved(),
            state,
            #[cfg(feature = "subagents")]
            observation: None,
        }
    }
    #[cfg(feature = "subagents")]
    fn edit_observation(
        &self,
        observation: Option<&crate::session::background_observation::JobObservation>,
    ) -> AccountMutation<'_> {
        let mut mutation = self.edit();
        mutation.observation = observation.cloned();
        mutation
    }

    #[cfg(feature = "subagents")]
    pub(crate) fn with_observation_snapshot<T>(
        &self,
        capture: impl FnOnce(AccountObservation) -> T,
    ) -> T {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        capture(AccountObservation {
            original: self.clone(),
            saved: state.saved(),
            stamp: state.observation.clone(),
        })
    }
    #[cfg(feature = "subagents")]
    pub(crate) fn bind_observation_owner(
        &self,
        owner: &Arc<crate::session::background_observation::ObservationOwner>,
    ) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .observation
            .bind(owner)
    }
    pub(crate) fn take_budget_notice(&self) -> bool {
        let mut state = self.edit();
        let show = !state.notice_shown;
        state.notice_shown = true;
        show
    }

    pub(crate) fn runtime_ledger(&self, candidate: Arc<Ledger>) -> Arc<Ledger> {
        let mut state = self.edit();
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
            call_history_complete: legacy_estimate == 0.0,
            ..Default::default()
        }))))
    }
    pub(crate) fn set_cap(&self, cap: Option<f64>) -> anyhow::Result<()> {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).cap = validate_cap(cap)?;
        Ok(())
    }
    /// Session token ceilings. Unlike the USD cap they need no prices and no
    /// ledger: every observed receipt counts in memory at once.
    pub(crate) fn set_token_caps(&self, input: Option<u64>, total: Option<u64>) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.token_cap_input = input;
        state.token_cap_total = total;
    }
    /// True when any spend or token ceiling is configured.
    #[cfg_attr(not(feature = "acp"), allow(dead_code))]
    pub(crate) fn has_ceiling(&self) -> bool {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.cap.is_some() || state.token_cap_input.is_some() || state.token_cap_total.is_some()
    }
    /// Observed (input, input+output) session tokens, ledger history included.
    #[cfg(test)]
    pub(crate) fn token_usage(&self) -> (u64, u64) {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.token_usage()
    }
    pub(crate) fn tokens_exhausted(&self) -> bool {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let (input, total) = state.token_usage();
        state.token_cap_input.is_some_and(|cap| input >= cap)
            || state.token_cap_total.is_some_and(|cap| total >= cap)
    }
    pub(crate) fn inherit_uncertainty(&self, prior: AccountSnapshot) {
        let mut state = self.edit();
        state.tracking_error |= prior.tracking_error;
        state.call_history_complete &= !prior.tracking_error;
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
        let mut state = self.edit();
        state.cap = cap;
        state.model_calls = state
            .model_calls
            .max(prior.records.saturating_sub(prior.unreported_records) as u64);
        state.call_history_complete &= prior.unreported_records == 0;
        state.ledger_ready = true;
        // Conservative for resumed sessions: the ledger totals count cached
        // input on top of reported input, which only over-counts for
        // providers whose input already includes it.
        let prior_input = prior
            .input_tokens
            .saturating_add(prior.cached_input_tokens)
            .saturating_add(prior.cache_creation_input_tokens);
        state.historical_input_tokens = state
            .historical_input_tokens
            .max(prior_input.saturating_sub(state.observed_input_tokens));
        state.historical_output_tokens = state.historical_output_tokens.max(
            prior
                .output_tokens
                .saturating_sub(state.observed_output_tokens),
        );
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
    /// Display-only count of deduplicated canonical reported requests. Older
    /// checkpoints and missing reports remain a lower bound, never exact zero.
    pub(crate) fn model_call_count(&self) -> Option<(u64, bool)> {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        (!state.session_id.is_empty()).then_some((state.model_calls, state.call_history_complete))
    }

    /// The statusline's spend inputs under one lock: the display call count
    /// (as [`Self::model_call_count`]) and whether tracking failed.
    pub(crate) fn status_snapshot(&self) -> (Option<(u64, bool)>, bool) {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        (
            (!state.session_id.is_empty())
                .then_some((state.model_calls, state.call_history_complete)),
            state.tracking_error,
        )
    }

    pub(crate) fn inherit_call_count(&self, count: u64, complete: bool) {
        let mut state = self.edit();
        state.model_calls = state.model_calls.max(count);
        state.call_history_complete &= complete;
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
        anyhow::ensure!(!self.tokens_exhausted(), "session token budget exhausted");
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
        #[cfg(feature = "subagents")]
        let mut state = self.edit_observation(receipt.observation.as_ref());
        #[cfg(not(feature = "subagents"))]
        let mut state = self.edit();
        anyhow::ensure!(
            row.session_id == state.session_id,
            "usage receipt belongs to another session"
        );
        if receipt.charged.swap(true, Ordering::AcqRel) {
            return Ok(false);
        }
        state.observation_revision = state.observation_revision.saturating_add(1);
        if row.usage_reported {
            state.model_calls = state.model_calls.saturating_add(1);
        } else {
            state.call_history_complete = false;
        }
        let usage = row.usage;
        let input = crate::session::Session::real_input_tokens(
            row.prices.anthropic_native,
            usage.input_tokens,
            usage.total_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.cache_creation_input_tokens,
        );
        state.observed_input_tokens = state.observed_input_tokens.saturating_add(input);
        state.observed_output_tokens = state
            .observed_output_tokens
            .saturating_add(usage.output_tokens);
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
        #[cfg(feature = "subagents")]
        if let Some(observation) = &receipt.observation {
            state
                .pending_observations
                .insert(row.id, observation.clone());
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
        #[cfg(feature = "subagents")]
        let observation = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending_observations
            .get(&id)
            .cloned();
        #[cfg(feature = "subagents")]
        let mut state = self.edit_observation(observation.as_ref());
        #[cfg(not(feature = "subagents"))]
        let mut state = self.edit();
        state.pending.remove(&id);
        #[cfg(feature = "subagents")]
        state.pending_observations.remove(&id);
    }
    pub(crate) fn tracking_failed(&self) {
        let mut state = self.edit();
        state.tracking_error = true;
        state.call_history_complete = false;
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
    fn split_usage(five: u64, hour: u64) -> UsageDelta {
        UsageDelta {
            cache_creation_input_tokens: five + hour,
            cache_creation: Some(rig::completion::CacheCreationUsage {
                ephemeral_5m_input_tokens: five,
                ephemeral_1h_input_tokens: hour,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn cache71_captured_mixed_tiers_round_once_and_never_scale_five_minute_override() {
        let mut prices = PriceSnapshot::capture(
            Some((2.0, 10.0)),
            crate::pricing::CachePrices {
                read: Some(0.2),
                write: Some(9.0),
                write_1h: None,
                split_required: false,
            },
            true,
        );
        assert_eq!(prices.cache_write, Some(9.0));
        assert_eq!(prices.cache_write_1h, Some(4.0));
        prices.cache_write = Some(2.5);
        let mut usage = split_usage(1, 1);
        usage.cached_input_tokens = 3;
        assert_eq!(prices.cost(usage), Some(0.000008));
        let mut paid_free_base = PriceSnapshot::capture(
            Some((0.0, 0.0)),
            crate::pricing::CachePrices {
                read: Some(0.3),
                write: Some(4.0),
                write_1h: None,
                split_required: false,
            },
            true,
        );
        paid_free_base.cache_write_1h = Some(8.0);
        usage = split_usage(1_000_000, 1_000_000);
        usage.cached_input_tokens = 1_000_000;
        assert_eq!(paid_free_base.cost(usage), Some(12.3));
        assert!(!paid_free_base.is_free());
    }

    #[test]
    fn cache71_missing_or_inconsistent_tiers_never_mint_known_one_hour_cost() {
        let mut prices = PriceSnapshot::capture(Some((2.0, 10.0)), Default::default(), true);
        prices.cache_split_required = true;
        let account = Account::new("cache71", Some(1.0), 0.0).unwrap();
        let missing = UsageDelta {
            input_tokens: 10,
            cache_creation_input_tokens: 100,
            ..Default::default()
        };
        assert_eq!(prices.cost(missing), None);
        let receipt = Receipt::new(
            "cache71",
            "anthropic",
            "native",
            RequestKind::Turn,
            missing,
            prices,
        );
        assert!(account.observe(&receipt).unwrap());
        assert!(account.snapshot().tracking_error);
        assert!(account.admit(prices).is_err());
        assert_eq!(receipt.row.cost, None);
        receipt.row.validate().unwrap();
        let mut invalid = split_usage(3, 2);
        invalid.cache_creation_input_tokens = 4;
        assert_eq!(prices.cost(invalid), None);
        let invalid = Receipt::new(
            "cache71",
            "anthropic",
            "native",
            RequestKind::Turn,
            invalid,
            prices,
        );
        assert!(invalid.row.validate().is_err());
        prices.cache_write_1h = None;
        assert_eq!(prices.cost(split_usage(0, 1)), None);
    }

    #[test]
    fn cache71_native_cache_policy_requires_tiers_without_guessing_request_ttl() {
        let usage = UsageDelta {
            cache_creation_input_tokens: 10,
            ..Default::default()
        };
        for (extra, required) in [
            (serde_json::json!({}), false),
            (serde_json::json!({"cache_control":null}), false),
            (
                serde_json::json!({"cache_control":{"type":"ephemeral","ttl":"1h"}}),
                true,
            ),
            (
                serde_json::json!({"cache_control":{"type":"ephemeral"}}),
                true,
            ),
            (
                serde_json::json!({"tools":[{"cache_control":{"type":"ephemeral","ttl":"1h"}}]}),
                true,
            ),
            (serde_json::json!({"system":"raw override"}), true),
            (serde_json::json!({"messages":[]}), true),
        ] {
            let cfg = crate::config::Config {
                extra_body: Some(extra),
                ..Default::default()
            };
            let native = PriceSnapshot::capture(Some((2.0, 10.0)), Default::default(), true)
                .with_cache_policy(&cfg, "fixture");
            assert_eq!(native.cache_split_required, required);
            assert_eq!(native.cost(usage).is_none(), required);
            let routed = PriceSnapshot::capture(Some((2.0, 10.0)), Default::default(), false)
                .with_cache_policy(&cfg, "fixture");
            assert!(!routed.cache_split_required);
        }
        let cfg = crate::config::Config {
            cache_ttl: Some(crate::config::PromptCacheTtl::OneHour),
            ..Default::default()
        };
        assert!(
            PriceSnapshot::capture(Some((2.0, 10.0)), Default::default(), true)
                .with_cache_policy(&cfg, "fixture")
                .cache_split_required
        );
    }

    #[test]
    fn cache71_receipt_versions_preserve_old_rounding_and_reject_metadata_smuggling() {
        let usage = UsageDelta {
            input_tokens: 10,
            cache_creation_input_tokens: 3,
            ..Default::default()
        };
        let mut prices = PriceSnapshot::capture(Some((2.0, 10.0)), Default::default(), true);
        prices.cache_write_1h = None;
        let mut legacy = Receipt::new(
            "cache71",
            "anthropic",
            "native",
            RequestKind::Turn,
            usage,
            prices,
        )
        .row
        .as_ref()
        .clone();
        legacy.version = 1;
        legacy.cost = prices.legacy_cost(usage);
        legacy.validate().unwrap();
        let bytes = serde_json::to_vec(&legacy).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("cache_write_1h"));
        assert!(!String::from_utf8_lossy(&bytes).contains("cache_split_required"));
        let decoded: Row = serde_json::from_slice(&bytes).unwrap();
        decoded.validate().unwrap();
        assert_eq!(serde_json::to_vec(&decoded).unwrap(), bytes);
        let wire = String::from_utf8(bytes.clone()).unwrap();
        for (key, value) in [
            ("cache_write_1h", "null"),
            ("cache_split_required", "false"),
            ("cache_creation", "null"),
        ] {
            let original = if key == "cache_creation" {
                "\"usage\":{"
            } else {
                "\"prices\":{"
            };
            let injected = wire.replacen(original, &format!("{original}\"{key}\":{value},"), 1);
            assert!(serde_json::from_str::<Row>(&injected).is_err(), "{key}");
        }
        // Untagged version selection must not collapse duplicate map keys.
        for duplicate in [
            wire.replacen("\"version\":1", "\"version\":1,\"version\":1", 1),
            wire.replacen(
                "\"input_tokens\":10",
                "\"input_tokens\":10,\"input_tokens\":10",
                1,
            ),
            wire.replacen("\"input\":2.0", "\"input\":2.0,\"input\":2.0", 1),
        ] {
            assert!(serde_json::from_str::<Row>(&duplicate).is_err());
        }
        let mut smuggled = legacy.clone();
        smuggled.prices.cache_write_1h = Some(4.0);
        assert!(smuggled.validate().is_err());
        smuggled = legacy.clone();
        smuggled.usage.cache_creation = split_usage(3, 0).cache_creation;
        assert!(smuggled.validate().is_err());
        legacy.version = 3;
        assert!(legacy.validate().is_err());
        assert!(serde_json::from_slice::<Row>(&serde_json::to_vec(&legacy).unwrap()).is_err());
        let modern = Receipt::new(
            "cache71",
            "anthropic",
            "native",
            RequestKind::Turn,
            split_usage(3, 0),
            PriceSnapshot::capture(Some((2.0, 10.0)), Default::default(), true),
        );
        assert_eq!(modern.row.version, 2);
        modern.row.validate().unwrap();
        let modern_wire = serde_json::to_string(modern.row.as_ref()).unwrap();
        let modern_decoded: Row = serde_json::from_str(&modern_wire).unwrap();
        modern_decoded.validate().unwrap();
        assert_eq!(modern_decoded, *modern.row);
        let duplicate = modern_wire.replacen(
            "\"input_tokens\":0",
            "\"input_tokens\":0,\"input_tokens\":0",
            1,
        );
        assert!(serde_json::from_str::<Row>(&duplicate).is_err());
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
    fn session_token_caps_stop_unpriced_sessions_including_subagent_receipts() {
        let receipt = |kind, input, output| {
            Receipt::new(
                "s",
                "p",
                "m",
                kind,
                UsageDelta {
                    input_tokens: input,
                    output_tokens: output,
                    total_tokens: input + output,
                    ..Default::default()
                },
                prices(None),
            )
        };
        let account = Account::new("s", None, 0.0).unwrap();
        assert!(!account.has_ceiling());
        account.set_token_caps(Some(100), Some(130));
        assert!(account.has_ceiling());
        assert!(account.admit(prices(None)).is_ok(), "no price is required");
        account
            .observe(&receipt(RequestKind::Turn, 60, 10))
            .unwrap();
        assert!(account.admit(prices(None)).is_ok());
        account
            .observe(&receipt(RequestKind::Subagent, 30, 10))
            .unwrap();
        assert_eq!(account.token_usage(), (90, 110));
        assert!(account.admit(prices(None)).is_ok());
        // Total cap trips before the input cap does.
        account
            .observe(&receipt(RequestKind::Scheduled, 5, 20))
            .unwrap();
        assert_eq!(account.token_usage(), (95, 135));
        assert!(account.admit(prices(None)).is_err());
        assert!(account.admit_effect().is_err());
        // The input cap alone stops too, and raising a cap re-admits.
        account.set_token_caps(Some(95), None);
        assert!(account.admit_effect().is_err());
        account.set_token_caps(Some(96), None);
        assert!(account.admit_effect().is_ok());
        account.set_token_caps(Some(0), None);
        assert!(account.admit_effect().is_err());
        account.set_token_caps(None, None);
        assert!(account.admit(prices(None)).is_ok());
    }

    #[test]
    fn session_token_caps_count_resumed_ledger_history() {
        let account = Account::new("s", None, 0.0).unwrap();
        account.set_token_caps(Some(1000), None);
        account
            .reconcile(
                None,
                &ledger::Totals {
                    input_tokens: 900,
                    output_tokens: 50,
                    cached_input_tokens: 100,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(account.token_usage(), (1000, 1050));
        assert!(account.admit_effect().is_err());
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
                write_1h: None,
                split_required: false,
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

#[cfg(test)]
mod model_call_count_tests {
    use super::*;
    fn receipt(kind: RequestKind, rate: f64, reported: bool) -> Receipt {
        Receipt::new(
            "s",
            "p",
            "m",
            kind,
            if reported {
                UsageDelta {
                    input_tokens: 10,
                    ..Default::default()
                }
            } else {
                UsageDelta::default()
            },
            PriceSnapshot::capture(
                Some((rate, rate)),
                crate::pricing::CachePrices::default(),
                false,
            ),
        )
    }
    #[test]
    fn model_call_count_deduplicates_paid_free_auxiliary_and_preserves_money() {
        let account = Account::new("s", None, 0.0).unwrap();
        for (kind, rate) in [
            (RequestKind::Turn, 1.0),
            (RequestKind::Title, 0.0),
            (RequestKind::Compaction, 2.0),
        ] {
            let receipt = receipt(kind, rate, true);
            assert!(account.observe(&receipt).unwrap());
            assert!(!account.observe(&receipt.clone()).unwrap());
            account.acknowledge(receipt.id());
        }
        assert_eq!(account.model_call_count(), Some((3, true)));
        assert!((account.snapshot().recorded_spend() - 0.000030).abs() < 1e-12);
        let saved = serde_json::to_string(&account).unwrap();
        let restored: Account = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored.model_call_count(), Some((3, true)));
        restored
            .reconcile(
                None,
                &ledger::Totals {
                    records: 5,
                    unreported_records: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(restored.model_call_count(), Some((4, false)));
        assert!(restored.admit_effect().is_ok());
    }
    #[test]
    fn scheduled_request_kind_round_trips_through_a_ledger_row() {
        let receipt = receipt(RequestKind::Scheduled, 1.0, true);
        let wire = serde_json::to_value(&*receipt.row).unwrap();
        assert_eq!(wire["kind"], "scheduled");
        assert_eq!(wire["version"], 2);
        let row: Row = serde_json::from_value(wire).unwrap();
        assert_eq!(row.kind, RequestKind::Scheduled);
        assert_eq!(row, *receipt.row);
        row.validate().unwrap();
        assert!(RequestKind::Scheduled.is_main_turn() && RequestKind::Turn.is_main_turn());
        assert!(!RequestKind::Title.is_main_turn());
        // A scheduled turn counts as a model call like any other request.
        let account = Account::new("s", None, 0.0).unwrap();
        assert!(account.observe(&receipt).unwrap());
        assert_eq!(account.model_call_count(), Some((1, true)));
    }
    #[test]
    fn model_call_count_old_checkpoints_missing_reports_and_saturation_are_honest() {
        let old: Account = serde_json::from_value(serde_json::json!({"version":1,"session_id":"s","legacy_estimate":0.0,"observed_cost":0.0,"unpriced_tokens":0,"tracking_error":false})).unwrap();
        assert_eq!(old.model_call_count(), Some((0, false)));
        old.observe(&receipt(RequestKind::Turn, 0.0, false))
            .unwrap();
        assert_eq!(old.model_call_count(), Some((0, false)));
        old.inherit_call_count(u64::MAX, false);
        old.observe(&receipt(RequestKind::Turn, 1.0, true)).unwrap();
        assert_eq!(old.model_call_count(), Some((u64::MAX, false)));
        assert_eq!(Account::default().model_call_count(), None);
    }
}

#[cfg(test)]
mod model_call_uncertainty_tests {
    use super::*;

    #[test]
    fn tracking_failure_without_a_receipt_never_claims_an_exact_zero() {
        let account = Account::new("s", None, 0.0).unwrap();
        assert_eq!(account.model_call_count(), Some((0, true)));
        account.tracking_failed();
        assert_eq!(account.model_call_count(), Some((0, false)));
        assert_eq!(account.snapshot().recorded_spend(), 0.0);
        assert!(account.snapshot().tracking_error);
        let restored: Account =
            serde_json::from_str(&serde_json::to_string(&account).unwrap()).unwrap();
        assert_eq!(restored.model_call_count(), Some((0, false)));
        let fork = Account::new("fork", None, 0.0).unwrap();
        fork.inherit_uncertainty(account.snapshot());
        fork.inherit_call_count(0, true);
        assert_eq!(fork.model_call_count(), Some((0, false)));
    }

    #[test]
    fn live_unpriced_report_counts_but_saved_uncertainty_remains_a_lower_bound() {
        let account = Account::new("s", None, 0.0).unwrap();
        let receipt = Receipt::new(
            "s",
            "p",
            "m",
            RequestKind::Turn,
            UsageDelta {
                input_tokens: 1,
                ..Default::default()
            },
            PriceSnapshot::capture(None, crate::pricing::CachePrices::default(), false),
        );
        assert!(account.observe(&receipt).unwrap());
        assert_eq!(account.model_call_count(), Some((1, true)));
        assert!(account.snapshot().tracking_error);
        let restored: Account =
            serde_json::from_str(&serde_json::to_string(&account).unwrap()).unwrap();
        assert_eq!(restored.model_call_count(), Some((1, false)));
        assert_eq!(
            restored.snapshot().recorded_spend(),
            account.snapshot().recorded_spend()
        );
        let ambiguous: Account = serde_json::from_value(serde_json::json!({"version":1,
            "session_id":"s", "model_calls":0, "call_history_complete":true,
            "legacy_estimate":0.0,"observed_cost":0.0,"unpriced_tokens":0,"tracking_error":true}))
        .unwrap();
        assert_eq!(ambiguous.model_call_count(), Some((0, false)));
    }
}
