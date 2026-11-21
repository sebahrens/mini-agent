use super::test_support::fixture_app;
use super::*;

#[tokio::test]
async fn quick_model_price_presence_actual_app_usage_charges_paid_alias() {
    let root = crate::tests::OwnedTestDirectory::new("quick-model-price-presence-app");
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "MINI_AGENT_CONFIG_DIR",
            Some(root.path().as_os_str().to_owned()),
        ),
        (
            "MINI_AGENT_DATA_DIR",
            Some(root.path().as_os_str().to_owned()),
        ),
        (
            "MINI_AGENT_STATE_DIR",
            Some(root.path().as_os_str().to_owned()),
        ),
    ]);
    let cli = crate::cli::Cli::default();
    let cfg: crate::config::Config =
        toml::from_str("[quick_models.paid]\nprovider='anthropic'\nmodel='claude-sonnet-5-5'")
            .unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("anthropic", "claude-fable-5-1", 1_000_000, "");
    session.set_input_output_prices(cfg.quick_models.as_ref().unwrap()["paid"].resolved_prices());
    let ui = UiContext {
        native_startup: None,
        native_ticket: None,
        #[cfg(feature = "hooks")]
        pending_session_start_context: None,
        cli: &cli,
        cfg: &cfg,
        session: &mut session,
        context: &mut context,
        workspace,
        client: crate::provider::create_client(
            "anthropic",
            Some("unused-test-key"),
            cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap(),
        permission: None,
        ask_tx: None,
        sandbox: crate::sandbox::Sandbox::new(false, "none"),
        prebuild_invalidated: Arc::new(AtomicBool::new(false)),
        status_signals: None,
        #[cfg(feature = "skills")]
        skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
        #[cfg(feature = "mcp")]
        mcp_manager: None,
    };
    let mut app = fixture_app(ui);
    let expected =
        crate::config::Config::catalog_input_output_cost("anthropic", "claude-sonnet-5-5").unwrap();
    app.handle_agent_event(AgentEvent::UsageDelta {
        usage: crate::event::UsageDelta {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Default::default()
        },
        context_complete: false,
        awaits_compaction_decision: false,
    })
    .await
    .unwrap();
    assert_eq!(app.ui.session.total_cost, expected.0 + expected.1);
    assert_eq!(app.ui.session.known_input_output_prices(), Some(expected));
}

#[test]
fn actual_btw_event_handler_charges_snapshot_and_ignores_duplicate_or_cancelled_ids() {
    let root = crate::tests::OwnedTestDirectory::new("btw-pricing-handler");
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "MINI_AGENT_CONFIG_DIR",
            Some(root.path().as_os_str().to_owned()),
        ),
        (
            "MINI_AGENT_DATA_DIR",
            Some(root.path().as_os_str().to_owned()),
        ),
        (
            "MINI_AGENT_STATE_DIR",
            Some(root.path().as_os_str().to_owned()),
        ),
    ]);
    let cli = crate::cli::Cli::default();
    let cfg = crate::config::Config::default();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("anthropic", "claude-fable-5-1", 1_000_000, "");
    session.input_token_cost = 10.0;
    session.output_token_cost = 50.0;
    let ui = UiContext {
        native_startup: None,
        native_ticket: None,
        #[cfg(feature = "hooks")]
        pending_session_start_context: None,
        cli: &cli,
        cfg: &cfg,
        session: &mut session,
        context: &mut context,
        workspace,
        client: crate::provider::create_client(
            "anthropic",
            Some("unused-test-key"),
            cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap(),
        permission: None,
        ask_tx: None,
        sandbox: crate::sandbox::Sandbox::new(false, "none"),
        prebuild_invalidated: Arc::new(AtomicBool::new(false)),
        status_signals: None,
        #[cfg(feature = "skills")]
        skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
        #[cfg(feature = "mcp")]
        mcp_manager: None,
    };
    let mut app = fixture_app(ui);
    let id = app
        .btw_pricing
        .start(crate::print::HeadlessPricing::from_session(
            app.ui.session,
            app.ui.cfg,
        ))
        .unwrap();
    app.btw_inflight = 1;
    app.ui.session.model = "claude-opus-5-5".into();
    app.ui.session.input_token_cost = 4.0;
    app.ui.session.output_token_cost = 20.0;
    let event = BtwEvent::Done {
        id,
        response: "answer".into(),
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 1_000_000,
        cache_creation_input_tokens: 0,
    };
    app.handle_btw_event(event.clone()).unwrap();
    assert_eq!(app.btw_total_cost, 0.25);
    assert_eq!(app.btw_inflight, 0);
    assert!(app.btw_pricing.pending.is_empty());
    app.handle_btw_event(event).unwrap();
    assert_eq!(
        app.btw_total_cost, 0.25,
        "duplicate Done cannot charge again"
    );
    let cancelled = app
        .btw_pricing
        .start(crate::print::HeadlessPricing::from_session(
            app.ui.session,
            app.ui.cfg,
        ))
        .unwrap();
    app.btw_pricing.cancel_all();
    let active = app
        .btw_pricing
        .start(crate::print::HeadlessPricing::from_session(
            app.ui.session,
            app.ui.cfg,
        ))
        .unwrap();
    app.btw_inflight = 1;
    app.handle_btw_event(BtwEvent::Done {
        id: cancelled,
        response: "stale".into(),
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 1_000_000,
        cache_creation_input_tokens: 0,
    })
    .unwrap();
    assert_eq!(app.btw_total_cost, 0.25);
    assert_eq!(
        app.btw_inflight, 1,
        "stale result cannot settle the active request"
    );
    app.handle_btw_event(BtwEvent::Error {
        id: active,
        message: "fixture failure".into(),
    })
    .unwrap();
    assert_eq!(app.btw_inflight, 0);
    assert!(app.btw_pricing.pending.is_empty());
    app.handle_btw_event(BtwEvent::Done {
        id: active,
        response: "stale after error".into(),
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 1_000_000,
        cache_creation_input_tokens: 0,
    })
    .unwrap();
    assert_eq!(app.btw_total_cost, 0.25);
    app.ui.session.input_token_cost = 0.0;
    app.ui.session.output_token_cost = 0.0;
    app.ui
        .session
        .set_cache_prices(crate::pricing::CachePrices {
            read: Some(1.0),
            write: None,
            write_1h: None,
            split_required: false,
        });
    let cache_only = app
        .btw_pricing
        .start(crate::print::HeadlessPricing::from_session(
            app.ui.session,
            app.ui.cfg,
        ))
        .unwrap();
    app.btw_inflight = 1;
    app.ui.session.input_token_cost = 4.0;
    app.ui
        .session
        .set_cache_prices(crate::pricing::CachePrices {
            read: Some(0.2),
            write: Some(5.0),
            write_1h: None,
            split_required: false,
        });
    app.handle_btw_event(BtwEvent::Done {
        id: cache_only,
        response: "cache-only answer".into(),
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 1_000_000,
        cache_creation_input_tokens: 0,
    })
    .unwrap();
    assert_eq!(
        app.btw_total_cost, 1.25,
        "zero-base absolute cache pricing belongs to the request snapshot"
    );
    assert_eq!(app.btw_inflight, 0);
    assert_eq!(
        app.ui.session.total_cost, 0.0,
        "side questions stay outside main accounting"
    );
}

#[test]
fn btw_pricing_snapshot_survives_model_switch_and_settles_each_request_once() {
    let cfg = crate::config::Config::default();
    let mut active = Session::new("anthropic", "claude-fable-5-1", 1_000_000, "");
    active.input_token_cost = 10.0;
    active.output_token_cost = 50.0;
    let mut registry = BtwPricingSnapshots::default();
    let id = registry
        .start(crate::print::HeadlessPricing::from_session(&active, &cfg))
        .unwrap();
    active.model = "claude-opus-5-5".into();
    active.input_token_cost = 4.0;
    active.output_token_cost = 20.0;
    let usage = rig::completion::Usage {
        cached_input_tokens: 1_000_000,
        ..Default::default()
    };
    assert_eq!(
        crate::print::HeadlessPricing::from_session(&active, &cfg).cost(usage),
        0.2
    );
    assert_eq!(registry.take(id).unwrap().cost(usage), 0.25);
    assert!(
        registry.take(id).is_none(),
        "duplicate completion cannot charge twice"
    );

    let retired = registry
        .start(crate::print::HeadlessPricing::from_session(&active, &cfg))
        .unwrap();
    registry.cancel_all();
    assert!(registry.pending.is_empty());
    let current = registry
        .start(crate::print::HeadlessPricing::from_session(&active, &cfg))
        .unwrap();
    assert_ne!(current, retired);
    assert!(
        registry.take(retired).is_none(),
        "stale cancelled/retired result cannot charge a current request"
    );
    assert_eq!(registry.take(current).unwrap().cost(usage), 0.2);
}

#[test]
fn btw_pricing_retention_is_bounded_and_ids_never_wrap() {
    let session = Session::new("anthropic", "unknown", 1_000, "");
    let pricing =
        crate::print::HeadlessPricing::from_session(&session, &crate::config::Config::default());
    let mut registry = BtwPricingSnapshots::default();
    let ids: Vec<_> = (0..BTW_MAX_INFLIGHT)
        .map(|_| registry.start(pricing).unwrap())
        .collect();
    assert!(registry.start(pricing).is_none());
    assert_eq!(registry.pending.len(), BTW_MAX_INFLIGHT);
    assert!(registry.take(ids[0]).is_some());
    let next = registry.start(pricing).unwrap();
    assert!(!ids.contains(&next));
    registry.cancel_all();
    registry.next_id = u32::MAX;
    assert!(registry.start(pricing).is_none());
    assert!(registry.pending.is_empty());
}
