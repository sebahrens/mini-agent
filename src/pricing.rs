/// Fallback rates for models without published or configured cache pricing.
/// Writes assume the default five-minute TTL used by the production client.
const ANTHROPIC_CACHE_WRITE_MULT: f64 = 1.25;
const ANTHROPIC_CACHE_READ_MULT: f64 = 0.10;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct CachePrices {
    pub read: Option<f64>,
    pub write: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheMultipliers {
    pub read: f64,
    pub write: f64,
}

impl Default for CacheMultipliers {
    fn default() -> Self {
        Self {
            read: ANTHROPIC_CACHE_READ_MULT,
            write: ANTHROPIC_CACHE_WRITE_MULT,
        }
    }
}

fn valid_price(price: f64) -> bool {
    price.is_finite() && price >= 0.0
}

impl CachePrices {
    #[cfg(feature = "goal")]
    pub(crate) fn has_valid_tier(self) -> bool {
        [self.read, self.write]
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
    let tier = |read: bool| {
        let get = |entry: &crate::config::QuickModelConfig| {
            (if read {
                entry.cache_read_token_cost
            } else {
                entry.cache_write_token_cost
            })
            .filter(|price| valid_price(*price))
        };
        selected
            .and_then(get)
            .or(if read {
                published.read
            } else {
                published.write
            })
            .or_else(|| {
                let entry = matching?;
                if !entry.input_token_cost.is_finite()
                    || entry.input_token_cost <= 0.0
                    || !input_price.is_finite()
                    || input_price <= 0.0
                {
                    return None;
                }
                let price = get(entry)? / entry.input_token_cost * input_price;
                valid_price(price).then_some(price)
            })
    };
    CachePrices {
        read: tier(true),
        write: tier(false),
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
                write: Some(0.0)
            }
            .multipliers(0.0),
            CacheMultipliers {
                read: 0.0,
                write: 0.0
            }
        );
        assert_eq!(
            CachePrices {
                read: Some(1.0),
                write: None
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
                write: None
            }
            .estimate_cost(true, one_cached_token, 0.0, 0.0),
            0.000001
        );
        assert_eq!(
            CachePrices {
                read: Some(0.25),
                write: Some(12.5)
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
            };
            assert_eq!(
                rates.multipliers(10.0),
                CacheMultipliers {
                    read: 0.10,
                    write: 0.0
                }
            );
            assert_eq!(
                CachePrices {
                    read: Some(0.0),
                    write: Some(invalid)
                }
                .multipliers(10.0),
                CacheMultipliers {
                    read: 0.0,
                    write: 1.25
                }
            );
            assert_eq!(
                rates.multipliers(invalid),
                CacheMultipliers {
                    read: 0.10,
                    write: 0.0
                }
            );
            assert_eq!(
                CachePrices {
                    read: Some(1.0),
                    write: None
                }
                .multipliers(invalid),
                CacheMultipliers::default(),
                "positive and missing tiers still fall back when the base is invalid"
            );
        }
        assert_eq!(
            CachePrices {
                read: Some(1.0),
                write: Some(2.0)
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
                write: Some(10.0)
            }
        );
        let raw = cache_prices_for_model("anthropic", "claude-opus-5-5", &cfg, None, 4.0);
        assert_eq!(
            raw,
            CachePrices {
                read: Some(0.2),
                write: Some(5.0)
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
                write: 0.0
            }
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
}
