use super::*;

pub(super) fn fixture_app(ui: UiContext<'_>) -> App<'_> {
    let mut input = InputEditor::new();
    input.keybindings = KeyBindings::from_overrides(ui.cfg.keys.as_ref()).unwrap();
    input.set_vim_enabled(ui.cfg.vim_mode.unwrap_or(false));
    let (git_status_tx, git_status_rx) = mpsc::channel(1);
    let (command_segment_tx, command_segment_rx) = mpsc::channel(1);
    let (btw_tx, btw_rx) = mpsc::channel(1);
    let (title_tx, title_rx) = mpsc::channel(1);
    let (user_tx, user_rx) = mpsc::channel(1);
    let cron = crate::ui::cron::CronStore::from_saved(&ui.session.cron_schedule);
    App {
        ui,
        run: AgentRunState::default(),
        chain: ChainState::default(),
        slash: SlashState {
            custom_commands: Default::default(),
            show_reasoning: false,
            reasoning_enabled: false,
            todo_tools_enabled: false,
        },
        cron,
        cron_turn: None,
        cron_dynamic: None,
        cron_noop: None,
        renderer: Renderer::for_app_test().unwrap(),
        input,
        loop_state: EventLoopState::default(),
        rewind_escape: None,
        git_status_tx,
        git_status_rx,
        command_segment_tx,
        command_segment_rx,
        git_status_in_flight: false,
        ask_rx: None,
        permission_prompt: None,
        question_prompt: None,
        #[cfg(feature = "advisor")]
        handoff_rx: None,
        btw_tx,
        btw_rx,
        title_tx,
        title_rx,
        title_requested_for: None,
        title_generation: 0,
        title_work: None,
        btw_abort: Vec::new(),
        btw_inflight: 0,
        btw_pricing: BtwPricingSnapshots::default(),
        btw_total_cost: 0.0,
        btw_total_in: 0,
        btw_total_out: 0,
        user_tx,
        user_rx,
        deferred_user_events: Default::default(),
        #[cfg(feature = "subagents")]
        background_requests: Default::default(),
        running: Arc::new(AtomicBool::new(false)),
        event_handle: None,
        prebuild: None,
        terminal_guard: TerminalGuard::detached_for_test(),
        #[cfg(feature = "mcp")]
        mcp_admin: None,
        model_cache_generation: 0,
        ctrl_c_exit_armed: false,
        session_start_task: None,
        skill_commands: None,
        update_notice: None,
    }
}
