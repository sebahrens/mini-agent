// App fixture tests for the `/cron` fire path (mini-agent-5eoy7.7). They
// drive the real `App` against the owned local provider of
// `plan_approval_tests`, calling the same `fire_due_cron` the event loop's
// cron arm awaits.
use super::*;
use std::time::Duration;
use tokio::time::Instant;

fn app_for<'a>(
    cli: &'a crate::cli::Cli,
    cfg: &'a config::Config,
    session: &'a mut Session,
    context: &'a mut crate::context::ContextFiles,
    roots: &Roots,
) -> OwnedApp<'a> {
    owned_app(ui(
        cli,
        cfg,
        session,
        context,
        roots.workspace(),
        SecurityMode::Standard,
    ))
}

fn cli() -> crate::cli::Cli {
    crate::cli::Cli {
        no_session: true,
        no_tools: true,
        ..Default::default()
    }
}

fn session(roots: &Roots) -> Session {
    Session::new(
        "plan80",
        "owned-model",
        128_000,
        roots.workspace().root().to_str().unwrap(),
    )
}

/// Arm `payload` every minute with its deadline `missed` intervals ago (at
/// least the current instant), so the entry is already expired.
fn arm_expired(app: &mut App<'_>, payload: &str, missed: u32) -> String {
    let interval = Duration::from_secs(60);
    let armed_at = Instant::now()
        .checked_sub(interval * missed)
        .expect("monotonic clock far enough from its origin");
    app.cron
        .arm(interval, payload.to_owned(), armed_at)
        .unwrap()
        .id
        .clone()
}

async fn fire(app: &mut App<'_>) {
    crate::startup::heap_future(|| app.fire_due_cron())
        .await
        .unwrap();
}

/// Handle agent events until one terminal event; unlike `finish_turn` this
/// tolerates a follow-up run (a dequeued input) started by the settle.
async fn settle(app: &mut App<'_>) -> bool {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
            let success = matches!(event, AgentEvent::Done { .. });
            let terminal = success
                || matches!(
                    event,
                    AgentEvent::Error { .. } | AgentEvent::AwaitingUser { .. }
                );
            crate::startup::heap_future(|| app.handle_agent_event(event))
                .await
                .unwrap();
            if terminal {
                return success;
            }
        }
    })
    .await
    .unwrap()
}

fn user_rows(app: &App<'_>) -> Vec<String> {
    app.ui
        .session
        .messages
        .iter()
        .filter(|row| row.role == MessageRole::User)
        .map(|row| row.content.to_string())
        .collect()
}

#[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
struct HookReset;
#[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
impl Drop for HookReset {
    fn drop(&mut self) {
        crate::extras::hooks::reset_dispatcher();
    }
}

#[tokio::test]
async fn cron_idle_entry_fires_at_its_deadline_as_a_scheduled_user_turn() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    #[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
    let (_workspace, _reset, hook_log) = {
        let workspace = crate::extras::hooks::retain_test_workspace_selection();
        let hook_log = roots.directory.path().join("prompt-hook.json");
        let settings = serde_json::json!({"UserPromptSubmit":[{"hooks":[{"type":"command","command":"/bin/sh","args":["-c",r#"cat > "$1""#,"cron-hook",hook_log],"trust":"trusted","timeout":5}]}]});
        let hooks = crate::extras::hooks::settings::parse_hooks_config(&settings).unwrap();
        crate::extras::hooks::init_dispatcher(
            crate::extras::hooks::dispatcher::HookDispatcher::from_config(&hooks).unwrap(),
        );
        crate::extras::hooks::set_active_workspace(roots.directory.path());
        (workspace, HookReset, hook_log)
    };
    let provider = Provider::new(vec!["CRON_DONE"]).await;
    let cfg = provider.config();
    let cli = cli();
    let mut session = session(&roots);
    let mut context = crate::context::load(true).for_workspace_binding(true, &roots.workspace());
    let mut app = app_for(&cli, &cfg, &mut session, &mut context, &roots);

    let interval = Duration::from_secs(300);
    let now = Instant::now();
    app.cron
        .arm(interval, "check the deploy".into(), now)
        .unwrap();
    // Not yet due: the arm waits for the deadline and a fire is a no-op.
    assert_eq!(app.cron_wakeup(), Some(now + interval));
    fire(&mut app).await;
    assert!(!app.run.is_running && provider.calls().is_empty());

    app.cron.stop_all();
    arm_expired(&mut app, "check the deploy", 1);
    assert!(app.cron_wakeup().is_some_and(|at| at <= Instant::now()));
    fire(&mut app).await;
    assert!(app.run.is_running, "an idle due entry starts a turn");
    let output = app.renderer.captured_output();
    assert!(
        output.contains("⟳ cron c2 (every 1m): check the deploy"),
        "{output}"
    );
    assert!(!output.contains("> check the deploy"), "{output}");
    // The deadline moved before the turn started: no second fire while busy.
    let entry = app.cron.get("c2").unwrap();
    assert!(!entry.due && entry.fires == 1 && entry.next_at > Instant::now());
    assert!(settle(&mut app).await);
    assert_eq!(user_rows(&app), ["check the deploy"]);
    assert_eq!(provider.calls().len(), 1);
    #[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
    {
        let envelope: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&hook_log).unwrap()).unwrap();
        assert_eq!(envelope["origin"], "scheduled");
        assert_eq!(envelope["prompt"], "check the deploy");
    }
    // Nothing else is due afterwards.
    assert!(app.cron_wakeup().is_some_and(|at| at > Instant::now()));
    drop(app);
    provider.finish().await;
}

#[tokio::test]
async fn cron_deadline_during_a_run_parks_one_fire_after_queued_input() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["USER_DONE", "QUEUED_DONE", "CRON_DONE"]).await;
    let cfg = provider.config();
    let cli = cli();
    let mut session = session(&roots);
    let mut context = crate::context::load(true).for_workspace_binding(true, &roots.workspace());
    let mut app = app_for(&cli, &cfg, &mut session, &mut context, &roots);

    user(&mut app, UserEvent::Paste("typed first".into())).await;
    key(&mut app, KeyCode::Enter).await;
    assert!(app.run.is_running);
    // Two whole intervals were missed while the run is busy.
    arm_expired(&mut app, "scheduled ping", 2);
    fire(&mut app).await;
    fire(&mut app).await;
    let entry = app.cron.get("c1").unwrap();
    assert!(entry.due && entry.fires == 0);
    // A parked fire does not wake a busy loop.
    assert_eq!(app.cron_wakeup(), None);
    user(&mut app, UserEvent::Paste("queued second".into())).await;
    key(&mut app, KeyCode::Enter).await;
    assert_eq!(app.run.pending_inputs.len(), 1);

    assert!(settle(&mut app).await);
    // The settle ran the queued input first; the fire stays parked.
    assert!(app.run.is_running && app.run.pending_inputs.is_empty());
    fire(&mut app).await;
    assert_eq!(app.cron.get("c1").unwrap().fires, 0);
    assert!(settle(&mut app).await);
    assert!(!app.run.is_running);

    assert!(app.cron_wakeup().is_some_and(|at| at <= Instant::now()));
    fire(&mut app).await;
    assert!(app.run.is_running);
    assert!(settle(&mut app).await);
    assert_eq!(
        user_rows(&app),
        ["typed first", "queued second", "scheduled ping"]
    );
    let entry = app.cron.get("c1").unwrap();
    assert!(!entry.due && entry.fires == 1, "missed deadlines fire once");
    assert_eq!(provider.calls().len(), 3);
    drop(app);
    provider.finish().await;
}

#[cfg(feature = "git-worktree")]
#[tokio::test]
async fn cron_pending_git_work_defers_the_fire_without_an_error() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["CRON_DONE"]).await;
    let cfg = provider.config();
    let cli = cli();
    let mut session = session(&roots);
    let mut context = crate::context::load(true).for_workspace_binding(true, &roots.workspace());
    let mut app = app_for(&cli, &cfg, &mut session, &mut context, &roots);

    let (_observation_tx, observations) = tokio::sync::mpsc::channel(1);
    app.chain.git.work = Some(crate::ui::slash::git::Work {
        session_id: app.ui.session.id.clone(),
        workspace: app.ui.workspace.clone(),
        kind: crate::ui::slash::git::WorkKind::Diff,
        observations,
        usage: Default::default(),
        pricing: crate::print::HeadlessPricing::from_session(app.ui.session, app.ui.cfg),
        provider: app.ui.session.provider.clone(),
        pending_calls: Default::default(),
    });
    arm_expired(&mut app, "after git", 1);
    assert!(!app.cron_idle());
    // `start_main_run_prepared` would `ensure!` here and exit the TUI.
    fire(&mut app).await;
    assert!(!app.run.is_running && provider.calls().is_empty());
    assert!(app.cron.get("c1").unwrap().due);
    assert_eq!(app.cron_wakeup(), None);

    app.chain.git.work = None;
    assert!(app.cron_idle());
    fire(&mut app).await;
    assert!(app.run.is_running);
    assert!(settle(&mut app).await);
    assert_eq!(user_rows(&app), ["after git"]);
    drop(app);
    provider.finish().await;
}

#[tokio::test]
async fn cron_budget_cap_suspends_every_entry_with_one_notice_and_keeps_them() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec![]).await;
    let mut cfg = provider.config();
    // Unknown model pricing under a USD cap refuses every model request.
    cfg.max_budget_usd = Some(1.0);
    let cli = cli();
    let mut session = session(&roots);
    let mut context = crate::context::load(true).for_workspace_binding(true, &roots.workspace());
    let mut app = app_for(&cli, &cfg, &mut session, &mut context, &roots);

    arm_expired(&mut app, "first", 1);
    arm_expired(&mut app, "second", 1);
    fire(&mut app).await;
    assert!(!app.run.is_running && app.cron.is_suspended());
    assert_eq!(app.cron.len(), 2);
    assert!(app.cron.entries().iter().all(|entry| entry.fires == 0));
    assert_eq!(app.cron_wakeup(), None, "a suspended store never wakes");
    fire(&mut app).await;
    let output = app.renderer.captured_output();
    assert_eq!(
        output.matches("schedule(s) suspended and kept").count(),
        1,
        "{output}"
    );
    assert!(output.contains("cron: USD budget reached; 2 schedule(s)"));
    assert!(user_rows(&app).is_empty() && app.input.buffer.is_empty());
    drop(app);
    provider.finish().await;
}

#[tokio::test]
async fn cron_interrupt_keeps_the_schedule_escape_clears_parked_and_quit_drops() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["hold"]).await;
    let cfg = provider.config();
    let cli = cli();
    let mut session = session(&roots);
    let mut context = crate::context::load(true).for_workspace_binding(true, &roots.workspace());
    let mut app = app_for(&cli, &cfg, &mut session, &mut context, &roots);

    arm_expired(&mut app, "long scheduled job", 1);
    fire(&mut app).await;
    assert!(app.run.is_running);
    user(
        &mut app,
        UserEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    )
    .await;
    assert!(!app.run.is_running);
    let entry = app.cron.get("c1").unwrap();
    assert!(!entry.due && entry.fires == 1, "Ctrl-C keeps the schedule");
    assert!(
        app.input.buffer.is_empty(),
        "an interrupted fire is not the user's draft"
    );

    assert!(app.cron.mark_due("c1"));
    key(&mut app, KeyCode::Esc).await;
    assert!(!app.cron.has_due() && app.cron.len() == 1);
    assert!(
        app.renderer
            .captured_output()
            .contains("cron: cleared 1 parked fire(s); schedules kept")
    );
    // A second Escape with nothing parked keeps its ordinary meaning.
    key(&mut app, KeyCode::Esc).await;
    assert_eq!(app.cron.len(), 1);

    assert!(matches!(
        crate::startup::heap_future(|| app.handle_user_event(UserEvent::Quit))
            .await
            .unwrap(),
        ControlFlow::Break(())
    ));
    assert!(app.cron.is_empty(), "Quit drops every entry");
    drop(app);
    provider.cancel().await;
}

#[tokio::test]
async fn cron_reaching_max_fires_removes_the_entry_with_a_notice() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["LAST_DONE"]).await;
    let mut cfg = provider.config();
    cfg.cron_max_fires = Some(1);
    let cli = cli();
    let mut session = session(&roots);
    let mut context = crate::context::load(true).for_workspace_binding(true, &roots.workspace());
    let mut app = app_for(&cli, &cfg, &mut session, &mut context, &roots);

    arm_expired(&mut app, "only once", 1);
    fire(&mut app).await;
    assert!(app.run.is_running && app.cron.is_empty());
    assert!(
        app.renderer
            .captured_output()
            .contains("cron c1: reached cron_max_fires (1); schedule removed after this fire")
    );
    assert!(settle(&mut app).await);
    assert_eq!(user_rows(&app), ["only once"]);
    assert_eq!(app.cron_wakeup(), None);
    drop(app);
    provider.finish().await;
}

#[tokio::test]
async fn cron_teardown_drops_every_entry() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec![]).await;
    let cfg = provider.config();
    let cli = cli();
    let mut session = session(&roots);
    let mut context = crate::context::load(true).for_workspace_binding(true, &roots.workspace());
    let mut app = app_for(&cli, &cfg, &mut session, &mut context, &roots);
    arm_expired(&mut app, "never", 1);
    crate::startup::heap_future(|| app.teardown_owners()).await;
    assert!(app.cron.is_empty());
    drop(app);
    provider.finish().await;
}

#[tokio::test]
async fn cron_resumed_session_holds_saved_schedules_disarmed_behind_one_notice() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec![]).await;
    let cfg = provider.config();
    let cli = cli();
    let mut session = session(&roots);
    session.cron_schedule = crate::session::SavedCronSchedule {
        last_id: 2,
        entries: vec![crate::session::SavedCronEntry {
            id: "c2".into(),
            interval_secs: 60,
            payload: "saved job".into(),
            fires: 3,
            // Long past: a resumed schedule must never catch up.
            next_fire_at: "2020-01-01T00:00:00Z".into(),
        }],
    };
    let mut context = crate::context::load(true).for_workspace_binding(true, &roots.workspace());
    let mut app = app_for(&cli, &cfg, &mut session, &mut context, &roots);

    // The fixture loads the session's store as startup does; switching to it
    // in-process (`/resume`) adopts it the same way and shows the notice.
    app.adopt_saved_cron().unwrap();
    assert!(app.renderer.captured_output().contains(
        "this session had 1 /cron schedule (c2 every 1m: saved job); /cron resume to re-arm"
    ));
    assert_eq!(app.cron_wakeup(), None);
    fire(&mut app).await;
    assert!(!app.run.is_running, "a disarmed schedule never fires");
    assert!(user_rows(&app).is_empty());

    crate::startup::heap_future(|| app.run_slash_command("/cron resume"))
        .await
        .unwrap();
    let entry = app.cron.get("c2").expect("re-armed under its saved id");
    assert_eq!(entry.fires, 3);
    assert!(!entry.due, "no catch-up for the missed wall-clock time");
    assert!(entry.next_at > Instant::now() + Duration::from_secs(50));
    // The re-armed schedule is copied back into the session for the next save.
    assert_eq!(app.ui.session.cron_schedule.entries.len(), 1);
    assert_eq!(app.ui.session.cron_schedule.entries[0].id, "c2");

    // Quit stops the live schedule but keeps the saved copy for next time.
    assert!(matches!(
        crate::startup::heap_future(|| app.handle_user_event(UserEvent::Quit))
            .await
            .unwrap(),
        ControlFlow::Break(())
    ));
    assert!(app.cron.is_empty());
    assert_eq!(app.ui.session.cron_schedule.entries.len(), 1);
    drop(app);
    provider.finish().await;
}

#[tokio::test]
async fn cron_fire_copies_the_new_fire_count_into_the_session() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["DONE"]).await;
    let cfg = provider.config();
    let cli = cli();
    let mut session = session(&roots);
    let mut context = crate::context::load(true).for_workspace_binding(true, &roots.workspace());
    let mut app = app_for(&cli, &cfg, &mut session, &mut context, &roots);

    arm_expired(&mut app, "persisted tick", 1);
    fire(&mut app).await;
    assert!(settle(&mut app).await);
    let saved = &app.ui.session.cron_schedule;
    assert_eq!(saved.entries.len(), 1);
    assert_eq!((saved.entries[0].id.as_str(), saved.entries[0].fires), ("c1", 1));
    drop(app);
    provider.finish().await;
}
