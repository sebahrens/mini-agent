//! Hosted web search is opt-in, default closed, and limited to providers with
//! a supported bridge.

use super::*;
use clap::Parser;
use serde_json::json;

fn model(provider_type: &str, api_style: &str) -> AnyModel {
    let config: Config = serde_json::from_value(json!({
        "custom_providers":{"hosted":{"provider_type":provider_type,"base_url":"http://127.0.0.1:9","api_style":api_style}}
    }))
    .unwrap();
    create_client("hosted", Some("fake"), config.custom_providers_map(), None)
        .unwrap()
        .completion_model("m")
}

fn cfg(hosted: Option<bool>) -> Config {
    Config {
        hosted_web_search: hosted,
        ..Default::default()
    }
}

fn cli() -> Cli {
    Cli::parse_from(["mini-agent"])
}

#[test]
fn hosted_web_search_is_closed_by_default() {
    for m in [
        model("anthropic", "responses"),
        model("openai", "responses"),
    ] {
        for hosted in [None, Some(false)] {
            let out = with_hosted_web_search(&m, &cli(), &cfg(hosted), None);
            assert_eq!(out, None, "default must not add any hosted tool");
            assert!(!declares_hosted_web_search(out.as_ref()));
        }
    }
}

#[test]
fn hosted_web_search_bridges_anthropic_and_openai_responses_when_enabled() {
    let out = with_hosted_web_search(
        &model("anthropic", "responses"),
        &cli(),
        &cfg(Some(true)),
        None,
    );
    assert_eq!(
        out.as_ref().unwrap()["tools"][0]["type"],
        "web_search_20250305"
    );
    assert!(declares_hosted_web_search(out.as_ref()));
    let out = with_hosted_web_search(
        &model("openai", "responses"),
        &cli(),
        &cfg(Some(true)),
        None,
    );
    assert_eq!(
        out.as_ref().unwrap()["tools"],
        json!([{"type":"web_search"}])
    );
}

#[test]
fn hosted_web_search_keeps_user_params_and_never_duplicates() {
    let m = model("anthropic", "responses");
    let user = json!({"temperature":0.2,"tools":[{"type":"custom_thing"}]});
    let out = with_hosted_web_search(&m, &cli(), &cfg(Some(true)), Some(user)).unwrap();
    assert_eq!(out["temperature"], 0.2);
    assert_eq!(out["tools"].as_array().unwrap().len(), 2);
    let again = with_hosted_web_search(&m, &cli(), &cfg(Some(true)), Some(out.clone())).unwrap();
    assert_eq!(again, out);
}

#[test]
fn hosted_web_search_respects_off_deny_and_tool_selection() {
    let m = model("anthropic", "responses");
    let mut off = cfg(Some(true));
    off.web_search_provider = Some("off".into());
    assert_eq!(with_hosted_web_search(&m, &cli(), &off, None), None);
    let mut deny = cfg(Some(true));
    deny.permission_deny = Some([("web_search".to_string(), vec!["*".to_string()])].into());
    assert_eq!(with_hosted_web_search(&m, &cli(), &deny, None), None);
    let no_tools = Cli::parse_from(["mini-agent", "--no-tools"]);
    assert_eq!(
        with_hosted_web_search(&m, &no_tools, &cfg(Some(true)), None),
        None
    );
    let other = Cli::parse_from(["mini-agent", "--tools", "read"]);
    assert_eq!(
        with_hosted_web_search(&m, &other, &cfg(Some(true)), None),
        None
    );
}

#[test]
fn hosted_web_search_skips_providers_without_a_bridge() {
    let completions = model("openai", "completions");
    assert_eq!(
        with_hosted_web_search(&completions, &cli(), &cfg(Some(true)), None),
        None
    );
}

fn hosted_with(cfg: Config, cli: &Cli) -> Option<serde_json::Value> {
    with_hosted_web_search(&model("anthropic", "responses"), cli, &cfg, None)
}

#[test]
fn hosted_web_search_is_withheld_for_every_deny_shape() {
    let plain = cli();
    let mut string_deny = cfg(Some(true));
    string_deny.permission = Some(json!({"web_search": "deny"}));
    assert_eq!(hosted_with(string_deny, &plain), None);

    let mut granular = cfg(Some(true));
    granular.permission = Some(json!({"web_search": {"*": "deny"}}));
    assert_eq!(hosted_with(granular, &plain), None);

    // A partial (pattern) deny cannot be enforced provider-side: closed.
    let mut partial = cfg(Some(true));
    partial.permission = Some(json!({"web_search": {"*": "allow", "*.evil.test": "deny"}}));
    assert_eq!(hosted_with(partial, &plain), None);

    let mut regex = cfg(Some(true));
    regex.permission_regex = Some(json!({"web_search": {"^https://x": "deny"}}));
    assert_eq!(hosted_with(regex, &plain), None);

    let mut deny_patterns = cfg(Some(true));
    deny_patterns.permission_deny = Some(
        [(
            "web_search".to_string(),
            vec!["https://x.test/*".to_string()],
        )]
        .into(),
    );
    assert_eq!(hosted_with(deny_patterns, &plain), None);
}

#[test]
fn hosted_web_search_is_withheld_in_readonly_and_planwrite_modes() {
    let ro = Cli::parse_from(["mini-agent", "--read-only"]);
    assert_eq!(hosted_with(cfg(Some(true)), &ro), None);
    for mode in ["readonly", "planwrite"] {
        let mut c = cfg(Some(true));
        c.default_permission_mode = Some(mode.into());
        assert_eq!(hosted_with(c, &cli()), None, "{mode}");
    }
    // An explicit allow rule or a more permissive flag wins over the mode default.
    let mut allow = cfg(Some(true));
    allow.default_permission_mode = Some("readonly".into());
    allow.permission = Some(json!({"web_search": "allow"}));
    assert!(declares_hosted_web_search(
        hosted_with(allow, &cli()).as_ref()
    ));
}

#[test]
fn hosted_web_search_stays_available_under_allow_ask_and_standard_modes() {
    let plain = cli();
    for rule in [json!("allow"), json!("ask"), json!({"*": "ask"})] {
        let mut c = cfg(Some(true));
        c.permission = Some(json!({"web_search": rule}));
        assert!(declares_hosted_web_search(hosted_with(c, &plain).as_ref()));
    }
    let mut guarded = cfg(Some(true));
    guarded.default_permission_mode = Some("guarded".into());
    assert!(declares_hosted_web_search(
        hosted_with(guarded, &plain).as_ref()
    ));
}

fn tool_for(provider: &str, cfg: Config) -> Option<serde_json::Value> {
    with_hosted_web_search(&model(provider, "responses"), &cli(), &cfg, None)
        .map(|body| body["tools"][0].clone())
}

#[test]
fn hosted_web_search_defaults_are_unchanged() {
    assert_eq!(
        tool_for("anthropic", cfg(Some(true))).unwrap(),
        json!({"type":"web_search_20250305","name":"web_search","max_uses":5})
    );
    assert_eq!(
        tool_for("openai", cfg(Some(true))).unwrap(),
        json!({"type":"web_search"})
    );
}

#[test]
fn hosted_web_search_maps_domain_filters_and_max_uses() {
    let mut allow = cfg(Some(true));
    allow.hosted_web_search_allowed_domains =
        Some(vec!["https://Docs.RS/".into(), "a.example".into()]);
    allow.hosted_web_search_max_uses = Some(2);
    assert_eq!(
        tool_for("anthropic", allow.clone()).unwrap(),
        json!({"type":"web_search_20250305","name":"web_search","max_uses":2,
               "allowed_domains":["docs.rs","a.example"]})
    );
    assert_eq!(
        tool_for("openai", allow).unwrap(),
        json!({"type":"web_search","filters":{"allowed_domains":["docs.rs","a.example"]}})
    );
    let mut block = cfg(Some(true));
    block.hosted_web_search_blocked_domains = Some(vec!["bad.example".into()]);
    assert_eq!(
        tool_for("anthropic", block).unwrap()["blocked_domains"],
        json!(["bad.example"])
    );
}

#[test]
fn hosted_web_search_fails_closed_on_unenforceable_or_invalid_filters() {
    // OpenAI has no block filter: withheld rather than unfiltered.
    let mut block = cfg(Some(true));
    block.hosted_web_search_blocked_domains = Some(vec!["bad.example".into()]);
    assert_eq!(tool_for("openai", block), None);
    let mut both = cfg(Some(true));
    both.hosted_web_search_allowed_domains = Some(vec!["a.example".into()]);
    both.hosted_web_search_blocked_domains = Some(vec!["b.example".into()]);
    assert_eq!(tool_for("anthropic", both), None);
    let mut bad = cfg(Some(true));
    bad.hosted_web_search_allowed_domains = Some(vec!["not a domain".into()]);
    assert_eq!(tool_for("anthropic", bad), None);
    for uses in [0, 51] {
        let mut c = cfg(Some(true));
        c.hosted_web_search_max_uses = Some(uses);
        assert_eq!(tool_for("anthropic", c), None);
    }
    // Filters never open the capability on their own.
    let mut closed = cfg(None);
    closed.hosted_web_search_allowed_domains = Some(vec!["a.example".into()]);
    assert_eq!(tool_for("anthropic", closed), None);
}

// mini-agent-cq3om: invalid filter configuration is a config-load error, not a
// silently withheld tool.
#[test]
fn invalid_hosted_search_config_is_rejected_at_config_load() {
    use crate::config::load::validate_extra_body;
    let mut both = cfg(Some(true));
    both.hosted_web_search_allowed_domains = Some(vec!["a.example".into()]);
    both.hosted_web_search_blocked_domains = Some(vec!["b.example".into()]);
    let error = validate_extra_body(&mut both).unwrap_err();
    assert!(error.contains("not both"), "{error}");
    let mut bad_domain = cfg(Some(true));
    bad_domain.hosted_web_search_allowed_domains = Some(vec!["not a domain".into()]);
    assert!(validate_extra_body(&mut bad_domain).is_err());
    for uses in [0, 51] {
        let mut c = cfg(Some(true));
        c.hosted_web_search_max_uses = Some(uses);
        let error = validate_extra_body(&mut c).unwrap_err();
        assert!(error.contains("hosted-web-search-max-uses"), "{error}");
    }
    // Valid values and unset keys stay accepted.
    let mut ok = cfg(Some(true));
    ok.hosted_web_search_allowed_domains = Some(vec!["docs.rs".into()]);
    ok.hosted_web_search_max_uses = Some(50);
    assert!(validate_extra_body(&mut ok).unwrap().is_empty());
    assert!(validate_extra_body(&mut cfg(None)).unwrap().is_empty());
}

#[test]
fn openai_responses_block_list_is_a_visible_startup_warning() {
    use crate::config::load::validate_extra_body;
    let mut c: Config = serde_json::from_value(json!({
        "provider": "hosted",
        "hosted_web_search": true,
        "hosted_web_search_blocked_domains": ["bad.example"],
        "custom_providers": {"hosted": {"provider_type":"openai","base_url":"http://127.0.0.1:9","api_style":"responses"}}
    }))
    .unwrap();
    let warnings = validate_extra_body(&mut c).unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("hosted-web-search-blocked-domains")),
        "{warnings:?}"
    );
    // The same block-list on Anthropic is supported: no notice.
    let mut anthropic: Config = serde_json::from_value(json!({
        "provider": "hosted",
        "hosted_web_search": true,
        "hosted_web_search_blocked_domains": ["bad.example"],
        "custom_providers": {"hosted": {"provider_type":"anthropic","base_url":"http://127.0.0.1:9"}}
    }))
    .unwrap();
    assert!(validate_extra_body(&mut anthropic).unwrap().is_empty());
}
