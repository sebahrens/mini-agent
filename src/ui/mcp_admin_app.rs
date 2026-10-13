use super::*;
use crate::ui::mcp_admin::{self, Action, Approval, Completion, State, Work};

#[cfg(test)]
#[path = "mcp_admin_app_tests.rs"]
mod tests;

impl App<'_> {
    fn mcp_edit_busy(&self) -> bool {
        if self.chain.plan.pending.is_some() {
            return true;
        }
        #[cfg(feature = "git-worktree")]
        if self.chain.git.pending.is_some() || self.chain.git.work.is_some() {
            return true;
        }
        #[cfg(feature = "goal")]
        if self
            .ui
            .session
            .goal_store
            .snapshot()
            .is_some_and(|goal| !goal.status.is_terminal())
        {
            return true;
        }
        self.run.is_running
            || self.run.agent_rx.is_some()
            || self.run.main_abort.is_some()
            || self.run.pending_turn.is_some()
            || self.run.validation_active()
            || self.permission_prompt.is_some()
            || self.question_prompt.is_some()
            || self.renderer.chain_prompt.is_some()
    }

    pub(super) async fn retire_mcp_work(&mut self) -> anyhow::Result<()> {
        if let Some(work) = self
            .mcp_admin
            .as_mut()
            .and_then(|state| state.work.as_mut())
        {
            work.retire().await?;
        }
        if let Some(state) = &mut self.mcp_admin {
            state.work = None;
        }
        Ok(())
    }

    fn mcp_state(&mut self) -> &mut State {
        self.mcp_admin
            .get_or_insert_with(|| Box::new(State::default()))
    }

    fn mcp_current_config(&self) -> &config::Config {
        self.mcp_admin
            .as_ref()
            .and_then(|state| state.live.as_deref())
            .unwrap_or(self.ui.cfg)
    }

    fn mcp_global_path(&self) -> anyhow::Result<std::path::PathBuf> {
        self.ui.cfg.provenance.global_file.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "MCP global config source was not captured; no edit or refresh performed"
            )
        })
    }

    pub(super) fn mcp_server_config(
        &self,
        name: &str,
    ) -> Option<crate::extras::mcp::config::McpServerConfig> {
        self.mcp_current_config()
            .mcp_servers
            .as_ref()
            .and_then(|servers| servers.get(name))
            .cloned()
    }

    async fn mcp_invalidate_agent(&mut self) -> anyhow::Result<()> {
        self.ui.invalidate_prebuild();
        if let Some(prebuild) = self.prebuild.take() {
            prebuild.retire(Duration::from_secs(5)).await?;
        }
        self.run.agent = None;
        Ok(())
    }

    fn mcp_status(&mut self) -> anyhow::Result<()> {
        let cfg = self.mcp_current_config();
        let health = self
            .ui
            .mcp_manager
            .as_ref()
            .map(|manager| manager.health_snapshot())
            .unwrap_or_default();
        let mut names = std::collections::BTreeSet::new();
        let mut omitted = health.omitted;
        for name in cfg
            .mcp_origins
            .keys()
            .chain(cfg.mcp_servers.iter().flat_map(|servers| servers.keys()))
            .chain(health.servers.keys())
        {
            if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
                omitted = true;
                continue;
            }
            names.insert(name.clone());
            if names.len() > 128 {
                names.pop_last();
                omitted = true;
            }
        }
        let mut rows = Vec::new();
        for name in names {
            let connected = self
                .ui
                .mcp_manager
                .as_ref()
                .is_some_and(|mgr| mgr.handles.iter().any(|handle| handle.server_name == name));
            if let Some(origins) = cfg.mcp_origins.get(&name) {
                for origin in origins {
                    let state = if let Some(reason) = &origin.inactive {
                        format!("inactive: {reason}")
                    } else if !origin.selected {
                        "shadowed".into()
                    } else if connected {
                        "connected".into()
                    } else {
                        "not connected".into()
                    };
                    let digest = origin
                        .digest
                        .as_deref()
                        .map(|digest| format!(", SHA256 {}", &digest[..digest.len().min(12)]))
                        .unwrap_or_default();
                    rows.push(format!(
                        "  {name}: {state}; origin {}{digest}",
                        origin.source
                    ));
                }
            } else {
                rows.push(format!(
                    "  {name}: {}; origin loaded config",
                    if connected {
                        "connected"
                    } else {
                        "not connected"
                    }
                ));
            }
            if let Some(observed) = health.servers.get(&name) {
                for (kind, attempt) in [
                    ("connection", observed.connection),
                    ("catalogue", observed.catalogue),
                ] {
                    if let Some(attempt) = attempt {
                        rows.push(format!(
                            "    last {kind} attempt #{}: {:?}",
                            attempt.id, attempt.outcome
                        ));
                    }
                }
            }
        }
        if omitted {
            rows.push(
                "  additional definitions or attempt metadata omitted by the bounded catalogue"
                    .into(),
            );
        }
        if rows.is_empty() {
            self.renderer
                .write_line("no MCP servers configured", C_AGENT)?;
        } else {
            self.renderer.write_line("MCP servers:", C_AGENT)?;
            for row in rows {
                self.renderer.write_line(&sanitize_output(&row), C_AGENT)?;
            }
        }
        Ok(())
    }

    fn spawn_mcp_work(
        &mut self,
        name: String,
        server: Option<crate::extras::mcp::config::McpServerConfig>,
        origin: Option<crate::extras::mcp::project::ServerOrigin>,
        operation: impl Future<Output = Result<Completion, String>> + Send + 'static,
    ) -> anyhow::Result<()> {
        let state = self.mcp_state();
        anyhow::ensure!(state.work.is_none(), "MCP work is already active");
        let id = state
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("MCP work generation exhausted"))?;
        state.generation = id;
        let workspace = self.ui.workspace.clone();
        let session_id = self.ui.session.id.clone();
        let scope = crate::agent::runner::AgentWorkScope::new();
        let task_scope = scope.clone();
        let (sender, receiver) = mpsc::channel(1);
        let notify = self.user_tx.clone();
        let task = tokio::spawn(async move {
            task_scope.run(async {
                let result=operation.await;
                if task_scope.is_cancelled() {
                    if let Ok(completion)=result {completion.retire().await?;}
                } else if let Err(error)=sender.send(result).await {
                    if let Ok(completion)=error.0 {completion.retire().await?;}
                } else {
                    tokio::select! {biased; _=crate::agent::runner::current_work_scope_cancelled()=>{}, _=notify.send(UserEvent::McpAdminDone{id})=>{}}
                }
                Ok::<(), anyhow::Error>(())
            }).await
        });
        self.mcp_state().work = Some(Work {
            id,
            session_id,
            workspace,
            name,
            server,
            origin,
            scope,
            task: Some(task),
            receiver,
            attempt: None,
            retirement_failed: false,
        });
        Ok(())
    }

    async fn start_mcp_reconnect(&mut self, name: String) -> anyhow::Result<()> {
        let server = self
            .mcp_server_config(&name)
            .ok_or_else(|| anyhow::anyhow!("MCP server is inactive or absent"))?;
        let origin = self
            .mcp_current_config()
            .mcp_origins
            .get(&name)
            .and_then(|origins| origins.iter().find(|origin| origin.selected))
            .cloned();
        let workspace = self.ui.workspace.clone();
        let task_name = name.clone();
        let task_server = server.clone();
        let task_origin = origin.clone();
        let proof = Work {
            id: 0,
            session_id: self.ui.session.id.clone(),
            workspace: workspace.clone(),
            name: name.clone(),
            server: Some(server.clone()),
            origin: origin.clone(),
            scope: crate::agent::runner::AgentWorkScope::new(),
            task: None,
            receiver: mpsc::channel(1).1,
            attempt: None,
            retirement_failed: false,
        };
        anyhow::ensure!(
            proof.valid(&self.ui.session.id, &self.ui.workspace),
            "MCP source or workspace changed; no reconnect started"
        );
        let attempt = self
            .ui
            .mcp_manager
            .get_or_insert_with(|| McpClientManager::from_handles(Vec::new()))
            .begin_connection_attempt(&name);
        self.spawn_mcp_work(name, Some(server), origin, async move {
            workspace
                .validate()
                .map_err(|_| "MCP workspace changed before connection")?;
            if let Some(origin) = task_origin
                && let Some(digest) = origin.digest
            {
                let file = crate::extras::mcp::project::FileSnapshot::read(
                    &workspace,
                    Path::new(&origin.source),
                )
                .map_err(|_| "MCP source is unavailable")?;
                if crate::extras::mcp::project::digest(&file.content) != digest {
                    return Err("MCP source changed before connection".into());
                }
            }
            let handle = crate::extras::mcp::client::McpClientHandle::connect_in_binding_guarded(
                task_name.clone().into(),
                &task_server,
                &workspace,
                &|| proof.valid(&proof.session_id, &proof.workspace),
            )
            .await
            .map_err(|_| "MCP connection failed (credential values omitted)".to_owned())?
            .with_source_proof(
                crate::extras::mcp::project::ServerSourceProof::capture(
                    workspace,
                    task_name,
                    Some(task_server),
                    proof.origin.clone(),
                    false,
                ),
            );
            Ok(Completion::Connected(handle))
        })?;
        self.mcp_state().work.as_mut().unwrap().attempt = Some(attempt);
        self.renderer
            .write_line("MCP reconnect started; Ctrl-C cancels it", C_AGENT)?;
        Ok(())
    }

    pub(super) async fn start_mcp_login(&mut self, name: String) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.mcp_edit_busy(),
            "Finish or stop active work before MCP login"
        );
        anyhow::ensure!(
            !self
                .mcp_admin
                .as_ref()
                .is_some_and(|state| state.work.is_some() || state.approval.is_some()),
            "MCP work is already active"
        );
        let server = self
            .mcp_server_config(&name)
            .ok_or_else(|| anyhow::anyhow!("MCP server is inactive or absent"))?;
        let crate::extras::mcp::config::McpServerConfig::Url {
            url,
            allow_local,
            oauth: Some(oauth),
            ..
        } = &server
        else {
            anyhow::bail!("MCP server is not an OAuth URL server");
        };
        let url = url.clone();
        let settings = oauth
            .settings()
            .ok_or_else(|| anyhow::anyhow!("MCP OAuth settings are unavailable"))?;
        let allow_local = *allow_local;
        let origin = self
            .mcp_current_config()
            .mcp_origins
            .get(&name)
            .and_then(|origins| origins.iter().find(|origin| origin.selected))
            .cloned();
        let proof = Work {
            id: 0,
            session_id: self.ui.session.id.clone(),
            workspace: self.ui.workspace.clone(),
            name: name.clone(),
            server: Some(server.clone()),
            origin: origin.clone(),
            scope: crate::agent::runner::AgentWorkScope::new(),
            task: None,
            receiver: mpsc::channel(1).1,
            attempt: None,
            retirement_failed: false,
        };
        anyhow::ensure!(
            proof.valid(&proof.session_id, &proof.workspace),
            "MCP authentication source changed; no URL contacted"
        );
        let task_name = name.clone();
        let proof = Arc::new(proof);
        self.spawn_mcp_work(name, Some(server), origin, async move {
            if !proof.valid(&proof.session_id, &proof.workspace) { return Err("MCP authentication source changed; no URL contacted".into()); }
            tokio::select! {
                biased;
                _ = crate::agent::runner::current_work_scope_cancelled() => Err("MCP login cancelled".into()),
                result = tokio::time::timeout(crate::extras::mcp::client::MCP_INITIALIZE_TIMEOUT, crate::extras::mcp::oauth::begin_login_bound(&task_name, &url, &settings, allow_local, Arc::new(move || proof.valid(&proof.session_id, &proof.workspace)))) => {
                    let login = result.map_err(|_| "MCP OAuth setup timed out")?.map_err(|_| "MCP OAuth setup failed (credential values omitted)")?;
                    Ok(Completion::LoginReady(Box::new(login)))
                }
            }
        })?;
        self.renderer
            .write_line("MCP OAuth login started; Ctrl-C cancels it", C_AGENT)?;
        Ok(())
    }

    async fn start_mcp_logout(&mut self, name: String) -> anyhow::Result<()> {
        let server = self
            .mcp_server_config(&name)
            .ok_or_else(|| anyhow::anyhow!("MCP server inactive"))?;
        let crate::extras::mcp::config::McpServerConfig::Url {
            url,
            oauth: Some(oauth),
            ..
        } = &server
        else {
            anyhow::bail!("MCP server is not an OAuth URL server");
        };
        let url = url.clone();
        let settings = oauth
            .settings()
            .ok_or_else(|| anyhow::anyhow!("MCP OAuth disabled"))?;
        let origin = self
            .mcp_current_config()
            .mcp_origins
            .get(&name)
            .and_then(|origins| origins.iter().find(|origin| origin.selected))
            .cloned();
        let proof = Work {
            id: 0,
            session_id: self.ui.session.id.clone(),
            workspace: self.ui.workspace.clone(),
            name: name.clone(),
            server: Some(server.clone()),
            origin: origin.clone(),
            scope: crate::agent::runner::AgentWorkScope::new(),
            task: None,
            receiver: mpsc::channel(1).1,
            attempt: None,
            retirement_failed: false,
        };
        let task_name = name.clone();
        self.spawn_mcp_work(name, Some(server), origin, async move {
            crate::agent::runner::spawn_blocking_scoped(move || {
                if !proof.valid(&proof.session_id, &proof.workspace) {
                    return Err(
                        "MCP authentication source changed; no stored credential altered"
                            .to_owned(),
                    );
                }
                let removed = crate::extras::mcp::oauth::logout(&task_name, &url, &settings)
                    .map_err(|_| "MCP logout failed (credential values omitted)".to_owned())?;
                Ok(Completion::LoggedOut { removed })
            })
            .await
            .map_err(|_| "MCP logout worker failed".to_owned())?
        })?;
        self.renderer.write_line("MCP logout started", C_AGENT)?;
        Ok(())
    }

    pub(super) async fn handle_mcp_command(&mut self, text: &str) -> anyhow::Result<bool> {
        if text.split_whitespace().next() != Some("/mcp") {
            return Ok(false);
        }
        let action = match mcp_admin::parse(text) {
            Ok(action) => action,
            Err(error) => {
                self.renderer.write_line(error, C_ERROR)?;
                return Ok(true);
            }
        };
        if matches!(action, Action::Status) {
            self.mcp_status()?;
            return Ok(true);
        }
        if matches!(action, Action::Existing) {
            let parts: Vec<_> = text.split_whitespace().collect();
            if matches!(parts.get(1), Some(&"login" | &"logout")) {
                if parts.len() != 3 {
                    self.renderer
                        .write_line("usage: /mcp login|logout <server>", C_ERROR)?;
                } else if self.mcp_edit_busy() || self.mcp_admin.as_ref().is_some_and(|state| state.work.is_some() || state.approval.is_some()) {
                    self.renderer.write_line("Finish or stop active work before MCP login", C_ERROR)?;
                } else if !self.mcp_server_config(parts[2]).is_some_and(|server| matches!(server, crate::extras::mcp::config::McpServerConfig::Url {oauth:Some(oauth),..} if oauth.settings().is_some())) {
                    self.renderer.write_line("MCP server is inactive or is not an OAuth URL server", C_ERROR)?;
                } else {
                    if parts[1]=="login" { self.start_mcp_login(parts[2].to_owned()).await?; }
                    else { self.start_mcp_logout(parts[2].to_owned()).await?; }
                }
                return Ok(true);
            }
            return Ok(false);
        }
        if self.mcp_edit_busy()
            || self
                .mcp_admin
                .as_ref()
                .is_some_and(|state| state.work.is_some() || state.approval.is_some())
        {
            self.renderer.write_line(
                "Finish or stop active work before editing MCP servers",
                C_ERROR,
            )?;
            return Ok(true);
        }
        #[cfg(feature = "goal")]
        if self
            .ui
            .session
            .goal_store
            .snapshot()
            .is_some_and(|goal| !goal.status.is_terminal())
        {
            self.renderer.write_line(
                "Finish or stop the active goal before editing MCP servers",
                C_ERROR,
            )?;
            return Ok(true);
        }
        let name = match &action {
            Action::Add { name, .. } | Action::Remove { name, .. } => name.clone(),
            _ => unreachable!(),
        };
        if matches!(action, Action::Add { .. })
            && self
                .mcp_current_config()
                .mcp_servers
                .as_ref()
                .is_some_and(|servers| servers.contains_key(&name))
        {
            self.renderer.write_line(
                "MCP name already exists; remove its owning entry explicitly first",
                C_ERROR,
            )?;
            return Ok(true);
        }
        if self
            .mcp_server_config(&name)
            .is_some_and(|server| server.trusted_identity().is_some())
        {
            self.renderer.write_line(
                "Built-in MCP servers use their dedicated disable settings",
                C_ERROR,
            )?;
            return Ok(true);
        }
        if let Action::Remove { project, .. } = &action {
            let origins = self.mcp_current_config().mcp_origins.get(&name);
            let selected =
                origins.and_then(|origins| origins.iter().find(|origin| origin.selected));
            let target_matches = selected.is_none_or(|origin| {
                if *project {
                    origin.source == ".mcp.json"
                } else {
                    self.ui
                        .cfg
                        .provenance
                        .global_file
                        .as_ref()
                        .is_some_and(|path| origin.source == path.to_string_lossy())
                }
            });
            if !target_matches {
                self.renderer.write_line("MCP removal source does not own the selected server; choose its exact source explicitly (project TOML remains manually configured)", C_ERROR)?;
                return Ok(true);
            }
        }
        self.mcp_invalidate_agent().await?;
        let workspace = self.ui.workspace.clone();
        let global_path = self.mcp_global_path()?;
        self.spawn_mcp_work(name, None, None, async move {
            crate::agent::runner::spawn_blocking_scoped(move || {
                mcp_admin::prepare(action, &workspace, &global_path)
            })
            .await
            .map_err(|_| "MCP edit worker failed".to_owned())?
            .map(|(_, completion)| completion)
            .map_err(|error| sanitize_output(&error.to_string()).to_string())
        })?;
        self.renderer
            .write_line("MCP config edit started", C_AGENT)?;
        Ok(true)
    }

    async fn handle_mcp_completion(&mut self, id: u64) -> anyhow::Result<()> {
        let Some(work) = self
            .mcp_admin
            .as_mut()
            .and_then(|state| state.work.as_mut())
        else {
            return Ok(());
        };
        if work.id != id {
            return Ok(());
        }
        let Ok(result) = work.receiver.try_recv() else {
            return Ok(());
        };
        if let Some(task) = &mut work.task {
            let joined = tokio::time::timeout(Duration::from_secs(5), task).await?;
            if !matches!(joined, Ok(Ok(()))) {
                work.retirement_failed = true;
                anyhow::bail!("MCP worker settlement failed; further admission is blocked");
            }
        }
        work.task = None;
        tokio::time::timeout(Duration::from_secs(5), work.scope.wait_idle())
            .await
            .map_err(|_| {
                anyhow::anyhow!("MCP worker did not settle; further admission is blocked")
            })?;
        let valid = work.valid(&self.ui.session.id, &self.ui.workspace);
        if matches!(result, Ok(Completion::RetirementFailed)) {
            work.retirement_failed = true;
            anyhow::bail!("MCP service retirement failed; further admission is blocked");
        }
        let name = work.name.clone();
        let server = work.server.clone();
        let origin = work.origin.clone();
        if let Some(attempt) = work.attempt.take() {
            let outcome = if !valid {
                crate::extras::mcp::health::AttemptOutcome::Cancelled
            } else if result.is_ok() {
                crate::extras::mcp::health::AttemptOutcome::Succeeded
            } else {
                crate::extras::mcp::health::AttemptOutcome::Failed
            };
            attempt.finish(outcome);
        }
        if !valid && !matches!(result, Ok(Completion::Discarded)) {
            self.mcp_state().work = None;
            if let Ok(completion) = result {
                self.spawn_mcp_work(name, None, None, async move {
                    Ok(if completion.retire().await.is_ok() {
                        Completion::Discarded
                    } else {
                        Completion::RetirementFailed
                    })
                })?;
            }
            self.renderer.write_line(
                "MCP result is stale; no connection installed; candidate retirement remains owned",
                C_ERROR,
            )?;
            return Ok(());
        }
        self.mcp_state().work = None;
        let result = match result {
            Ok(Completion::Closed { next, names }) => {
                if !matches!(&*next, Completion::Connected(_))
                    && let Some(manager) = &self.ui.mcp_manager
                {
                    for name in names {
                        manager.record_removed(&name);
                    }
                }
                Ok(*next)
            }
            result => result,
        };
        match result {
            Err(error) => {
                self.renderer
                    .write_line(&sanitize_output(&error), C_ERROR)?;
            }
            Ok(Completion::Prepared {
                cfg,
                approval,
                remove,
            }) => {
                // A project content edit invalidates every old receipt from
                // that file. Retire those handles before asking about new trust.
                let old = self.mcp_current_config().mcp_origins.clone();
                let mut closing = Vec::new();
                if let Some(manager) = &mut self.ui.mcp_manager {
                    for index in (0..manager.handles.len()).rev() {
                        let old_name = manager.handles[index].server_name.as_str();
                        let from_json = approval.is_some()
                            && old.get(old_name).is_some_and(|origins| {
                                origins
                                    .iter()
                                    .any(|origin| origin.selected && origin.source == ".mcp.json")
                            });
                        if from_json || remove && old_name == name {
                            closing.push(manager.handles.remove(index));
                        }
                    }
                }
                if !closing.is_empty() {
                    let next = Completion::Prepared {
                        cfg,
                        approval,
                        remove,
                    };
                    self.spawn_mcp_work(name, None, None, mcp_admin::close_handles(closing, next))?;
                    self.renderer
                        .write_line("MCP previous connections are closing", C_AGENT)?;
                    self.refresh()?;
                    return Ok(());
                }
                self.ui
                    .mcp_manager
                    .get_or_insert_with(|| McpClientManager::from_handles(Vec::new()))
                    .capture_config(&cfg);
                self.mcp_state().live = Some(cfg);
                if let Some((digest, review)) = approval {
                    let captured = Approval {
                        session_id: self.ui.session.id.clone(),
                        workspace: self.ui.workspace.clone(),
                        digest,
                        name,
                        remove,
                    };
                    self.mcp_state().approval = Some(captured);
                    self.renderer
                        .write_line(&sanitize_output(&review), C_AGENT)?;
                    self.renderer.chain_prompt = Some(ChainPrompt {
                        question:
                            "Trust this exact resulting .mcp.json? [Y] approve [N] keep inactive"
                                .into(),
                    });
                    self.renderer.write_line(
                        "Trust this exact resulting .mcp.json? [Y] approve [N] keep inactive",
                        C_PERM,
                    )?;
                } else if remove {
                    self.renderer.write_line("MCP entry removed; connection closed (shadowed definitions remain separately configured)",C_AGENT)?;
                } else {
                    self.start_mcp_reconnect(name).await?;
                }
            }
            Ok(Completion::Connected(handle)) => {
                let manager = self
                    .ui
                    .mcp_manager
                    .get_or_insert_with(|| McpClientManager::from_handles(Vec::new()));
                let old = manager
                    .handles
                    .iter()
                    .position(|old| old.server_name == name)
                    .map(|index| manager.handles.remove(index));
                if let Some(old) = old {
                    self.spawn_mcp_work(
                        name,
                        server,
                        origin,
                        mcp_admin::close_handles(vec![old], Completion::Connected(handle)),
                    )?;
                    self.renderer.write_line(
                        "MCP replacement initialized; previous connection is closing",
                        C_AGENT,
                    )?;
                    self.refresh()?;
                    return Ok(());
                }
                manager.handles.push(handle);
                manager
                    .handles
                    .sort_by(|a, b| a.server_name.cmp(&b.server_name));
                self.run.agent = None;
                self.renderer.write_line(
                    &format!("MCP server '{}' connected", sanitize_output(&name)),
                    C_AGENT,
                )?;
            }
            Ok(Completion::LoginReady(login)) => {
                // This URL is shown only to the operator, after the exact
                // launch definition/workspace was revalidated by this actor.
                self.renderer
                    .write_line("Open this URL to authorize:", C_AGENT)?;
                self.renderer
                    .write_line(&sanitize_output(&login.auth_url), Color::Cyan)?;
                self.renderer.write_line(
                    "Waiting for authorization; Ctrl-C cancels the owned listener",
                    C_AGENT,
                )?;
                let proof = Work {
                    id: 0,
                    session_id: self.ui.session.id.clone(),
                    workspace: self.ui.workspace.clone(),
                    name: name.clone(),
                    server: server.clone(),
                    origin: origin.clone(),
                    scope: crate::agent::runner::AgentWorkScope::new(),
                    task: None,
                    receiver: mpsc::channel(1).1,
                    attempt: None,
                    retirement_failed: false,
                };
                self.spawn_mcp_work(name, server, origin, async move {
                    login
                        .wait_for_callback_owned(Duration::from_secs(180), || {
                            anyhow::ensure!(
                                proof.valid(&proof.session_id, &proof.workspace),
                                "MCP authentication source changed"
                            );
                            Ok(())
                        })
                        .await
                        .map_err(|_| {
                            "MCP login failed or cancelled (credential values omitted)".to_owned()
                        })?;
                    Ok(Completion::Authorized)
                })?;
            }
            Ok(Completion::Authorized) => {
                self.start_mcp_reconnect(name).await?;
            }
            Ok(Completion::LoggedOut { removed }) => {
                let message = if removed {
                    "removed stored OAuth token (effective next start)"
                } else {
                    "no stored OAuth token"
                };
                self.renderer.write_line(
                    &format!("MCP '{}': {message}", sanitize_output(&name)),
                    C_AGENT,
                )?;
            }
            Ok(Completion::Trusted { cfg, remove }) => {
                self.ui
                    .mcp_manager
                    .get_or_insert_with(|| McpClientManager::from_handles(Vec::new()))
                    .capture_config(&cfg);
                self.mcp_state().live = Some(cfg);
                if remove {
                    self.renderer.write_line(
                        "MCP removal saved; remaining exact project content approved",
                        C_AGENT,
                    )?;
                } else {
                    self.start_mcp_reconnect(name).await?;
                }
            }
            Ok(Completion::Discarded) => {
                self.renderer.write_line(
                    "Stale MCP candidate retired; no new authority adopted",
                    C_AGENT,
                )?;
            }
            Ok(Completion::Closed { .. } | Completion::RetirementFailed) => {
                unreachable!("closed/failed stage handled before dispatch")
            }
        }
        self.refresh()?;
        Ok(())
    }

    pub(super) async fn handle_mcp_user_event(
        &mut self,
        event: &UserEvent,
    ) -> anyhow::Result<bool> {
        if let UserEvent::McpAdminDone { id } = event {
            self.handle_mcp_completion(*id).await?;
            return Ok(true);
        }
        let state = self.mcp_admin.as_ref();
        if matches!(event, UserEvent::Quit) {
            self.retire_mcp_work().await?;
            if let Some(state) = &mut self.mcp_admin {
                state.approval = None;
            }
            self.renderer.chain_prompt = None;
            return Ok(false);
        }
        let UserEvent::Key(key) = event else {
            return Ok(false);
        };
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Ok(false);
        }
        let cancel = key.code == KeyCode::Esc
            || matches!(key.code, KeyCode::Char('c' | 'd'))
                && key.modifiers.contains(KeyModifiers::CONTROL);
        if state.is_some_and(|state| state.work.is_some()) && cancel {
            self.retire_mcp_work().await?;
            self.discard_interrupted_queue();
            self.renderer.write_line("MCP operation cancelled; saved edits stay configured but no new connection is authorized",C_AGENT)?;
            self.refresh()?;
            return Ok(true);
        }
        if state.is_none_or(|state| state.approval.is_none()) {
            return Ok(false);
        }
        if matches!(
            key.code,
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::BackTab
        ) || key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return Ok(false);
        }
        if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT || cancel {
            match key.code {
                KeyCode::Char('y' | 'Y') if !cancel => {
                    let approval = self.mcp_state().approval.take().unwrap();
                    self.renderer.chain_prompt = None;
                    if !approval.valid(&self.ui.session.id, &self.ui.workspace) {
                        self.renderer.write_line(
                            "MCP trust approval is stale; no receipt or connection created",
                            C_ERROR,
                        )?;
                        return Ok(true);
                    }
                    self.discard_interrupted_queue();
                    let name = approval.name.clone();
                    let global_path = self.mcp_global_path()?;
                    self.spawn_mcp_work(name, None, None, async move {
                        let cancellation = crate::agent::runner::current_work_cancellation();
                        crate::agent::runner::spawn_blocking_scoped(move || {
                            if cancellation
                                .as_ref()
                                .is_some_and(|cancel| cancel.is_cancelled())
                            {
                                return Err("MCP trust approval cancelled".to_owned());
                            }
                            if !approval.valid(&approval.session_id, &approval.workspace) {
                                return Err("MCP trust approval is stale".to_owned());
                            }
                            let paths = crate::paths::AppPaths::from_process(Some(
                                approval.workspace.root().to_path_buf(),
                            ))
                            .map_err(|_| "MCP private paths unavailable")?;
                            config::approve_project_mcp(
                                &paths,
                                &approval.workspace,
                                &approval.digest,
                            )
                            .map_err(|_| "MCP trust receipt could not be stored")?;
                            let cfg = config::mcp_config_in_workspace_at(
                                &approval.workspace,
                                &global_path,
                            )
                            .map_err(|_| "MCP approved config unavailable")?;
                            if !approval.valid(&approval.session_id, &approval.workspace) {
                                return Err(
                                    "MCP content changed after approval; no reconnect".to_owned()
                                );
                            }
                            Ok(Completion::Trusted {
                                cfg: Box::new(cfg),
                                remove: approval.remove,
                            })
                        })
                        .await
                        .map_err(|_| "MCP trust worker failed".to_owned())?
                    })?;
                    self.renderer.write_line(
                        "Recording exact MCP trust; Ctrl-C cancels further connection work",
                        C_AGENT,
                    )?;
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc | KeyCode::Char('c' | 'd')
                    if cancel || matches!(key.code, KeyCode::Char('n' | 'N')) =>
                {
                    self.mcp_state().approval = None;
                    self.renderer.chain_prompt = None;
                    self.discard_interrupted_queue();
                    self.renderer.write_line(
                        "MCP edit saved; project definitions remain inactive",
                        C_AGENT,
                    )?;
                }
                _ => {}
            }
        }
        self.refresh()?;
        Ok(true)
    }
}
