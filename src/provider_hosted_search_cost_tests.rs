//! Provider-hosted web search is billed per request on top of tokens; the
//! count must reach the session total, the spend ledger and the budget gate.

use super::*;
use rig::completion::GetTokenUsage;
use rig::streaming::StreamedAssistantContent;

fn anthropic_reply(searches: u64) -> String {
    start()
        + &text(0, "searched answer")
        + &event(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},
                   "usage":{"output_tokens":2,"server_tool_use":{"web_search_requests":searches}}}),
        )
        + &event("message_stop", json!({"type":"message_stop"}))
}

async fn final_usage<M>(model: M) -> rig::completion::Usage
where
    M: rig::completion::CompletionModel + 'static,
{
    let mut stream = model
        .completion_request("search for something")
        .max_tokens(100)
        .stream()
        .await
        .unwrap();
    let mut usage = None;
    while let Some(item) = stream.next().await {
        if let StreamedAssistantContent::Final(response) = item.unwrap() {
            usage = Some(response.token_usage());
        }
    }
    usage.expect("the stream ends with a final response")
}

// mini-agent-zoea5: Anthropic `usage.server_tool_use.web_search_requests`.
#[tokio::test]
async fn anthropic_stream_reports_hosted_search_requests() {
    let wire = Wire::new(vec![(true, anthropic_reply(3))]);
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    let usage = final_usage(model).await;
    assert_eq!(usage.hosted_search_requests, 3);
    assert_eq!((usage.input_tokens, usage.output_tokens), (10, 2));
    // A response with no hosted tool use reports none.
    let wire = Wire::new(vec![(true, start() + &text(0, "plain") + &finish())]);
    let AnyModel::Anthropic(model) = wire.client().completion_model("claude-opus-5-5") else {
        unreachable!()
    };
    assert_eq!(final_usage(model).await.hosted_search_requests, 0);
}

// mini-agent-zoea5: OpenAI Responses `web_search_call` output items.
#[tokio::test]
async fn openai_responses_stream_reports_hosted_search_requests() {
    let item = |id: &str, kind: &str| {
        event(
            "response.output_item.done",
            json!({"type":"response.output_item.done","sequence_number":1,"output_index":0,
                   "item":{"type":kind,"id":id,"status":"completed"}}),
        )
    };
    let body = item("ws_1", "web_search_call")
        + &item("ws_2", "web_search_call")
        + &item("fs_1", "file_search_call")
        + &event(
            "response.completed",
            json!({"type":"response.completed","sequence_number":9,"response":{
                "id":"resp_search","object":"response","created_at":0,"status":"completed",
                "model":"gpt-test","output":[],"tools":[],
                "usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}}),
        );
    let wire = Wire::at_path(vec![(true, body)], "/responses");
    let config: Config = serde_json::from_value(json!({
        "custom_providers":{"responses":{"provider_type":"openai","base_url":wire.base,"api_style":"responses"}}
    }))
    .unwrap();
    let client = create_client(
        "responses",
        Some("fake-key"),
        config.custom_providers_map(),
        None,
    )
    .unwrap();
    let AnyModel::OpenAI(OpenAiModel::Responses(model)) = client.completion_model("gpt-test")
    else {
        panic!("expected an OpenAI Responses model");
    };
    let usage = final_usage(model).await;
    assert_eq!(
        usage.hosted_search_requests, 2,
        "only web_search_call counts"
    );
    assert_eq!((usage.input_tokens, usage.output_tokens), (10, 2));
}

/// One headless `-p` run per prompt on one saved session with a $0.03 budget.
async fn headless_budget_runs(
    searches: u64,
    prompts: &[&str],
) -> (Vec<Value>, Vec<bool>, crate::session::Session, f64) {
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = crate::tests::OwnedTestDirectory::new("hosted-search-cost");
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "MINI_AGENT_CONFIG_DIR",
            Some(roots.path().join("config").into_os_string()),
        ),
        (
            "MINI_AGENT_DATA_DIR",
            Some(roots.path().join("data").into_os_string()),
        ),
        (
            "MINI_AGENT_STATE_DIR",
            Some(roots.path().join("state").into_os_string()),
        ),
        (
            "MINI_AGENT_CACHE_DIR",
            Some(roots.path().join("cache").into_os_string()),
        ),
    ]);
    #[cfg(feature = "hooks")]
    let _workspace_selection = crate::extras::hooks::retain_test_workspace_selection();
    let wire = Wire::new(
        prompts
            .iter()
            .map(|_| (true, anthropic_reply(searches)))
            .collect(),
    );
    let cfg: crate::config::Config = serde_json::from_value(json!({
        "session_title_model":"off",
        "custom_providers":{"search-cost":{"provider_type":"anthropic","base_url":wire.base}},
        "quick_models":{"priced":{"provider":"search-cost","model":"claude-opus-5-5",
            "context_window":200000,"input_token_cost":2.0,"output_token_cost":4.0}}
    }))
    .unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(roots.path()).unwrap());
    let paths = crate::paths::process_paths_ref().unwrap();
    let mut id: Option<String> = None;
    let mut outcomes = Vec::new();
    for prompt in prompts {
        let cli = crate::cli::Cli {
            api_key: Some("unused-key".into()),
            quick_model: id.is_none().then(|| "priced".to_owned()),
            session: id.clone(),
            print: true,
            quiet: true,
            message: vec![(*prompt).into()],
            max_budget_usd: Some(0.03),
            no_context_files: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let mut startup = crate::startup::heap_future(|| async {
            Box::new(
                crate::startup::Startup::init(
                    cli,
                    cfg.clone(),
                    paths.clone(),
                    workspace.clone(),
                    false,
                    false,
                    false,
                )
                .await
                .unwrap(),
            )
        })
        .await;
        let sid = startup.session.id.to_string();
        startup.init_features().await.unwrap();
        startup.resolve_prompts().await.unwrap();
        outcomes.push(
            crate::startup::heap_future(|| startup.dispatch())
                .await
                .is_ok(),
        );
        crate::session::lock::release_session(&sid);
        id = Some(sid);
    }
    let sid = id.unwrap();
    let saved = crate::session::storage::load_session_exact(&sid)
        .unwrap()
        .unwrap();
    let ledger_cost = crate::spend::Ledger::new(&paths.state_dir)
        .summary_for_session(&sid)
        .unwrap()
        .session
        .known_cost;
    (wire.finish(), outcomes, saved, ledger_cost)
}

// mini-agent-zoea5: 3 hosted searches add 3 x $10/1,000 to the session cost,
// the ledger receipt and the --max-budget-usd gate (token cost alone is far
// below the $0.03 cap, so only the search fee can trip it).
#[tokio::test]
async fn hosted_searches_are_charged_to_session_cost_ledger_and_budget_gate() {
    let (bodies, outcomes, saved, ledger_cost) =
        headless_budget_runs(3, &["first", "second"]).await;
    let token_cost = 0.000028;
    let fee = 3.0 * 10.0 / 1000.0;
    assert_eq!(saved.total_hosted_search_requests, 3);
    assert!(
        (saved.total_cost - (token_cost + fee)).abs() < 1e-9,
        "{}",
        saved.total_cost
    );
    assert!(
        (ledger_cost - (token_cost + fee)).abs() < 1e-9,
        "{ledger_cost}"
    );
    assert_eq!(
        outcomes,
        vec![true, false],
        "the first run is admitted; the search fee alone exhausts the cap before the second"
    );
    assert_eq!(
        bodies.len(),
        1,
        "the refused run never reaches the provider"
    );
}

// Control: without hosted searches the same two runs both fit the budget.
#[tokio::test]
async fn runs_without_hosted_searches_stay_within_the_same_budget() {
    let (bodies, outcomes, saved, ledger_cost) =
        headless_budget_runs(0, &["first", "second"]).await;
    assert_eq!(outcomes, vec![true, true]);
    assert_eq!(bodies.len(), 2);
    assert_eq!(saved.total_hosted_search_requests, 0);
    assert!((saved.total_cost - 2.0 * 0.000028).abs() < 1e-9);
    assert!((ledger_cost - saved.total_cost).abs() < 1e-9);
}
