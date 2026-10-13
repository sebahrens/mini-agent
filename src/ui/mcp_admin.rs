//! Operator-owned MCP edits/reconnects. UI notifications carry only an exact
//! generation; the actual handle remains in the run-local owned receiver.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::extras::mcp::client::McpClientHandle;
use crate::extras::mcp::config::McpServerConfig;
use crate::paths::WorkspaceBinding;

pub(super) enum Action {
    Add {
        name: String,
        server: McpServerConfig,
        project: bool,
    },
    Remove {
        name: String,
        project: bool,
    },
    Status,
    Existing,
}

fn words(text: &str) -> Result<Vec<String>, &'static str> {
    if text.len() > 32 * 1024 || text.contains(['\0', '\n', '\r']) {
        return Err("MCP command exceeds bounds or contains control text");
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote = None;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if let Some(delimiter) = quote {
            if ch == delimiter {
                quote = None;
            } else if delimiter == '"'
                && ch == '\\'
                && chars.peek().is_some_and(|ch| *ch == '"' || *ch == '\\')
            {
                word.push(chars.next().unwrap());
            } else {
                word.push(ch);
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
            started = true;
        } else if ch.is_whitespace() {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else if ch == '\\'
            && chars
                .peek()
                .is_some_and(|ch| ch.is_whitespace() || *ch == '\'' || *ch == '"' || *ch == '\\')
        {
            word.push(chars.next().unwrap());
            started = true;
        } else {
            word.push(ch);
            started = true;
        }
        if word.len() > 4096 || words.len() > 128 {
            return Err("MCP argument exceeds bounds");
        }
    }
    if quote.is_some() {
        return Err("MCP command contains an unfinished quote");
    }
    if started {
        words.push(word);
    }
    if words.len() > 128 {
        return Err("MCP argument exceeds bounds");
    }
    Ok(words)
}

pub(super) fn parse(text: &str) -> Result<Action, &'static str> {
    let mut words = words(text)?;
    if words.first().map(String::as_str) != Some("/mcp") {
        return Ok(Action::Existing);
    }
    if words.len() == 1 {
        return Ok(Action::Status);
    }
    if !matches!(words[1].as_str(), "add" | "remove") {
        return Ok(Action::Existing);
    }
    let add = words[1] == "add";
    words.drain(..2);
    let mut project = false;
    let mut local = false;
    let mut positional = Vec::new();
    let mut flags = true;
    for value in words {
        if flags && value == "--" {
            flags = false;
            continue;
        }
        if flags && value == "--project" {
            if project {
                return Err("duplicate --project");
            }
            project = true;
        } else if flags && value == "--allow-local" {
            if local {
                return Err("duplicate --allow-local");
            }
            local = true;
        } else {
            positional.push(value);
        }
    }
    let Some(name) = positional.first().filter(|name| {
        !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
    }) else {
        return Err(
            "usage: /mcp add <name> <command|url> [args...] [--project] [--allow-local], /mcp remove <name> [--project]",
        );
    };
    let name = name.clone();
    if !add {
        if positional.len() != 1 || local {
            return Err("usage: /mcp remove <name> [--project]");
        }
        return Ok(Action::Remove { name, project });
    }
    if positional.len() < 2 || positional[1].is_empty() {
        return Err("MCP add requires a command or URL");
    }
    let target = positional[1].clone();
    let server = if target.starts_with("http://") || target.starts_with("https://") {
        if positional.len() != 2 {
            return Err("URL MCP servers do not take command arguments");
        }
        McpServerConfig::Url {
            url: target,
            allow_local: local,
            headers: HashMap::new(),
            oauth: None,
        }
    } else {
        if local {
            return Err("--allow-local applies only to URL MCP servers");
        }
        McpServerConfig::Command {
            command: target,
            args: positional[2..].to_vec(),
            cwd: None,
            env: HashMap::new(),
            inherit_env: Vec::new(),
            sandbox: None,
            network: Default::default(),
        }
    };
    Ok(Action::Add {
        name,
        server,
        project,
    })
}

pub(super) struct Approval {
    pub session_id: compact_str::CompactString,
    pub workspace: Arc<WorkspaceBinding>,
    pub digest: String,
    pub name: String,
    pub remove: bool,
}
impl Approval {
    pub fn valid(&self, session_id: &str, workspace: &Arc<WorkspaceBinding>) -> bool {
        self.session_id == session_id
            && Arc::ptr_eq(&self.workspace, workspace)
            && workspace.validate().is_ok()
            && crate::extras::mcp::project::FileSnapshot::read(
                workspace,
                std::path::Path::new(".mcp.json"),
            )
            .is_ok_and(|snapshot| {
                snapshot.exists()
                    && crate::extras::mcp::project::digest(&snapshot.content) == self.digest
            })
    }
}

pub(super) enum Completion {
    Prepared {
        cfg: Box<crate::config::Config>,
        approval: Option<(String, String)>,
        remove: bool,
    },
    Connected(McpClientHandle),
    LoginReady(Box<crate::extras::mcp::oauth::LoginSession>),
    Authorized,
    LoggedOut {
        removed: bool,
    },
    Trusted {
        cfg: Box<crate::config::Config>,
        remove: bool,
    },
    Closed {
        next: Box<Completion>,
        names: Vec<String>,
    },
    RetirementFailed,
    Discarded,
}

impl Completion {
    pub(super) async fn retire(self) -> anyhow::Result<()> {
        let mut current = self;
        loop {
            match current {
                Self::Connected(mut handle) => {
                    return handle
                        .running_service
                        .close()
                        .await
                        .map(|_| ())
                        .map_err(anyhow::Error::from);
                }
                Self::Closed { next, .. } => current = *next,
                _ => return Ok(()),
            }
        }
    }
}

pub(super) async fn close_handles(
    handles: Vec<McpClientHandle>,
    next: Completion,
) -> Result<Completion, String> {
    let names = handles
        .iter()
        .map(|handle| handle.server_name.to_string())
        .collect();
    let mut failed = false;
    // One actor owns every retained handle and attempts every close, including
    // after a sibling failure. Retirement keeps this actor retained while it
    // is settling rather than dropping an incomplete service-close await.
    for mut handle in handles {
        failed |= handle.running_service.close().await.is_err();
    }
    if failed {
        next.retire()
            .await
            .map_err(|_| "MCP candidate retirement failed")?;
        Ok(Completion::RetirementFailed)
    } else {
        Ok(Completion::Closed {
            next: Box::new(next),
            names,
        })
    }
}

pub(super) struct Work {
    pub id: u64,
    pub session_id: compact_str::CompactString,
    pub workspace: Arc<WorkspaceBinding>,
    pub name: String,
    pub server: Option<McpServerConfig>,
    pub origin: Option<crate::extras::mcp::project::ServerOrigin>,
    pub scope: Arc<crate::agent::runner::AgentWorkScope>,
    pub task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    pub receiver: tokio::sync::mpsc::Receiver<Result<Completion, String>>,
    pub attempt: Option<crate::extras::mcp::health::AttemptGuard>,
    pub retirement_failed: bool,
}
impl Work {
    pub async fn retire(&mut self) -> anyhow::Result<()> {
        // A previous failure remains an admission obstruction, but cannot skip
        // cancellation/join when the final App teardown retries exact ownership.
        self.scope.cancellation_handle().cancel();
        self.receiver.close();
        tokio::time::timeout(Duration::from_secs(5), async {
            if let Some(task) = &mut self.task
                && !matches!(task.await, Ok(Ok(())))
            {
                self.retirement_failed = true;
            }
            self.task = None;
            while let Ok(result) = self.receiver.try_recv() {
                if let Ok(completion) = result
                    && completion.retire().await.is_err()
                {
                    self.retirement_failed = true;
                }
            }
            self.scope.wait_idle().await;
            anyhow::ensure!(
                !self.retirement_failed,
                "MCP retirement failed; further admission is blocked"
            );
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!("MCP reconnect did not settle; further admission is blocked")
        })??;
        // Cancellation is recorded only once the exact owning task/transport
        // has settled; a timeout keeps the retirement obstruction visible.
        self.attempt.take();
        Ok(())
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        self.scope.cancellation_handle().cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    pub generation: u64,
    pub approval: Option<Approval>,
    pub work: Option<Work>,
    pub live: Option<Box<crate::config::Config>>,
}

impl Work {
    pub fn valid(&self, session_id: &str, workspace: &Arc<WorkspaceBinding>) -> bool {
        if self.session_id != session_id
            || !Arc::ptr_eq(&self.workspace, workspace)
            || workspace.validate().is_err()
        {
            return false;
        }
        crate::extras::mcp::project::server_source_is_current(
            workspace,
            &self.name,
            self.server.as_ref(),
            self.origin.as_ref(),
        )
    }
}

pub(super) fn prepare(
    action: Action,
    workspace: &WorkspaceBinding,
    global_path: &std::path::Path,
) -> anyhow::Result<(String, Completion)> {
    let (name, server, project) = match action {
        Action::Add {
            name,
            server,
            project,
        } => (name, Some(server), project),
        Action::Remove { name, project } => (name, None, project),
        _ => anyhow::bail!("not an MCP mutation"),
    };
    let remove = server.is_none();
    let approval = if project {
        let snapshot = crate::extras::mcp::project::FileSnapshot::read(
            workspace,
            std::path::Path::new(".mcp.json"),
        )?;
        let value = server.as_ref().map(serde_json::to_value).transpose()?;
        let changed = crate::extras::mcp::project::edit_json(&snapshot.content, &name, value)
            .map_err(anyhow::Error::msg)?;
        let review = crate::extras::mcp::project::review(&changed).map_err(anyhow::Error::msg)?;
        let digest = crate::extras::mcp::project::digest(&changed);
        snapshot.publish(workspace, &changed)?;
        Some((digest, review))
    } else {
        crate::config::save_mcp_server_at(global_path, &name, server.as_ref())?;
        None
    };
    let cfg = crate::config::mcp_config_in_workspace_at(workspace, global_path)?;
    if !project && let Some(server) = server {
        let current = cfg
            .mcp_servers
            .as_ref()
            .and_then(|servers| servers.get(&name));
        anyhow::ensure!(
            current.and_then(|s| serde_json::to_value(s).ok()) == serde_json::to_value(server).ok(),
            "MCP entry changed during publication; no reconnect authorized"
        );
    }
    Ok((
        name,
        Completion::Prepared {
            cfg: Box::new(cfg),
            approval,
            remove,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_parser_final_word_respects_total_and_unicode_byte_bounds() {
        assert_eq!(words(&vec!["x"; 128].join(" ")).unwrap().len(), 128);
        assert!(words(&vec!["x"; 129].join(" ")).is_err());
        assert!(words(&"雪".repeat(1366)).is_err());
        assert!(words("/mcp add named runner\0arg").is_err());
    }
    #[test]
    fn literal_parser_preserves_unicode_spaces_empty_arguments_and_windows_paths_without_shell_evaluation()
     {
        let Action::Add {
            name,
            server,
            project,
        } = parse(
            r#"/mcp add 雪 'C:\Program Files\runner.exe' 'a b' "" '$(touch nope)' --project"#,
        )
        .unwrap()
        else {
            panic!()
        };
        assert_eq!(name, "雪");
        assert!(project);
        let McpServerConfig::Command { command, args, .. } = server else {
            panic!()
        };
        assert_eq!(command, r"C:\Program Files\runner.exe");
        assert_eq!(args, ["a b", "", "$(touch nope)"]);
        let Action::Add { server, .. } =
            parse("/mcp add local http://localhost:3000/mcp --allow-local").unwrap()
        else {
            panic!()
        };
        assert!(matches!(
            server,
            McpServerConfig::Url {
                allow_local: true,
                ..
            }
        ));
        let Action::Add { server, .. } = parse("/mcp add runner cmd -- --project").unwrap() else {
            panic!()
        };
        let McpServerConfig::Command { args, .. } = server else {
            panic!()
        };
        assert_eq!(args, ["--project"]);
        for text in [
            "/mcp add",
            "/mcp add x cmd --allow-local",
            "/mcp add x https://example.com extra",
            "/mcp remove x --allow-local",
            "/mcp add x cmd --project --project",
            "/mcp add x 'unterminated",
            "/mcp remove x y",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[tokio::test]
    async fn failed_owned_retirement_remains_an_admission_obstruction_after_task_is_joined() {
        let root = crate::tests::OwnedTestDirectory::new("mcp-retirement-error");
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut work = Work {
            id: 1,
            session_id: "owned-session".into(),
            workspace,
            name: "owned".into(),
            server: None,
            origin: None,
            scope: crate::agent::runner::AgentWorkScope::new(),
            task: Some(tokio::spawn(async {
                anyhow::bail!("controlled retirement failure")
            })),
            receiver: tokio::sync::mpsc::channel(1).1,
            attempt: None,
            retirement_failed: false,
        };
        assert!(work.retire().await.is_err());
        assert!(work.retirement_failed && work.task.is_none());
        assert!(
            work.retire().await.is_err(),
            "a second interrupt must not clear a failed settlement boundary"
        );
    }

    #[tokio::test]
    async fn cancellation_joins_the_exact_actor_before_retirement_can_clear_work() {
        for retirement_failed in [false, true] {
            let root = crate::tests::OwnedTestDirectory::new("mcp-retirement-cancel");
            let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let scope = crate::agent::runner::AgentWorkScope::new();
            let task_scope = scope.clone();
            let settled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let task_settled = settled.clone();
            let task = tokio::spawn(async move {
                task_scope
                    .run(async {
                        crate::agent::runner::current_work_scope_cancelled().await;
                        task_settled.store(true, std::sync::atomic::Ordering::Release);
                        Ok(())
                    })
                    .await
            });
            let mut work = Work {
                id: 1,
                session_id: "owned-session".into(),
                workspace,
                name: "owned".into(),
                server: None,
                origin: None,
                scope,
                task: Some(task),
                receiver: tokio::sync::mpsc::channel(1).1,
                attempt: None,
                retirement_failed,
            };
            let result = work.retire().await;
            assert_eq!(result.is_err(), retirement_failed);
            assert_eq!(work.retirement_failed, retirement_failed);
            assert!(settled.load(std::sync::atomic::Ordering::Acquire));
            assert!(work.task.is_none());
        }
    }
}
