//! Captured, provider-independent task work units. These are not USD or GPU time.

use rig::completion::Usage;

use crate::config::{Config, TaskCostWeighting, TaskCostWeights};

#[derive(Clone, Copy, Debug)]
pub(crate) struct CostPolicy {
    pub(crate) weights: TaskCostWeights,
    pub(crate) source: &'static str,
    pub(crate) native_input: bool,
    pub(crate) exclusive_reasoning: bool,
    pub(crate) split_required: bool,
}

impl CostPolicy {
    pub(crate) fn resolve(cfg: &Config, provider: &str, model: &str, alias: Option<&str>) -> Self {
        // An opaque gateway alias is not evidence of an upstream model. Only
        // the selected quick model, bound to this exact route, can override it.
        let selected = alias
            .and_then(|name| cfg.quick_models.as_ref()?.get(name))
            .filter(|entry| entry.provider == provider && entry.model == model);
        let local = selected.and_then(|_| cfg.task_cost_overrides.as_ref()?.get(alias?));
        let mode = local
            .and_then(|entry| entry.weighting)
            .unwrap_or_else(|| cfg.resolve_task_cost_weighting());
        let mut weights = local
            .and_then(|entry| entry.weights)
            .unwrap_or_else(|| cfg.resolve_task_cost_weights());
        let mut policy = Self {
            weights,
            source: "configured fallback",
            native_input: cfg.is_anthropic_native(provider),
            exclusive_reasoning: cfg.reasoning_tokens_are_exclusive_of_output(provider),
            split_required: cfg.native_cache_split_required(model),
        };
        if mode == TaskCostWeighting::Fixed {
            policy.source = "fixed settings";
            return policy;
        }
        let catalog_provider = if policy.native_input {
            "anthropic"
        } else {
            provider
        };
        let catalog = Config::catalog_input_output_cost(catalog_provider, model);
        let base = selected
            .and_then(|entry| entry.input_token_cost)
            .or(catalog.map(|price| price.0));
        let Some(base) = base.filter(|price| price.is_finite() && *price > 0.0) else {
            // Free/on-prem pricing does not disable the work budget. In
            // particular, do not divide by zero or turn all work into zero.
            return policy;
        };
        let published = crate::pricing::CachePrices::from_catalog(catalog_provider, model, base);
        let ratio = |price: Option<f64>, fallback: f64, positive: bool| {
            price
                .map(|price| price / base * weights.input)
                .filter(|ratio| {
                    ratio.is_finite()
                        && if positive {
                            *ratio > 0.0
                        } else {
                            *ratio >= 0.0
                        }
                })
                .unwrap_or(fallback)
        };
        weights.output = ratio(
            selected
                .and_then(|entry| entry.output_token_cost)
                .or(catalog.map(|price| price.1)),
            weights.output,
            true,
        );
        weights.cache_read = ratio(
            selected
                .and_then(|entry| entry.cache_read_token_cost)
                .or(published.read),
            if policy.native_input {
                weights.input * 0.1
            } else {
                weights.cache_read
            },
            false,
        );
        weights.cache_write_5m = ratio(
            selected
                .and_then(|entry| entry.cache_write_token_cost)
                .or(published.write),
            if policy.native_input {
                weights.input * 1.25
            } else {
                weights.cache_write_5m
            },
            false,
        );
        weights.cache_write_1h = ratio(
            selected.and_then(|entry| entry.cache_write_1h_token_cost),
            if policy.native_input {
                weights.input * 2.0
            } else {
                weights.cache_write_1h
            },
            false,
        );
        policy.weights = weights;
        policy.source = "model pricing with configured fallbacks";
        policy
    }

    pub(crate) fn units(self, usage: &Usage) -> u64 {
        let w = self.weights;
        let reads = usage.cached_input_tokens;
        let writes = usage.cache_creation_input_tokens;
        let split = usage
            .cache_creation
            .filter(|split| split.total() == Some(writes));
        let output = usage
            .output_tokens
            .saturating_add(if self.exclusive_reasoning {
                usage.reasoning_tokens
            } else {
                0
            });
        let mut amount = output as f64 * w.output + usage.tool_use_prompt_tokens as f64 * w.input;

        let cache_total = reads.checked_add(writes);
        let uncached = if self.native_input {
            Some(usage.input_tokens)
        } else {
            cache_total.and_then(|total| usage.input_tokens.checked_sub(total))
        };
        if let Some(uncached) = uncached {
            amount += uncached as f64 * w.input + reads as f64 * w.cache_read;
            amount += if let Some(split) = split {
                split.ephemeral_5m_input_tokens as f64 * w.cache_write_5m
                    + split.ephemeral_1h_input_tokens as f64 * w.cache_write_1h
            } else {
                // Gateway cache-write counts do not identify a TTL. Use the
                // greater configured rate; never infer a five-minute write.
                let write_weight = if self.split_required
                    || !self.native_input
                    || usage.cache_creation.is_some()
                {
                    w.cache_write_5m.max(w.cache_write_1h)
                } else {
                    w.cache_write_5m
                };
                writes as f64 * write_weight
            };
        } else {
            // Inconsistent inclusive counters cannot earn a cache discount.
            // Bound all reported input conservatively, including a premium
            // write tier, without overflowing a u64 sum of cache counters.
            let input = usage.input_tokens.max(reads.saturating_add(writes));
            amount += input as f64
                * w.input
                    .max(w.cache_read)
                    .max(w.cache_write_5m)
                    .max(w.cache_write_1h);
        }
        if !usage.has_values() {
            return 0;
        }
        if usage.input_tokens == 0
            && output == 0
            && reads == 0
            && writes == 0
            && usage.tool_use_prompt_tokens == 0
        {
            // Only a combined total is available: charge the largest weight,
            // since neither the input/output split nor cache status is known.
            amount = usage.total_tokens as f64
                * w.input
                    .max(w.output)
                    .max(w.cache_read)
                    .max(w.cache_write_5m)
                    .max(w.cache_write_1h);
        }
        if !amount.is_finite() {
            u64::MAX
        } else {
            amount.ceil() as u64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(native: bool, exclusive: bool) -> CostPolicy {
        CostPolicy {
            weights: TaskCostWeights::default(),
            source: "test",
            native_input: native,
            exclusive_reasoning: exclusive,
            split_required: true,
        }
    }

    #[test]
    fn task130_weighted_work_budget_covers_eight_children_and_full_window() {
        let usage = Usage {
            input_tokens: 200_000,
            output_tokens: 32_000,
            reasoning_tokens: 16_000,
            ..Usage::new()
        };
        assert_eq!(fixed(false, false).units(&usage) * 8, 2_880_000);
        let full = Usage {
            cache_creation_input_tokens: 1_048_576 - 32_000,
            output_tokens: 32_000,
            ..Usage::new()
        };
        assert_eq!(fixed(true, false).units(&full), 2_193_152);
        assert!(fixed(true, false).units(&full) < 4_000_000);
    }

    #[test]
    fn task130_gateway_and_native_cache_categories_are_disjoint() {
        let mut policy = fixed(false, false);
        policy.weights.cache_read = 0.1;
        policy.weights.cache_write_5m = 1.25;
        let usage = Usage {
            input_tokens: 100,
            cached_input_tokens: 60,
            cache_creation_input_tokens: 20,
            output_tokens: 10,
            reasoning_tokens: 8,
            ..Usage::new()
        };
        assert_eq!(policy.units(&usage), 116); // 20 + 6 + 40 unknown-TTL writes + 50.
        let native = Usage {
            input_tokens: 20,
            ..usage
        };
        policy.native_input = true;
        assert_eq!(policy.units(&native), 116);
        policy.exclusive_reasoning = true;
        assert_eq!(policy.units(&native), 156);
        let split = Usage {
            cache_creation: Some(rig::completion::CacheCreationUsage {
                ephemeral_5m_input_tokens: 20,
                ephemeral_1h_input_tokens: 0,
            }),
            ..native
        };
        assert_eq!(policy.units(&split), 141);
    }

    #[test]
    fn task130_unpriced_routes_and_zero_prices_still_consume_work() {
        for provider in ["vllm", "litellm-corp", "openrouter", "openai"] {
            let cfg: Config = serde_json::from_value(serde_json::json!({
                "quick_models":{"private":{"provider":provider,"model":"opaque-private", "input_token_cost":0,"output_token_cost":0}}
            })).unwrap();
            let policy = CostPolicy::resolve(&cfg, provider, "opaque-private", Some("private"));
            assert_eq!(policy.weights, TaskCostWeights::default());
            assert_eq!(
                policy.units(&Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    ..Usage::new()
                }),
                150
            );
        }
    }

    #[test]
    fn task130_exact_route_alias_prices_and_manual_overrides_win() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "quick_models":{
                "corp":{"provider":"litellm-corp","model":"route","input_token_cost":2,"output_token_cost":16,"cache_read_token_cost":0.2},
                "other":{"provider":"elsewhere","model":"route","input_token_cost":1,"output_token_cost":100}
            },
            "task_cost_overrides":{"corp":{"weighting":"fixed","weights":{"output":3}}}
        })).unwrap();
        let fixed = CostPolicy::resolve(&cfg, "litellm-corp", "route", Some("corp"));
        assert_eq!(fixed.weights.output, 3.0);
        assert_eq!(fixed.source, "fixed settings");
        let mut auto = cfg.clone();
        auto.task_cost_overrides = None;
        let priced = CostPolicy::resolve(&auto, "litellm-corp", "route", Some("corp"));
        assert_eq!(priced.weights.output, 8.0);
        assert_eq!(priced.weights.cache_read, 0.1);
        let foreign = CostPolicy::resolve(&cfg, "litellm-corp", "route", Some("other"));
        assert_eq!(foreign.weights, TaskCostWeights::default());
        let no_alias = CostPolicy::resolve(&cfg, "litellm-corp", "route", None);
        assert_eq!(no_alias.weights, TaskCostWeights::default());
    }

    #[test]
    fn task130_bad_cache_counts_total_only_rounding_and_overflow_are_bounded() {
        let policy = fixed(false, false);
        assert_eq!(
            policy.units(&Usage {
                total_tokens: 10,
                ..Usage::new()
            }),
            50
        );
        assert_eq!(
            policy.units(&Usage {
                input_tokens: 2,
                cached_input_tokens: 10,
                ..Usage::new()
            }),
            20
        );
        assert_eq!(
            policy.units(&Usage {
                input_tokens: u64::MAX,
                output_tokens: u64::MAX,
                ..Usage::new()
            }),
            u64::MAX
        );
        let mut tiny = policy;
        tiny.weights.cache_read = 3.0;
        assert_eq!(
            tiny.units(&Usage {
                input_tokens: 2,
                cached_input_tokens: 10,
                ..Usage::new()
            }),
            30
        );
        tiny.weights.output = 0.001;
        assert_eq!(
            tiny.units(&Usage {
                output_tokens: 1,
                ..Usage::new()
            }),
            1
        );
        assert_eq!(policy.units(&Usage::new()), 0);
    }

    #[test]
    fn task130_derived_native_weight_overflow_cannot_make_usage_free() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "task_cost_weights":{"input":1e308},
            "quick_models":{"native":{"provider":"anthropic","model":"private","input_token_cost":1,"output_token_cost":5}}
        })).unwrap();
        let policy = CostPolicy::resolve(&cfg, "anthropic", "private", Some("native"));
        assert_eq!(
            policy.units(&Usage {
                input_tokens: 1,
                ..Usage::new()
            }),
            u64::MAX
        );
    }
}
