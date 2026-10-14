//! Native cache usage capture before any TTL/rate changes.

use super::*;

#[tokio::test]
async fn cache71_typed_ttl_factory_preserves_default_and_sets_native_one_hour_wire() {
    use crate::config::PromptCacheTtl;
    for stream in [false, true] {
        for ttl in [PromptCacheTtl::FiveMinutes, PromptCacheTtl::OneHour] {
            let response = if stream {
                start() + &text(0, "done") + &finish()
            } else {
                json!({"id":"msg_ttl","type":"message","role":"assistant","content":[{"type":"text","text":"done"}],"model":"claude-opus-5-5","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":2}}).to_string()
            };
            let wire = Wire::new(vec![(stream, response)]);
            let AnyModel::Anthropic(model) = wire
                .client()
                .completion_model_with_cache_ttl("claude-opus-5-5", ttl)
            else {
                unreachable!()
            };
            let mut request = model
                .completion_request("cache policy")
                .preamble("stable system".to_string())
                .max_tokens(1000)
                .build();
            request.tools.push(rig::completion::ToolDefinition {
                name: "static_tool".into(),
                description: "No effect is executed by this wire probe".into(),
                parameters: json!({"type":"object","properties":{}}),
            });
            if stream {
                let mut response = model.stream(request).await.unwrap();
                while let Some(part) = response.next().await {
                    part.unwrap();
                }
            } else {
                model.completion(request).await.unwrap();
            }
            let requests = wire.finish();
            assert_eq!(requests.len(), 1);
            let body = &requests[0];
            let marker = if ttl == PromptCacheTtl::OneHour {
                json!({"type":"ephemeral","ttl":"1h"})
            } else {
                json!({"type":"ephemeral"})
            };
            assert_eq!(body["system"][0]["cache_control"], marker);
            assert_eq!(body["tools"][0]["cache_control"], marker);
            if ttl == PromptCacheTtl::OneHour {
                assert_eq!(body["cache_control"], marker);
                assert!(
                    body["messages"][0]["content"][0]
                        .get("cache_control")
                        .is_none()
                );
            } else {
                assert!(body.get("cache_control").is_none());
                assert_eq!(body["messages"][0]["content"][0]["cache_control"], marker);
            }
        }
    }
}

#[tokio::test]
async fn cache71_native_start_cache_usage_survives_output_only_delta() {
    let response = event(
        "message_start",
        json!({
            "type":"message_start", "message":{
                "id":"msg_cache_probe", "type":"message", "role":"assistant",
                "model":"claude-opus-5-5", "content":[], "stop_reason":null,
                "stop_sequence":null,"usage":{
                    "input_tokens":10,"output_tokens":0,
                    "cache_read_input_tokens":120,"cache_creation_input_tokens":80,
                    "cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":50}
                }
            }
        }),
    ) + &text(0, "done")
        + &finish();
    let wire = Wire::new(vec![(true, response)]);
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    let mut stream = model
        .completion_request("cache usage capture")
        .max_tokens(1000)
        .stream()
        .await
        .unwrap();
    while let Some(item) = stream.next().await {
        item.unwrap();
    }
    let usage = stream.usage();
    let raw = serde_json::to_value(stream.response.as_ref().unwrap()).unwrap();
    assert_eq!(
        usage.cache_creation,
        Some(rig::completion::CacheCreationUsage {
            ephemeral_5m_input_tokens: 30,
            ephemeral_1h_input_tokens: 50,
        })
    );
    println!(
        "CACHE71_USAGE_CAPTURE={}",
        json!({"generic_usage":usage,"raw_native_response":raw})
    );
    assert_eq!(wire.finish().len(), 1);
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 2);
    assert_eq!(
        usage.cached_input_tokens, 120,
        "message_start cache hits must not disappear"
    );
    assert_eq!(
        usage.cache_creation_input_tokens, 80,
        "message_start cache writes must not disappear"
    );
    assert_eq!(
        raw["usage"]["cache_creation"]["ephemeral_5m_input_tokens"],
        30
    );
    assert_eq!(
        raw["usage"]["cache_creation"]["ephemeral_1h_input_tokens"],
        50
    );
}

fn cached_start(usage: Value) -> String {
    event(
        "message_start",
        json!({"type":"message_start","message":{
            "id":"msg_cache_delta","type":"message","role":"assistant","model":"claude-opus-5-5",
            "content":[],"stop_reason":null,"stop_sequence":null,"usage":usage
        }}),
    )
}

fn cached_delta(stop: bool, usage: Value) -> String {
    event(
        "message_delta",
        json!({"type":"message_delta","delta":{
        "stop_reason":if stop {Some("end_turn")} else {None},"stop_sequence":null
    },"usage":usage}),
    )
}

#[tokio::test]
async fn cache71_native_cumulative_delta_overwrites_and_does_not_infer_missing_split() {
    let initial = json!({"input_tokens":10,"output_tokens":0,"cache_read_input_tokens":120,
        "cache_creation_input_tokens":80,"cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":50}});
    for (delta, expected_read, expected_write, expected_split) in [
        (
            json!({"input_tokens":20,"output_tokens":2,"cache_read_input_tokens":140,
            "cache_creation_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":40,"ephemeral_1h_input_tokens":60}}),
            140,
            100,
            Some(json!({"ephemeral_5m_input_tokens":40,"ephemeral_1h_input_tokens":60})),
        ),
        (
            json!({"input_tokens":20,"output_tokens":2,"cache_read_input_tokens":0,
            "cache_creation_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0}}),
            0,
            0,
            Some(json!({"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0})),
        ),
        (
            json!({"input_tokens":20,"output_tokens":2,"cache_creation_input_tokens":100}),
            130,
            100,
            None,
        ),
    ] {
        let body = cached_start(initial.clone())
            + &text(0, "done")
            + &cached_delta(
                false,
                json!({"output_tokens":1,"cache_read_input_tokens":130}),
            )
            + &cached_delta(true, delta)
            + &event("message_stop", json!({"type":"message_stop"}));
        let wire = Wire::new(vec![(true, body)]);
        let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
            unreachable!()
        };
        let mut response = model
            .completion_request("cumulative usage")
            .max_tokens(1000)
            .stream()
            .await
            .unwrap();
        while let Some(item) = response.next().await {
            item.unwrap();
        }
        let usage = response.usage();
        let raw = serde_json::to_value(response.response.as_ref().unwrap()).unwrap();
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.cached_input_tokens, expected_read);
        assert_eq!(usage.cache_creation_input_tokens, expected_write);
        match expected_split {
            Some(split) => assert_eq!(raw["usage"]["cache_creation"], split),
            None => assert!(raw["usage"].get("cache_creation").is_none()),
        }
        assert_eq!(wire.finish().len(), 1);
    }
}

#[tokio::test]
async fn cache71_native_invalid_reported_counts_fail_without_inventing_usage() {
    for (usage, expected) in [
        (
            json!({"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":79,
            "cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":50}}),
            "inconsistent Anthropic cache creation usage",
        ),
        (
            json!({"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":u64::MAX,
            "cache_creation":{"ephemeral_5m_input_tokens":u64::MAX,"ephemeral_1h_input_tokens":1}}),
            "Anthropic cache creation usage exceeds limits",
        ),
        (
            json!({"input_tokens":10,"output_tokens":0,"cache_read_input_tokens":u64::MAX}),
            "Anthropic token usage exceeds limits",
        ),
    ] {
        let wire = Wire::new(vec![(
            true,
            cached_start(usage) + &text(0, "unused") + &finish(),
        )]);
        let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
            unreachable!()
        };
        let mut stream = model
            .completion_request("malformed usage")
            .max_tokens(1000)
            .stream()
            .await
            .unwrap();
        let mut errors = Vec::new();
        while let Some(item) = stream.next().await {
            if let Err(error) = item {
                errors.push(error);
            }
        }
        assert_eq!(errors.len(), 1);
        let rig::completion::CompletionError::ResponseError(error) = errors.pop().unwrap() else {
            panic!("expected static response error")
        };
        assert_eq!(error, expected);
        assert_eq!(stream.usage(), rig::completion::Usage::new());
        assert_eq!(wire.finish().len(), 1);
    }
}

#[tokio::test]
async fn cache71_native_blocking_preserves_reported_split_and_rejects_inconsistent_total() {
    for total in [80, 79] {
        let response = json!({"id":"msg_cache_blocking","type":"message","role":"assistant",
            "content":[{"type":"text","text":"done"}],"model":"claude-opus-5-5",
            "stop_reason":"end_turn","stop_sequence":null,
            "usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":120,
                "cache_creation_input_tokens":total,"cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":50}}});
        let wire = Wire::new(vec![(false, response.to_string())]);
        let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
            unreachable!()
        };
        let result = model
            .completion(
                model
                    .completion_request("blocking cache")
                    .max_tokens(1000)
                    .build(),
            )
            .await;
        if total == 80 {
            let response = result.unwrap();
            assert_eq!(response.usage.cached_input_tokens, 120);
            assert_eq!(response.usage.cache_creation_input_tokens, 80);
            assert_eq!(
                response.usage.cache_creation,
                Some(rig::completion::CacheCreationUsage {
                    ephemeral_5m_input_tokens: 30,
                    ephemeral_1h_input_tokens: 50,
                })
            );
            let raw = serde_json::to_value(response.raw_response).unwrap();
            assert_eq!(
                raw["usage"]["cache_creation"],
                json!({"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":50})
            );
        } else {
            let rig::completion::CompletionError::ResponseError(error) = result.err().unwrap()
            else {
                panic!("expected response error")
            };
            assert_eq!(error, "inconsistent Anthropic cache creation usage");
        }
        assert_eq!(wire.finish().len(), 1);
    }
}

#[test]
fn cache71_generic_tier_carrier_keeps_unknownness_zero_sentinel_and_checked_totals() {
    use rig::completion::{CacheCreationUsage as Split, Usage};
    let mixed = Split {
        ephemeral_5m_input_tokens: 30,
        ephemeral_1h_input_tokens: 50,
    };
    let earlier = Split {
        ephemeral_5m_input_tokens: 10,
        ephemeral_1h_input_tokens: 20,
    };
    assert_eq!(Split::add_optional(0, None, 80, Some(mixed)), Some(mixed));
    assert_eq!(Split::add_optional(80, Some(mixed), 0, None), Some(mixed));
    assert_eq!(Split::add_optional(1, None, 80, Some(mixed)), None);
    assert_eq!(Split::add_optional(80, Some(mixed), 1, None), None);
    assert_eq!(Split::add_optional(0, None, 0, None), None);
    assert_eq!(
        Split::difference_optional(80, Some(mixed), 0, None),
        Some(mixed)
    );
    assert_eq!(
        Split::difference_optional(80, Some(mixed), 30, Some(earlier)),
        Some(Split {
            ephemeral_5m_input_tokens: 20,
            ephemeral_1h_input_tokens: 30
        })
    );
    assert_eq!(Split::difference_optional(80, Some(mixed), 1, None), None);
    assert_eq!(Split::difference_optional(79, Some(mixed), 0, None), None);
    assert_eq!(
        Split::difference_optional(30, Some(earlier), 80, Some(mixed)),
        None
    );
    assert_eq!(
        Split::add_optional(
            u64::MAX,
            Some(Split {
                ephemeral_5m_input_tokens: u64::MAX,
                ephemeral_1h_input_tokens: 0
            }),
            1,
            Some(Split {
                ephemeral_5m_input_tokens: 0,
                ephemeral_1h_input_tokens: 1
            })
        ),
        None
    );
    let mut zero = Usage::new();
    zero.cache_creation = Some(Split {
        ephemeral_5m_input_tokens: 0,
        ephemeral_1h_input_tokens: 0,
    });
    assert!(
        !zero.has_values(),
        "zero tier metadata is not proof of paid usage"
    );
    let reported = Usage {
        cache_creation_input_tokens: 80,
        cache_creation: Some(mixed),
        ..Usage::new()
    };
    assert_eq!((Usage::new() + reported).cache_creation, Some(mixed));
    let mut aggregate = Usage::new();
    aggregate += reported;
    assert_eq!(aggregate.cache_creation, Some(mixed));
    assert_eq!(
        (reported
            + Usage {
                cache_creation_input_tokens: 1,
                ..Usage::new()
            })
        .cache_creation,
        None
    );
    assert!(
        serde_json::to_value(Usage::new())
            .unwrap()
            .get("cache_creation")
            .is_none()
    );
    assert!(
        serde_json::from_value::<Split>(
            json!({"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":50,"unknown":1})
        )
        .is_err()
    );
}

#[test]
fn cache71_application_usage_tiers_survive_conversion_and_reconciliation_without_false_presence() {
    use rig::completion::{CacheCreationUsage as Split, Usage};
    let mixed = Split {
        ephemeral_5m_input_tokens: 30,
        ephemeral_1h_input_tokens: 50,
    };
    let reported = Usage {
        input_tokens: 10,
        output_tokens: 2,
        total_tokens: 92,
        cache_creation_input_tokens: 80,
        cache_creation: Some(mixed),
        ..Usage::new()
    };
    let delta: crate::event::UsageDelta = reported.into();
    assert_eq!(delta.cache_creation, Some(mixed));
    assert_eq!(Usage::from(delta), reported);
    assert_eq!(
        crate::event::UsageDelta::default()
            .saturating_add(delta)
            .cache_creation,
        Some(mixed)
    );
    assert_eq!(
        delta
            .nonnegative_difference(Default::default())
            .cache_creation,
        Some(mixed)
    );
    let zero = crate::event::UsageDelta {
        cache_creation: Some(Split {
            ephemeral_5m_input_tokens: 0,
            ephemeral_1h_input_tokens: 0,
        }),
        ..Default::default()
    };
    assert!(!zero.has_values());
    let mut ledger = crate::agent::runner::UsageLedger::default();
    assert_eq!(ledger.record(reported).cache_creation, Some(mixed));
    let mut terminal = reported;
    terminal.cache_creation_input_tokens = 100;
    terminal.total_tokens = 112;
    terminal.cache_creation = Some(Split {
        ephemeral_5m_input_tokens: 40,
        ephemeral_1h_input_tokens: 60,
    });
    let remaining = ledger.reconcile_terminal(terminal);
    assert_eq!(remaining.cache_creation_input_tokens, 20);
    assert_eq!(
        remaining.cache_creation,
        Some(Split {
            ephemeral_5m_input_tokens: 10,
            ephemeral_1h_input_tokens: 10
        })
    );
    assert_eq!(
        crate::agent::runner::usage_saturating_add(
            reported,
            Usage {
                cache_creation_input_tokens: 1,
                ..Usage::new()
            }
        )
        .cache_creation,
        None
    );
}

#[tokio::test]
async fn cache71_actual_canonical_mixed_receipt_and_missing_split_remain_honest() {
    use crate::spend::{Account, Ledger, PriceSnapshot, run::Run};
    for reported in [true, false] {
        let root = crate::tests::OwnedTestDirectory::new("cache71-canonical");
        let state = root.path().join("state");
        crate::fs::ensure_private_directory(&state).unwrap();
        let cfg: crate::config::Config = serde_json::from_value(json!({"cache_ttl":"1h", "quick_models":{"cache71":{
            "provider":"anthropic", "model":"claude-opus-5-5", "input_token_cost":2.0, "output_token_cost":10.0,
            "cache_read_token_cost":0.2, "cache_write_token_cost":9.0, "cache_write_1h_token_cost":6.0
        }}})).unwrap();
        let mut usage = json!({"input_tokens":8,"output_tokens":0,
            "cache_read_input_tokens":4,"cache_creation_input_tokens":10});
        if reported {
            usage["cache_creation"] =
                json!({"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":7});
        }
        let response = cached_start(usage)
            + &text(0, "done")
            + &cached_delta(true, json!({"output_tokens":5}))
            + &event("message_stop", json!({"type":"message_stop"}));
        let wire = Wire::new(vec![(true, response)]);
        let AnyModel::Anthropic(model) = wire
            .client()
            .completion_model_with_cache_ttl("claude-opus-5-5", cfg.cache_ttl.unwrap_or_default())
        else {
            unreachable!()
        };
        let inner = rig::agent::AgentBuilder::new(model)
            .max_tokens(1000)
            .default_max_turns(1)
            .build();
        let ledger = Arc::new(Ledger::new(&state));
        let account = Account::new("cache71-canonical", Some(1.0), 0.0).unwrap();
        let rates = crate::pricing::cache_prices_for_model(
            "anthropic",
            "claude-opus-5-5",
            &cfg,
            Some("cache71"),
            2.0,
        );
        let prices = PriceSnapshot::capture(Some((2.0, 10.0)), rates, true)
            .with_cache_policy(&cfg, "claude-opus-5-5");
        let profile = Run::fixture(account.clone(), ledger.clone(), "cache71-canonical", prices)
            .with_captured_profile(
                "anthropic",
                "claude-opus-5-5",
                prices,
                &cfg,
                crate::spend::RequestKind::Turn,
            );
        let agent =
            AnyAgent::without_skills(AnyAgentInner::Anthropic(inner)).with_spend(Some(profile));
        let turn = agent
            .run_print_with_output(
                "actual canonical cache accounting",
                crate::print::HeadlessOutputPolicy::legacy(false, false),
                &RetryConfig {
                    max_attempts: 1,
                    ..Default::default()
                },
                Vec::<Message>::new(),
                #[cfg(feature = "hooks")]
                None,
            )
            .await;
        assert_eq!(turn.usage.input_tokens, 8);
        assert_eq!(turn.usage.output_tokens, 5);
        assert_eq!(turn.usage.cache_creation_input_tokens, 10);
        let rows = ledger.summary().unwrap().all_time;
        assert_eq!(
            rows.records, 1,
            "SDK projection/canonical report charged twice"
        );
        assert_eq!(rows.cache_creation_input_tokens, 10);
        assert_eq!(rows.cache_creation.is_some(), reported);
        assert!(!account.has_pending());
        let bytes = std::fs::read(state.join("usage.jsonl")).unwrap();
        let row: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(row["version"], 2);
        assert_eq!(row["provider"], "anthropic");
        assert_eq!(row["model"], "claude-opus-5-5");
        if reported {
            assert!(turn.failure.is_none(), "{:?}", turn.failure);
            assert_eq!(
                turn.usage.cache_creation.unwrap().ephemeral_1h_input_tokens,
                7
            );
            assert!((rows.known_cost - 0.000136).abs() < 1e-12);
            assert!((account.snapshot().recorded_spend() - 0.000136).abs() < 1e-12);
            assert!(!account.snapshot().tracking_error);
            assert_eq!(row["prices"]["cache_write_1h"], 6.0);
            assert_eq!(row["prices"]["cache_write"], 9.0);
            assert_eq!(
                row["usage"]["cache_creation"]["ephemeral_5m_input_tokens"],
                3
            );
        } else {
            assert!(
                turn.failure.is_none(),
                "valid completed answer remains usable"
            );
            assert_eq!(turn.response, "done");
            assert!(turn.usage.cache_creation.is_none());
            assert_eq!(rows.unpriced_records, 1);
            assert_eq!(row["cost"], Value::Null);
            assert!(account.snapshot().tracking_error);
            assert!(account.admit_effect().is_err());
            let next = Run::fixture(account.clone(), ledger.clone(), "cache71-canonical", prices)
                .with_captured_profile(
                    "anthropic",
                    "claude-opus-5-5",
                    prices,
                    &cfg,
                    crate::spend::RequestKind::Turn,
                );
            let refused = agent
                .clone()
                .with_spend(Some(next))
                .run_print_with_output(
                    "required capped continuation",
                    crate::print::HeadlessOutputPolicy::legacy(false, false),
                    &RetryConfig {
                        max_attempts: 1,
                        ..Default::default()
                    },
                    Vec::<Message>::new(),
                    #[cfg(feature = "hooks")]
                    None,
                )
                .await;
            assert!(
                refused
                    .failure
                    .as_ref()
                    .is_some_and(|error| error.is::<crate::spend::run::BudgetStopped>()),
                "{:?}",
                refused.failure
            );
            assert_eq!(ledger.summary().unwrap().all_time.records, 1);
        }
        let requests = wire.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
    }
}
