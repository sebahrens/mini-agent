use super::*;
#[test]
fn config68_actual_app_busy_listing_save_and_live_display_are_pure_on_one_mib() {
    std::thread::Builder::new().name("config68-app".into()).stack_size(1024*1024).spawn(||{
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(crate::startup::heap_future(||async {
            let root=crate::tests::OwnedTestDirectory::new("config68-app");
            let environment=["MINI_AGENT_CONFIG_DIR","MINI_AGENT_DATA_DIR","MINI_AGENT_LOCAL_DATA_DIR","MINI_AGENT_STATE_DIR","MINI_AGENT_CACHE_DIR","MINI_AGENT_CREDENTIALS_DIR"].map(|name|(name,Some(root.path().join(name).into_os_string())));
            let _environment=crate::tests::ScopedProcessEnv::set(&environment);
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
            let mut cfg:crate::config::Config=serde_json::from_value(serde_json::json!({"max_tokens":51,"custom_providers":{"editor68":{"provider_type":"openai","base_url":format!("http://{address}/v1"),"api_style":"completions"}}})).unwrap();
            let paths=crate::paths::process_paths_ref().unwrap();let global=paths.config_dir.join("config.toml");
            crate::fs::private_atomic_write_sync(&global,b"model='disk-model'\nmax_tokens=51\n").unwrap();cfg.provenance.global_file=Some(global.clone());
            let cli=crate::cli::Cli {no_session:true,no_context_files:true,max_tokens:Some(999),..Default::default()};
            let workspace=Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let mut context=crate::context::load(true).for_workspace_binding(true,&workspace);
            let mut session=Session::new("editor68","test-model",128_000,root.path().to_str().unwrap());
            let before=serde_json::to_vec(&session).unwrap();
            let ui=UiContext {cli:&cli,cfg:&cfg,session:&mut session,native_startup:None,native_ticket: None, #[cfg(feature = "hooks")] pending_session_start_context: None,context:&mut context,workspace,client:crate::provider::create_client("editor68",Some("unused"),cfg.custom_providers_map(),None).unwrap(),permission:None,ask_tx:None,sandbox:crate::sandbox::Sandbox::new(false,"none"),prebuild_invalidated:Arc::new(AtomicBool::new(false)),status_signals:None,
                #[cfg(feature="skills")] skill_services:Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
                #[cfg(feature="mcp")] mcp_manager:None,
            };
            let mut app=super::test_support::fixture_app(ui);app.run.is_running=true;
            app.run_slash_command("/config").await.unwrap();assert!(matches!(app.input.picker,Some(crate::ui::input::Picker::Config(_))));assert!(app.run.agent.is_none());
            app.run_slash_command("/config max_tokens 99").await.unwrap();assert!(std::fs::read_to_string(&global).unwrap().contains("51"));
            app.run.is_running=false;app.run_slash_command("/config max_tokens 99").await.unwrap();
            let saved=std::fs::read_to_string(&global).unwrap();assert!(saved.contains("99"));assert!(saved.contains("disk-model"));assert!(!saved.contains("999"));assert_eq!(app.ui.cfg.max_tokens,Some(51));
            app.run_slash_command("/config reasoning_display \"full\"").await.unwrap();assert_eq!(app.renderer.feed().reasoning_display(),crate::config::ReasoningDisplay::Full);
            app.run_slash_command("/config api_keys malformed-secret-value").await.unwrap();assert!(!app.renderer.captured_output().contains("malformed-secret-value"));
            assert_eq!(before,serde_json::to_vec(app.ui.session).unwrap());assert!(app.run.agent.is_none());assert!(tokio::time::timeout(Duration::from_millis(20),listener.accept()).await.is_err());
        }));
    }).unwrap().join().unwrap();
}
