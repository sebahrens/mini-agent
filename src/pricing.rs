/// Fallback rates for models without published or configured cache pricing.
/// Writes assume the default five-minute TTL used by the production client.
const ANTHROPIC_CACHE_WRITE_MULT: f64 = 1.25;
const ANTHROPIC_CACHE_WRITE_1H_MULT: f64 = 2.0;
const ANTHROPIC_CACHE_READ_MULT: f64 = 0.10;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct CachePrices {
    pub read: Option<f64>,
    pub write: Option<f64>,
    pub write_1h: Option<f64>,
    pub split_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheMultipliers {
    pub read: f64,
    pub write: f64,
    pub write_1h: f64,
}

impl Default for CacheMultipliers {
    fn default() -> Self {
        Self {
            read: ANTHROPIC_CACHE_READ_MULT,
            write: ANTHROPIC_CACHE_WRITE_MULT,
            write_1h: ANTHROPIC_CACHE_WRITE_1H_MULT,
        }
    }
}

fn valid_price(price: f64) -> bool {
    price.is_finite() && price >= 0.0
}

impl CachePrices {
    #[cfg(feature = "goal")]
    pub(crate) fn has_valid_tier(self) -> bool {
        [self.read, self.write, self.write_1h]
            .into_iter()
            .flatten()
            .any(valid_price)
    }

    /// Charge native cache tiers independently when the uncached input is free.
    /// A positive base keeps the historical equivalent-token rounding policy.
    pub(crate) fn estimate_cost(
        self,
        anthropic_native: bool,
        usage: crate::event::UsageDelta,
        input_price: f64,
        output_price: f64,
    ) -> f64 {
        self.checked_cost(anthropic_native, usage, input_price, output_price)
            .unwrap_or(0.0)
    }

    pub(crate) fn checked_cost(
        self,
        anthropic_native: bool,
        usage: crate::event::UsageDelta,
        input_price: f64,
        output_price: f64,
    ) -> Option<f64> {
        if !valid_price(input_price) || !valid_price(output_price) {
            return None;
        }
        if anthropic_native
            && self.split_required
            && usage.cache_creation_input_tokens > 0
            && usage.cache_creation.is_none()
        {
            return None;
        }
        if anthropic_native && usage.cache_creation.is_some() {
            return self.effective_at(input_price).native_split_cost(
                usage,
                input_price,
                output_price,
            );
        }
        let cost = self.estimate_cost_legacy(anthropic_native, usage, input_price, output_price);
        valid_price(cost).then_some(cost)
    }

    /// Version-one rows retain the original total-write estimate exactly.
    pub(crate) fn estimate_cost_legacy(
        self,
        anthropic_native: bool,
        usage: crate::event::UsageDelta,
        input_price: f64,
        output_price: f64,
    ) -> f64 {
        if anthropic_native && input_price == 0.0 {
            let tier = |tokens: u64, price: Option<f64>| {
                tokens as f64 * price.filter(|price| valid_price(*price)).unwrap_or(0.0)
                    / 1_000_000.0
            };
            return estimate_cost(usage.input_tokens, usage.output_tokens, 0.0, output_price)
                + tier(usage.cached_input_tokens, self.read)
                + tier(usage.cache_creation_input_tokens, self.write);
        }
        estimate_cost(
            billable_input_tokens_with_multipliers(
                anthropic_native,
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.cache_creation_input_tokens,
                self.multipliers(input_price),
            ),
            usage.output_tokens,
            input_price,
            output_price,
        )
    }

    /// Capture independent native tier rates at the selected base price. The
    /// one-hour fallback never derives from an explicit five-minute override.
    pub(crate) fn effective_at(self, input: f64) -> Self {
        let (read, write) = if input.is_finite() && input > 0.0 {
            let ratios = self.multipliers(input);
            (Some(input * ratios.read), Some(input * ratios.write))
        } else if input == 0.0 {
            (
                Some(self.read.unwrap_or(0.0)),
                Some(self.write.unwrap_or(0.0)),
            )
        } else {
            (self.read, self.write)
        };
        Self {
            read,
            write,
            write_1h: self.write_1h.or_else(|| {
                let price = input * ANTHROPIC_CACHE_WRITE_1H_MULT;
                valid_price(price).then_some(price)
            }),
            split_required: self.split_required,
        }
    }

    /// Price reported native write tiers over a single equivalent-input pool.
    /// Callers supply captured effective rates; missing rates stay unknown.
    pub(crate) fn native_split_cost(
        self,
        usage: crate::event::UsageDelta,
        input: f64,
        output: f64,
    ) -> Option<f64> {
        let split = usage.cache_creation?;
        if split.total()? != usage.cache_creation_input_tokens
            || !valid_price(input)
            || !valid_price(output)
            || [self.read, self.write, self.write_1h]
                .into_iter()
                .flatten()
                .any(|rate| !valid_price(rate))
        {
            return None;
        }
        let read = self.read?;
        let write_5m = self.write?;
        let write_1h = if split.ephemeral_1h_input_tokens == 0 {
            0.0
        } else {
            self.write_1h?
        };
        let cost = if input == 0.0 {
            (usage.output_tokens as f64 * output
                + usage.cached_input_tokens as f64 * read
                + split.ephemeral_5m_input_tokens as f64 * write_5m
                + split.ephemeral_1h_input_tokens as f64 * write_1h)
                / 1_000_000.0
        } else {
            let equivalent = usage.input_tokens as f64
                + usage.cached_input_tokens as f64 * (read / input)
                + split.ephemeral_5m_input_tokens as f64 * (write_5m / input)
                + split.ephemeral_1h_input_tokens as f64 * (write_1h / input);
            if !equivalent.is_finite() || equivalent < 0.0 || equivalent >= u64::MAX as f64 {
                return None;
            }
            estimate_cost(
                equivalent.round() as u64,
                usage.output_tokens,
                input,
                output,
            )
        };
        (cost.is_finite() && cost >= 0.0).then_some(cost)
    }

    pub(crate) fn multipliers(self, input_price: f64) -> CacheMultipliers {
        let ratio = |price: Option<f64>, fallback| {
            if price == Some(0.0) {
                return 0.0;
            }
            if !input_price.is_finite() || input_price <= 0.0 {
                return fallback;
            }
            price
                .filter(|price| valid_price(*price))
                .map(|price| price / input_price)
                .filter(|ratio| ratio.is_finite())
                .unwrap_or(fallback)
        };
        CacheMultipliers {
            read: ratio(self.read, ANTHROPIC_CACHE_READ_MULT),
            write: ratio(self.write, ANTHROPIC_CACHE_WRITE_MULT),
            write_1h: ratio(self.write_1h, ANTHROPIC_CACHE_WRITE_1H_MULT),
        }
    }

    /// Inherit published model ratios at the selected base price. Changing an
    /// alias's base rate therefore preserves its discount instead of silently
    /// adopting an unrelated absolute vendor price.
    pub(crate) fn from_catalog(provider: &str, model: &str, input_price: f64) -> Self {
        if !input_price.is_finite() || input_price <= 0.0 {
            return Self::default();
        }
        let provider = if provider == "google" {
            "gemini"
        } else {
            provider
        };
        crate::models_catalog::catalog_entries(provider)
            .and_then(|models| models.iter().find(|entry| entry.id == model))
            .map(|entry| {
                let scale = |cache: Option<f64>| {
                    let base = entry
                        .input_price
                        .filter(|price| valid_price(*price) && *price > 0.0)?;
                    let cache = cache.filter(|price| valid_price(*price))?;
                    let price = cache / base * input_price;
                    valid_price(price).then_some(price)
                };
                Self {
                    read: scale(entry.cache_read_price),
                    write: scale(entry.cache_write_price),
                    write_1h: None,
                    split_required: false,
                }
            })
            .unwrap_or_default()
    }
}

/// Explicit alias cache prices win per tier; otherwise inherit catalog ratios,
/// then matching quick-model ratios, then the historical fallback. Custom
/// Anthropic routes may use the same model catalog without sharing credentials.
pub(crate) fn cache_prices_for_model(
    provider: &str,
    model: &str,
    cfg: &crate::config::Config,
    selected_alias: Option<&str>,
    input_price: f64,
) -> CachePrices {
    let catalog_provider = if cfg.is_anthropic_native(provider) {
        "anthropic"
    } else {
        provider
    };
    let published = CachePrices::from_catalog(catalog_provider, model, input_price);
    let quick = cfg.quick_models.as_ref();
    let matches = |entry: &&crate::config::QuickModelConfig| {
        entry.provider == provider && entry.model == model
    };
    let selected = selected_alias
        .and_then(|alias| quick?.get(alias))
        .filter(|entry| matches(entry));
    let matching = quick.and_then(|quick| {
        quick
            .iter()
            .filter(|(_, entry)| matches(entry))
            .min_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(_, entry)| entry)
    });
    // An explicit custom_providers.<name>.model_prices.<model> entry
    // (mini-agent-9osxv) ranks between a selected quick-model alias and the
    // published catalog: more specific than the catalog, but a quick-model
    // alias explicitly chosen for this session still wins.
    let custom = cfg
        .custom_providers_map()
        .get(provider)
        .and_then(|custom| custom.model_prices.get(model));
    let tier = |read: bool| {
        let get = |entry: &crate::config::QuickModelConfig| {
            (if read {
                entry.cache_read_token_cost
            } else {
                entry.cache_write_token_cost
            })
            .filter(|price| valid_price(*price))
        };
        let get_custom = |entry: &crate::config::CustomProviderModelPrices| {
            (if read {
                entry.cache_read_token_cost
            } else {
                entry.cache_write_token_cost
            })
            .filter(|price| valid_price(*price))
        };
        selected
            .and_then(get)
            .or_else(|| custom.and_then(get_custom))
            .or(if read {
                published.read
            } else {
                published.write
            })
            .or_else(|| {
                let entry = matching?;
                let entry_input_price = entry.resolved_input_price()?;
                if entry_input_price == 0.0 && input_price == 0.0 {
                    return get(entry);
                }
                if !entry_input_price.is_finite()
                    || entry_input_price <= 0.0
                    || !input_price.is_finite()
                    || input_price <= 0.0
                {
                    return None;
                }
                let price = get(entry)? / entry_input_price * input_price;
                valid_price(price).then_some(price)
            })
    };
    CachePrices {
        read: tier(true),
        write: tier(false),
        write_1h: selected
            .and_then(|entry| {
                entry
                    .cache_write_1h_token_cost
                    .filter(|price| valid_price(*price))
            })
            .or_else(|| {
                custom.and_then(|entry| {
                    entry
                        .cache_write_1h_token_cost
                        .filter(|price| valid_price(*price))
                })
            })
            .or_else(|| {
                let entry = matching?;
                let base = entry.resolved_input_price()?;
                let configured = entry
                    .cache_write_1h_token_cost
                    .filter(|price| valid_price(*price))?;
                if base == 0.0 && input_price == 0.0 {
                    return Some(configured);
                }
                if base <= 0.0 || !valid_price(input_price) {
                    return None;
                }
                let rate = configured / base * input_price;
                valid_price(rate).then_some(rate)
            }),
        split_required: cfg.is_anthropic_native(provider) && cfg.native_cache_split_required(model),
    }
}

/// Effective (billable) input-token count, folding Anthropic's cache tiers into
/// a single number priced at the base input rate. The cost-side sibling of
/// [`Session::real_input_tokens`](crate::session::Session::real_input_tokens):
/// where that one answers "how big is the context" (for compaction), this
/// answers "how much input do we pay for".
///
/// Anthropic's reported `input_tokens` *excludes* both cached and cache-creation
/// tokens, but both are billed — reads at 0.10× and writes at 1.25× — so pricing
/// on raw `input_tokens` alone undercounts massively whenever caching is active
/// (the fixed request prefix — tool defs + system prompt — is re-sent every turn
/// and lands almost entirely in the cache tiers). Every other provider folds the
/// cached subset into `input_tokens` and reports no separate cache-creation, so
/// this is a no-op for them.
///
/// `anthropic_native` must be the resolved protocol route — compute it with
/// [`Config::is_anthropic_native`](crate::config::Config::is_anthropic_native),
/// exactly as `real_input_tokens` callers do.
pub fn billable_input_tokens(
    anthropic_native: bool,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
) -> u64 {
    if anthropic_native {
        (input_tokens as f64
            + cache_creation_input_tokens as f64 * ANTHROPIC_CACHE_WRITE_MULT
            + cached_input_tokens as f64 * ANTHROPIC_CACHE_READ_MULT)
            .round() as u64
    } else {
        input_tokens
    }
}

pub(crate) fn billable_input_tokens_with_multipliers(
    anthropic_native: bool,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
    multipliers: CacheMultipliers,
) -> u64 {
    if multipliers == CacheMultipliers::default() {
        return billable_input_tokens(
            anthropic_native,
            input_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
        );
    }
    if anthropic_native {
        (input_tokens as f64
            + cache_creation_input_tokens as f64 * multipliers.write
            + cached_input_tokens as f64 * multipliers.read)
            .round() as u64
    } else {
        input_tokens
    }
}

/// Estimate cost for given token counts and per-million-token prices.
/// Returns 0.0 if either price is 0.0.
///
/// `input_tokens` is the *billable* input count — for Anthropic routes, pass the
/// output of [`billable_input_tokens`] so cache reads/writes are priced, not the
/// raw provider-reported `input_tokens` (which excludes both cache tiers).
pub fn estimate_cost(
    input_tokens: u64,
    output_tokens: u64,
    input_token_cost: f64,
    output_token_cost: f64,
) -> f64 {
    let input_cost = input_tokens as f64 * input_token_cost / 1_000_000.0;
    let output_cost = output_tokens as f64 * output_token_cost / 1_000_000.0;
    input_cost + output_cost
}

#[cfg(test)]
mod cache_pricing_tests {
    use super::*;

    #[test]
    fn cache71_one_hour_rates_are_independent_validated_and_persisted() {
        let make = |hour: Option<f64>| {
            let mut entry = serde_json::json!({"provider":"anthropic", "model":"cache71-unknown",
                "input_token_cost":2.0, "output_token_cost":10.0, "cache_write_token_cost":9.0});
            if let Some(hour) = hour {
                entry["cache_write_1h_token_cost"] = hour.into();
            }
            serde_json::from_value::<crate::config::Config>(
                serde_json::json!({"quick_models":{"long":entry}}),
            )
            .unwrap()
        };
        for (hour, expected) in [(None, 4.0), (Some(0.0), 0.0), (Some(6.0), 6.0)] {
            let cfg = make(hour);
            let rates =
                cache_prices_for_model("anthropic", "cache71-unknown", &cfg, Some("long"), 2.0);
            let snapshot = crate::spend::PriceSnapshot::capture(Some((2.0, 10.0)), rates, true);
            assert_eq!(snapshot.cache_write, Some(9.0));
            assert_eq!(snapshot.cache_write_1h, Some(expected));
            let restored: crate::config::Config =
                serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
            assert_eq!(
                restored.quick_models.unwrap()["long"].cache_write_1h_token_cost,
                hour
            );
        }
        let invalid = serde_json::json!({"provider":"anthropic", "model":"cache71-unknown", "cache_write_1h_token_cost":-1.0});
        assert!(serde_json::from_value::<crate::config::QuickModelConfig>(invalid).is_err());
        let rates =
            cache_prices_for_model("anthropic", "cache71-unknown", &make(Some(6.0)), None, 4.0);
        assert_eq!(
            rates.write_1h,
            Some(12.0),
            "matching alias's own one-hour ratio uses its base, not its five-minute tier"
        );

        let usage = crate::event::UsageDelta {
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
        let cfg = make(Some(6.0));
        let rates = cache_prices_for_model("anthropic", "cache71-unknown", &cfg, Some("long"), 2.0);
        let mut session = crate::session::Session::new("anthropic", "cache71-unknown", 100_000, "");
        session.set_input_output_prices(Some((2.0, 10.0)));
        session.set_cache_prices(rates);
        let mut restored: crate::session::Session =
            serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
        assert_eq!(restored.cache_prices().write_1h, Some(6.0));
        restored.charge_usage_delta(usage, true);
        assert_eq!(restored.total_cache_creation, usage.cache_creation);
        let snapshot = crate::spend::PriceSnapshot::from_session(&restored, &cfg);
        assert_eq!(snapshot.cost(usage), Some(restored.total_cost));
        assert_eq!(
            crate::print::HeadlessPricing::from_session(&restored, &cfg).cost(usage.into()),
            restored.total_cost
        );
        assert!((restored.total_cost - 0.000136).abs() < 1e-12);
        let mut legacy = session;
        legacy.total_cache_creation_input_tokens = 1;
        legacy.charge_usage_delta(usage, true);
        assert_eq!(
            legacy.total_cache_creation, None,
            "positive legacy writes do not acquire a fabricated TTL breakdown"
        );
        let mut cfg = cfg;
        cfg.cache_ttl = Some(crate::config::PromptCacheTtl::OneHour);
        let rates = cache_prices_for_model("anthropic", "cache71-unknown", &cfg, Some("long"), 2.0);
        let mut unpriced =
            crate::session::Session::new("anthropic", "cache71-unknown", 100_000, "");
        unpriced.set_input_output_prices(Some((2.0, 10.0)));
        unpriced.set_cache_prices(rates);
        let mut missing = usage;
        missing.cache_creation = None;
        unpriced.charge_usage_delta(missing, true);
        assert_eq!(
            unpriced.total_cost, 0.0,
            "missing required tiers add no fabricated five-minute dollars"
        );
        assert!(
            crate::spend::PriceSnapshot::from_session(&unpriced, &cfg)
                .cost(missing)
                .is_none()
        );
        assert_eq!(
            crate::print::HeadlessPricing::from_session(&unpriced, &cfg).cost(missing.into()),
            0.0
        );
        #[cfg(feature = "goal")]
        {
            let resolved = crate::extras::goal::ResolvedJudge {
                label: "long".into(),
                provider: "anthropic".into(),
                model: "cache71-unknown".into(),
                same_provider_as_session: true,
                same_as_session: false,
            };
            let call = crate::extras::goal::judge::JudgeCall {
                outcome: crate::extras::goal::gate::JudgeOutcome::Unavailable {
                    reason: "fixture".into(),
                },
                tokens: 27,
                usage: missing,
            };
            let charge = call
                .charge(&resolved, &cfg, Some((2.0, 10.0)), rates)
                .unwrap();
            assert!(
                !unpriced.charge_judge_call(&charge),
                "missing required judge tiers remain unpriced"
            );
            assert_eq!(unpriced.unpriced_judge_tokens, 27);
            assert_eq!(unpriced.total_cost, 0.0);
        }
    }

    #[test]
    fn cache71_raw_free_base_keeps_matching_paid_cache_tiers() {
        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "quick_models": {"cached": {"provider": "anthropic", "model": "cache71-free-base",
                "input_token_cost": 0.0, "output_token_cost": 0.0,
                "cache_read_token_cost": 1.0, "cache_write_token_cost": 2.0,
                "cache_write_1h_token_cost": 3.0}}
        }))
        .unwrap();
        let usage = crate::event::UsageDelta {
            cached_input_tokens: 5,
            cache_creation_input_tokens: 10,
            cache_creation: Some(rig::completion::CacheCreationUsage {
                ephemeral_5m_input_tokens: 5,
                ephemeral_1h_input_tokens: 5,
            }),
            ..Default::default()
        };
        for alias in [Some("cached"), None] {
            let rates = cache_prices_for_model("anthropic", "cache71-free-base", &cfg, alias, 0.0);
            assert_eq!(rates.read, Some(1.0));
            assert_eq!(rates.write, Some(2.0));
            assert_eq!(rates.write_1h, Some(3.0));
            let snapshot = crate::spend::PriceSnapshot::capture(Some((0.0, 0.0)), rates, true);
            assert_eq!(snapshot.cost(usage), Some(0.000030));
            assert!(
                snapshot.cost(usage).unwrap() > 0.0,
                "paid cache tiers remain billable"
            );
        }
        // There is no proportional conversion from a free configured base to a
        // different paid selected base; preserve the existing ratio fallback.
        let changed = cache_prices_for_model("anthropic", "cache71-free-base", &cfg, None, 4.0);
        assert_eq!(changed.read, None);
        assert_eq!(changed.write, None);
        assert_eq!(changed.write_1h, None);
    }

    #[test]
    fn published_fable_opus_and_sonnet_cache_rates_price_native_usage() {
        // models.dev snapshot and Anthropic pricing verified 2026-10-12.
        for (model, input, output, expected_read, expected_write) in [
            ("claude-fable-5-1", 10.0, 50.0, 0.25, 12.5),
            ("claude-opus-5-5", 4.0, 20.0, 0.20, 5.0),
            ("claude-sonnet-5-5", 2.0, 10.0, 0.10, 2.5),
        ] {
            let rates = cache_prices_for_model(
                "anthropic",
                model,
                &crate::config::Config::default(),
                None,
                input,
            );
            let multipliers = rates.multipliers(input);
            assert_eq!(
                estimate_cost(
                    billable_input_tokens_with_multipliers(true, 0, 1_000_000, 0, multipliers),
                    0,
                    input,
                    output
                ),
                expected_read,
                "{model}"
            );
            assert_eq!(
                estimate_cost(
                    billable_input_tokens_with_multipliers(true, 0, 0, 1_000_000, multipliers),
                    0,
                    input,
                    output
                ),
                expected_write,
                "{model}"
            );
            // The same model on a non-native route already includes its
            // cached subset in input_tokens; cache tiers cannot be added again.
            assert_eq!(
                billable_input_tokens_with_multipliers(
                    false,
                    123,
                    1_000_000,
                    1_000_000,
                    multipliers
                ),
                123
            );
        }
    }

    #[test]
    fn zero_base_explicit_free_cache_tiers_normalize_to_free_budget_units() {
        assert_eq!(
            CachePrices {
                read: Some(0.0),
                write: Some(0.0),
                write_1h: None,
                split_required: false,
            }
            .multipliers(0.0),
            CacheMultipliers {
                read: 0.0,
                write: 0.0,
                write_1h: 2.0,
            }
        );
        assert_eq!(
            CachePrices {
                read: Some(1.0),
                write: None,
                write_1h: None,
                split_required: false,
            }
            .multipliers(0.0),
            CacheMultipliers::default()
        );
        let one_cached_token = crate::event::UsageDelta {
            cached_input_tokens: 1,
            ..Default::default()
        };
        assert_eq!(
            CachePrices {
                read: Some(1.0),
                write: None,
                write_1h: None,
                split_required: false,
            }
            .estimate_cost(true, one_cached_token, 0.0, 0.0),
            0.000001
        );
        assert_eq!(
            CachePrices {
                read: Some(0.25),
                write: Some(12.5),
                write_1h: None,
                split_required: false,
            }
            .estimate_cost(true, one_cached_token, 10.0, 50.0),
            0.0,
            "positive-base nearest-token rounding remains unchanged"
        );
    }

    #[test]
    fn unknown_missing_and_invalid_cache_rates_keep_independent_fallbacks() {
        let unknown = cache_prices_for_model(
            "anthropic",
            "unknown",
            &crate::config::Config::default(),
            None,
            10.0,
        );
        assert_eq!(unknown.multipliers(10.0), CacheMultipliers::default());
        assert_eq!(
            estimate_cost(
                billable_input_tokens_with_multipliers(
                    true,
                    0,
                    1_000_000,
                    0,
                    unknown.multipliers(10.0)
                ),
                0,
                10.0,
                50.0
            ),
            1.0
        );
        for invalid in [f64::NAN, f64::INFINITY, -1.0] {
            let rates = CachePrices {
                read: Some(invalid),
                write: Some(0.0),
                write_1h: None,
                split_required: false,
            };
            assert_eq!(
                rates.multipliers(10.0),
                CacheMultipliers {
                    read: 0.10,
                    write: 0.0,
                    write_1h: 2.0,
                }
            );
            assert_eq!(
                CachePrices {
                    read: Some(0.0),
                    write: Some(invalid),
                    write_1h: None,
                    split_required: false,
                }
                .multipliers(10.0),
                CacheMultipliers {
                    read: 0.0,
                    write: 1.25,
                    write_1h: 2.0,
                }
            );
            assert_eq!(
                rates.multipliers(invalid),
                CacheMultipliers {
                    read: 0.10,
                    write: 0.0,
                    write_1h: 2.0,
                }
            );
            assert_eq!(
                CachePrices {
                    read: Some(1.0),
                    write: None,
                    write_1h: None,
                    split_required: false,
                }
                .multipliers(invalid),
                CacheMultipliers::default(),
                "positive and missing tiers still fall back when the base is invalid"
            );
        }
        assert_eq!(
            CachePrices {
                read: Some(1.0),
                write: Some(2.0),
                write_1h: None,
                split_required: false,
            }
            .multipliers(0.0),
            CacheMultipliers::default()
        );
    }

    #[test]
    fn selected_alias_overrides_cache_tiers_and_absent_tiers_inherit_model_ratios() {
        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "quick_models": {
                "operator": {"provider":"anthropic", "model":"claude-opus-5-5", "input_token_cost":8.0, "output_token_cost":40.0, "cache_read_token_cost":0.6},
                "gateway": {"provider":"private-gateway", "model":"private-model", "input_token_cost":8.0, "cache_read_token_cost":2.0, "cache_write_token_cost":0.0}
            }
        })).unwrap();
        let selected =
            cache_prices_for_model("anthropic", "claude-opus-5-5", &cfg, Some("operator"), 8.0);
        assert_eq!(
            selected,
            CachePrices {
                read: Some(0.6),
                write: Some(10.0),
                write_1h: None,
                split_required: false,
            }
        );
        let raw = cache_prices_for_model("anthropic", "claude-opus-5-5", &cfg, None, 4.0);
        assert_eq!(
            raw,
            CachePrices {
                read: Some(0.2),
                write: Some(5.0),
                write_1h: None,
                split_required: false,
            }
        );
        let gateway = cache_prices_for_model(
            "private-gateway",
            "private-model",
            &cfg,
            Some("gateway"),
            8.0,
        );
        assert_eq!(
            gateway.multipliers(8.0),
            CacheMultipliers {
                read: 0.25,
                write: 0.0,
                write_1h: 2.0,
            }
        );
        let gateway_config = &cfg.quick_models.as_ref().unwrap()["gateway"];
        assert_eq!(gateway_config.resolved_prices(), None);
        assert_eq!(gateway_config.resolved_input_price(), Some(8.0));
        assert_eq!(
            cache_prices_for_model("private-gateway", "private-model", &cfg, None, 4.0),
            CachePrices {
                read: Some(1.0),
                write: Some(0.0),
                write_1h: None,
                split_required: false,
            },
            "a known input/cache ratio does not fabricate an omitted output rate"
        );
        // A mismatched alias cannot lend its explicit rates to another model.
        assert_eq!(
            cache_prices_for_model(
                "anthropic",
                "claude-fable-5-1",
                &cfg,
                Some("operator"),
                10.0
            )
            .read,
            Some(0.25)
        );
    }

    /// mini-agent-9osxv: a `custom_providers.<name>.model_prices` entry
    /// prices cache tiers with no `quick_models` alias involved, and a
    /// selected alias (when one does exist for this exact provider/model)
    /// still wins.
    #[test]
    fn custom_provider_model_prices_price_cache_tiers_without_a_quick_model_alias() {
        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "custom_providers": { "litellm-gw": {
                "provider_type": "openai",
                "base_url": "http://127.0.0.1:1/v1",
                "model_prices": {
                    "bedrock.anthropic.claude-sonnet-5-5": {
                        "input_token_cost": 3.0,
                        "output_token_cost": 15.0,
                        "cache_read_token_cost": 0.3,
                        "cache_write_token_cost": 3.75,
                        "cache_write_1h_token_cost": 6.0
                    }
                }
            } }
        }))
        .unwrap();
        let rates = cache_prices_for_model(
            "litellm-gw",
            "bedrock.anthropic.claude-sonnet-5-5",
            &cfg,
            None,
            3.0,
        );
        assert_eq!(
            rates,
            CachePrices {
                read: Some(0.3),
                write: Some(3.75),
                write_1h: Some(6.0),
                split_required: false,
            }
        );

        // An invalid (negative) price in the table is rejected at config
        // load, not silently accepted.
        let invalid = serde_json::json!({
            "provider_type": "openai",
            "base_url": "http://127.0.0.1:1/v1",
            "model_prices": { "m": { "input_token_cost": -1.0 } }
        });
        assert!(serde_json::from_value::<crate::config::CustomProviderConfig>(invalid).is_err());

        // An unrecognized key in a model_prices entry is rejected rather
        // than silently ignored.
        let unknown_key = serde_json::json!({
            "provider_type": "openai",
            "base_url": "http://127.0.0.1:1/v1",
            "model_prices": { "m": { "typo_token_cost": 1.0 } }
        });
        assert!(
            serde_json::from_value::<crate::config::CustomProviderConfig>(unknown_key).is_err()
        );
    }
}
