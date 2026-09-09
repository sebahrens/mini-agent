use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as FmtWrite;
use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use compact_str::CompactString;
use rig::completion::{Message, Usage};
use rig::message::{AssistantContent, ToolResultContent, UserContent};
use serde::Serialize;

use crate::cli;
use crate::config;
use crate::sandbox::Sandbox;
use crate::session;
use crate::session::{
    MessageRole, PersistedCallProvenance, PersistedReasoning, Session, persisted_call_identifier,
};

const CHAT_HISTORY_FILE_LABEL: &str = "chat history file";

/// Wait for a headless interruption without affecting the interactive UI.
pub(crate) async fn headless_interrupt() -> io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = interrupt.recv() => Ok(()),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

pub(crate) async fn run_headless_command<F, T>(command: F) -> io::Result<T>
where
    F: std::future::Future<Output = io::Result<T>>,
{
    let scope = crate::agent::runner::AgentWorkScope::new();
    let result = scope
        .run(async {
            tokio::select! {
                // Poll first to install signal handlers before launching work.
                biased;
                signal = headless_interrupt() => {
                    signal?;
                    Err(io::Error::new(io::ErrorKind::Interrupted, "headless command interrupted"))
                }
                result = command => result,
            }
        })
        .await;
    // Dropping the command future closes its response receiver. The scoped
    // worker then cancels, kills/reaps the group, and completes its audit.
    scope.cancellation_handle().cancel();
    scope.wait_idle().await;
    result
}

/// Let the runner observe cancellation and return its partial transcript before
/// waiting for tool and hook cleanup. Dropping the whole turn would lose it.
pub(crate) async fn run_headless_turn<F>(turn: F) -> crate::agent::runner::HeadlessTurn
where
    F: std::future::Future<Output = crate::agent::runner::HeadlessTurn>,
{
    let scope = crate::agent::runner::AgentWorkScope::new();
    let result = scope
        .run(async {
            tokio::pin!(turn);
            tokio::select! {
                biased;
                signal = headless_interrupt() => {
                    scope.cancellation_handle().cancel();
                    let mut result = turn.await;
                    if let Err(error) = signal {
                        result.failure = Some(anyhow::anyhow!(error).context("headless signal handler failed"));
                    }
                    result
                }
                result = &mut turn => result,
            }
        })
        .await;
    scope.cancellation_handle().cancel();
    scope.wait_idle().await;
    result
}

/// Char-safe short preview of a session id for listings. Ids are normally
/// 32 hex chars, but imported sessions (`/import`) may carry shorter or
/// non-ASCII ids, where a byte slice (`&id[..8]`) would panic.
pub(crate) fn short_session_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspacePathState {
    status: [u8; 2],
    size: Option<u64>,
    modified_nanos: Option<u128>,
    kind: Option<u8>,
}

/// A bounded Git view of workspace paths that differ from `HEAD`. The
/// metadata fields let a later snapshot notice another edit to a path that
/// was already dirty when the headless run started.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkspaceChangeBaseline {
    paths: BTreeMap<String, WorkspacePathState>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct HeadlessToolCalls {
    pub total: u64,
    pub by_name: BTreeMap<String, u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct HeadlessJsonOutput {
    pub result: String,
    pub files_changed: Vec<String>,
    pub tool_calls: HeadlessToolCalls,
    pub usage: Usage,
    pub cost: f64,
    pub stop_reason: HeadlessStopReason,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HeadlessStopReason {
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct HeadlessPricing {
    pub anthropic_native: bool,
    pub input_token_cost: f64,
    pub output_token_cost: f64,
}

fn parse_git_status(output: &[u8]) -> BTreeMap<String, [u8; 2]> {
    output
        .split(|byte| *byte == 0)
        .filter_map(|record| {
            if record.len() < 4 || record[2] != b' ' {
                return None;
            }
            let path = String::from_utf8_lossy(&record[3..]).into_owned();
            (!path.is_empty()).then_some((path, [record[0], record[1]]))
        })
        .collect()
}

fn workspace_path_state(root: &Path, path: &str, status: [u8; 2]) -> WorkspacePathState {
    let metadata = std::fs::symlink_metadata(root.join(path)).ok();
    let kind = metadata.as_ref().map(|metadata| {
        let file_type = metadata.file_type();
        if file_type.is_file() {
            1
        } else if file_type.is_dir() {
            2
        } else if file_type.is_symlink() {
            3
        } else {
            4
        }
    });
    WorkspacePathState {
        status,
        size: metadata.as_ref().map(std::fs::Metadata::len),
        modified_nanos: metadata
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos()),
        kind,
    }
}

/// Capture the paths currently dirty in Git. Failure (including a non-Git
/// workspace) is deliberately non-fatal: explicit mutation tool targets can
/// still populate the JSON result.
pub(crate) async fn capture_workspace_change_baseline(
    root: &Path,
) -> Option<WorkspaceChangeBaseline> {
    let runner = crate::git::runner::GitRunner::discover().ok()?;
    let output = runner
        .run(
            root,
            "headless-output-status",
            [
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--no-renames",
            ],
            crate::git::runner::QUERY_LIMITS,
        )
        .await
        .ok()?;
    let root = root.to_path_buf();
    let statuses = parse_git_status(&output.stdout);
    let paths = tokio::task::spawn_blocking(move || {
        statuses
            .into_iter()
            .map(|(path, status)| {
                let state = workspace_path_state(&root, &path, status);
                (path, state)
            })
            .collect()
    })
    .await
    .ok()?;
    Some(WorkspaceChangeBaseline { paths })
}

fn normalize_tool_path(root: &Path, raw: &str) -> Option<String> {
    let path = Path::new(raw);
    if path.as_os_str().is_empty() {
        return None;
    }
    let relative: PathBuf = if path.is_absolute() {
        path.strip_prefix(root).ok()?.to_path_buf()
    } else {
        path.to_path_buf()
    };
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(component) => normalized.push(component),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then(|| normalized.to_string_lossy().into_owned())
}

fn interaction_summary(
    root: &Path,
    interactions: &[Message],
) -> (HeadlessToolCalls, BTreeSet<String>) {
    let mut by_name = BTreeMap::<String, u64>::new();
    let mut files = BTreeSet::new();
    for interaction in interactions {
        let Message::Assistant { content, .. } = interaction else {
            continue;
        };
        for item in content.iter() {
            let AssistantContent::ToolCall(call) = item else {
                continue;
            };
            *by_name.entry(call.function.name.clone()).or_default() += 1;
            if matches!(call.function.name.as_str(), "write" | "edit")
                && let Some(path) = call
                    .function
                    .arguments
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|path| normalize_tool_path(root, path))
            {
                files.insert(path);
            }
        }
    }
    let total = by_name.values().copied().sum();
    (HeadlessToolCalls { total, by_name }, files)
}

pub(crate) async fn files_changed_since(
    root: &Path,
    baseline: Option<&WorkspaceChangeBaseline>,
    interactions: &[Message],
) -> Vec<String> {
    let (_, mut files) = interaction_summary(root, interactions);
    let after = capture_workspace_change_baseline(root).await;
    if let (Some(before), Some(after)) = (baseline, after.as_ref()) {
        for path in before.paths.keys().chain(after.paths.keys()) {
            if before.paths.get(path) != after.paths.get(path) {
                files.insert(path.clone());
            }
        }
    }
    files.into_iter().collect()
}

pub(crate) fn render_headless_json(
    root: &Path,
    result: &str,
    interactions: &[Message],
    usage: Usage,
    files_changed: Vec<String>,
    pricing: HeadlessPricing,
    stop_reason: HeadlessStopReason,
) -> serde_json::Result<String> {
    let (tool_calls, explicit_files) = interaction_summary(root, interactions);
    let files_changed = files_changed
        .into_iter()
        .chain(explicit_files)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let billable_input = crate::pricing::billable_input_tokens(
        pricing.anthropic_native,
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.cache_creation_input_tokens,
    );
    let cost = crate::pricing::estimate_cost(
        billable_input,
        usage.output_tokens,
        pricing.input_token_cost,
        pricing.output_token_cost,
    );
    let cost = if cost.is_finite() { cost } else { 0.0 };
    serde_json::to_string(&HeadlessJsonOutput {
        result: result.to_string(),
        files_changed,
        tool_calls,
        usage,
        cost,
        stop_reason,
    })
}

/// Persist one completed headless (`-p`) turn in the same record order the
/// interactive UI produces while a turn streams: the user prompt, then every
/// tool call and result in provider order, then the final assistant message.
/// `--continue` replays the session in record order, so tool records written
/// after the assistant text would be replayed out of sequence.
///
/// `interactions` is the runner's canonical provider transcript for the turn
/// (`run_print`'s third return value), accumulated across every stream of the
/// turn. Tool results are attributed to their tool by the same identifier the
/// call was recorded under so the record carries the real tool name; assistant
/// text inside `interactions` is not persisted separately because `response`
/// already carries the turn's complete text.
///
/// Records are keyed by [`persisted_call_identifier`] — the provider `call_id`
/// when there is one — because the interactive path adopts that same key when
/// it commits a turn (`event_handler::commit_turn_response`), so `--continue`
/// replays a session identically whichever mode wrote it (mini-agent-wzv1).
/// The provider's own item id and the reasoning items emitted before each call
/// are recorded beside the record, which is what lets the replay present a
/// native `function_call` together with its required `reasoning` item.
pub(crate) fn persist_headless_turn(
    session: &mut Session,
    prompt: &str,
    response: &str,
    interactions: &[Message],
) {
    session.add_message(MessageRole::User, prompt);
    let mut tool_names: HashMap<String, &str> = HashMap::new();
    let mut pending_reasoning: Vec<PersistedReasoning> = Vec::new();
    for interaction in interactions {
        match interaction {
            Message::Assistant { content, .. } => {
                for item in content.iter() {
                    match item {
                        AssistantContent::Reasoning(reasoning) => {
                            let persisted = PersistedReasoning::from_rig(reasoning);
                            if !persisted.is_empty() {
                                pending_reasoning.push(persisted);
                            }
                        }
                        AssistantContent::ToolCall(call) => {
                            let identifier = persisted_call_identifier(
                                call.call_id.as_deref(),
                                call.id.as_str(),
                            )
                            .to_string();
                            tool_names.insert(identifier.clone(), call.function.name.as_str());
                            session.add_tool_call_with_id(
                                &identifier,
                                &call.function.name,
                                &call.function.arguments,
                            );
                            session.record_tool_call_provenance(
                                &identifier,
                                PersistedCallProvenance {
                                    provider_item_id: Some(CompactString::new(call.id.as_str())),
                                    provider_call_id: call
                                        .call_id
                                        .as_deref()
                                        .map(CompactString::new),
                                    reasoning: std::mem::take(&mut pending_reasoning),
                                },
                            );
                        }
                        AssistantContent::Text(_) | AssistantContent::Image(_) => {}
                    }
                }
            }
            Message::User { content } => {
                pending_reasoning.clear();
                for item in content.iter() {
                    let UserContent::ToolResult(result) = item else {
                        continue;
                    };
                    let identifier =
                        persisted_call_identifier(result.call_id.as_deref(), result.id.as_str())
                            .to_string();
                    let name = tool_names
                        .get(identifier.as_str())
                        .copied()
                        .unwrap_or("unknown");
                    let output = result
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            ToolResultContent::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    session.add_tool_result_with_id(&identifier, name, &output);
                }
            }
            Message::System { .. } => {}
        }
    }
    session.add_message(MessageRole::Assistant, response);
}

const CHAT_HISTORY_ENTRY_LIMIT_LABEL: &str = "chat history entry limit";
const CHAT_HISTORY_PATH_POLICY_LABEL: &str = "chat history path policy";
const INSTALLED_BUILD_LABEL: &str = "installed binary provenance";

fn javascript_worker_compiled_entry() -> (&'static str, String) {
    ("compiled", cfg!(feature = "js").to_string())
}

#[cfg(feature = "js")]
fn javascript_worker_entries_for_status(
    status: crate::sandbox::worker::WorkerContainmentStatus,
) -> Vec<(&'static str, String)> {
    use crate::sandbox::worker::{
        WorkerBackend, WorkerContainmentAssurance, WorkerContainmentStatus,
    };

    let (backend, assurance, availability, reason, available) = match status {
        WorkerContainmentStatus::Available { backend, assurance } => {
            (backend, assurance, "available", None, true)
        }
        WorkerContainmentStatus::Unavailable {
            backend,
            assurance,
            reason,
        } => (backend, assurance, "unavailable", Some(reason), false),
    };
    let assurance = match (assurance, available) {
        (WorkerContainmentAssurance::Enforced, true) => "enforced",
        #[cfg(any(test, target_os = "macos"))]
        (WorkerContainmentAssurance::DeprecatedBestEffort, true) => "deprecated weaker best-effort",
        (WorkerContainmentAssurance::Enforced, false) => "enforced backend class; inactive",
        #[cfg(any(test, target_os = "macos"))]
        (WorkerContainmentAssurance::DeprecatedBestEffort, false) => {
            "deprecated weaker best-effort backend class; inactive"
        }
    };
    let mut entries = vec![
        javascript_worker_compiled_entry(),
        ("backend", backend.to_string()),
        ("status", availability.to_string()),
        ("assurance", assurance.to_string()),
    ];
    if let Some(reason) = reason {
        entries.insert(3, ("reason", reason));
    }
    if available {
        let (process_limit, memory_limit, cpu_limit) = match backend {
            WorkerBackend::Bubblewrap => (
                "process creation denied",
                "at most 256 MiB",
                "at most 35 seconds",
            ),
            WorkerBackend::WindowsLpac => ("1 worker process", "256 MiB", "35 seconds"),
            WorkerBackend::Seatbelt => (
                "process creation denied",
                "at most 40 GiB virtual address space",
                "at most 35 seconds",
            ),
        };
        let spawn = if backend == WorkerBackend::WindowsLpac {
            "disabled on Windows"
        } else {
            "parent capability broker only"
        };
        entries.extend([
            (
                "worker launch",
                "enabled with validated containment".to_string(),
            ),
            (
                "filesystem authority",
                "none ambient; parent broker only".to_string(),
            ),
            (
                "network authority",
                "none ambient; parent broker only".to_string(),
            ),
            ("process creation", "denied in worker".to_string()),
            ("native process limit", process_limit.to_string()),
            ("native memory limit", memory_limit.to_string()),
            ("native CPU limit", cpu_limit.to_string()),
            ("parent-brokered spawn", spawn.to_string()),
        ]);
    } else {
        entries.extend([
            (
                "worker launch",
                "disabled; no worker process starts".to_string(),
            ),
            (
                "filesystem authority",
                "not granted; no worker process".to_string(),
            ),
            (
                "network authority",
                "not granted; no worker process".to_string(),
            ),
            (
                "process creation",
                "not applicable; no worker process".to_string(),
            ),
            (
                "native process limit",
                "not active; no worker process".to_string(),
            ),
            (
                "native memory limit",
                "not active; no worker process".to_string(),
            ),
            (
                "native CPU limit",
                "not active; no worker process".to_string(),
            ),
            (
                "parent-brokered spawn",
                "unavailable; no JS worker".to_string(),
            ),
        ]);
    }
    entries.push((
        "protocol version",
        crate::extras::js::protocol::PROTOCOL_VERSION.to_string(),
    ));
    entries
}

fn javascript_worker_entries(eligible: bool) -> Vec<(&'static str, String)> {
    #[cfg(feature = "js")]
    {
        if eligible {
            javascript_worker_entries_for_status(crate::sandbox::worker::containment_status())
        } else {
            javascript_worker_entries_for_status(
                crate::sandbox::worker::WorkerContainmentStatus::Unavailable {
                    backend: crate::sandbox::worker::WorkerBackend::for_current_platform(),
                    assurance: crate::sandbox::worker::WorkerContainmentAssurance::Enforced,
                    reason: "JavaScript tool was not requested".to_string(),
                },
            )
        }
    }
    #[cfg(not(feature = "js"))]
    {
        let _ = eligible;
        vec![
            javascript_worker_compiled_entry(),
            ("status", "not compiled".to_string()),
        ]
    }
}

fn installed_build_entry() -> (&'static str, String) {
    let assertion_mode = if cfg!(debug_assertions) {
        "debug assertions enabled"
    } else {
        "debug assertions disabled"
    };

    (
        INSTALLED_BUILD_LABEL,
        format!(
            "Cargo-installed package {} version {} is runnable; {assertion_mode} at compile time; target {}-{}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            std::env::consts::ARCH,
            std::env::consts::OS,
        ),
    )
}

fn chat_history_limit_entry() -> (&'static str, String) {
    (
        CHAT_HISTORY_ENTRY_LIMIT_LABEL,
        format!(
            "bounded retention keeps newest {} entries and discards older entries",
            session::chat_history::MAX_CHAT_HISTORY_ENTRIES
        ),
    )
}

fn chat_history_path_policy_entry() -> (&'static str, String) {
    (
        CHAT_HISTORY_PATH_POLICY_LABEL,
        crate::paths::PRIVATE_PATH_LINK_POLICY.to_string(),
    )
}

#[cfg(feature = "advisor")]
fn advisor_context_limit_entry(kilobytes: u32) -> (&'static str, String) {
    let per_side = u64::from(kilobytes) * 1024 / 2;
    (
        "context-limit",
        format!("{kilobytes} KB ({per_side} head / {per_side} tail)"),
    )
}

fn append_section(output: &mut String, title: &str, entries: &[(&str, String)]) {
    writeln!(output, "{}:", title).expect("writing configuration output to a String cannot fail");
    let width = entries.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, v) in entries {
        writeln!(output, "  {k:<width$}  {v}")
            .expect("writing configuration output to a String cannot fail");
    }
    writeln!(output).expect("writing configuration output to a String cannot fail");
}

fn write_output(mut writer: impl IoWrite, output: &str) -> io::Result<()> {
    if let Err(error) = writer.write_all(output.as_bytes()) {
        if error.kind() == io::ErrorKind::BrokenPipe {
            return Ok(());
        }
        return Err(error);
    }
    match writer.flush() {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

pub(crate) fn print_sessions() {
    let sessions = match session::storage::find_recent_sessions(20) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error listing sessions: {e}");
            return;
        }
    };
    if sessions.is_empty() {
        println!("no saved sessions");
    } else {
        println!("recent sessions ({}):", sessions.len());
        for s in &sessions {
            let last = s
                .messages
                .last()
                .map(|m| {
                    let truncated: String = m.content.chars().take(30).collect();
                    format!("...{truncated}")
                })
                .unwrap_or_default();
            let time = crate::ui::events::format_time(&s.updated_at);
            let name_col = if s.name.is_empty() {
                String::new()
            } else {
                format!("  [{}]", s.name)
            };
            println!(
                "  {}  {}  {}msgs  {}  {}{}",
                short_session_id(&s.id),
                time,
                s.messages.len(),
                s.model,
                last,
                name_col
            );
        }
        println!();
        println!("Use --session <id-or-name> to load a session by its ID prefix or name.");
    }
}

pub(crate) fn print_config(cli: &cli::Cli, cfg: &config::Config) -> io::Result<()> {
    let paths = crate::paths::process_paths().expect("startup must initialize application paths");
    let config_dir = paths.config_dir.clone();
    let data_dir = paths.data_dir.clone();
    let sessions_dir = paths.sessions_dir();
    let config_file = config::config_file_path();
    let mut output = String::new();

    let model = cli.resolve_model(cfg);
    let provider = cli.resolve_provider(cfg);
    let qm_map = config::quick_models_map(cfg);
    let max_tokens = cli.resolve_max_tokens(cfg);
    let max_agent_turns = cli.resolve_max_agent_turns(cfg);
    let context_window = cfg.resolve_context_window(&provider, &model, &qm_map);
    let temperature = config::resolve_temperature(cli, cfg, &model);
    let no_tools = cli.resolve_no_tools(cfg);
    let tools_allowlist = if cli.tools.is_empty() {
        "all".to_string()
    } else {
        cli.tools.join(",")
    };
    let no_context_files = cli.resolve_no_context_files(cfg);
    let sandbox = cli.resolve_sandbox(cfg);
    let sandbox_backend = cli.resolve_sandbox_backend(cfg);
    let shell = cli.resolve_shell(cfg);
    let sandbox_capabilities = Sandbox::new(
        sandbox && cli.general_sandbox_is_eligible(cfg),
        &sandbox_backend,
    )
    .with_shell(&shell)
    .with_windows_appcontainer_roots(
        cli.resolve_windows_appcontainer_read_roots(cfg),
        cli.resolve_windows_appcontainer_write_roots(cfg),
    )
    .capability_matrix();
    let edit_system = cli.resolve_edit_system(cfg);
    let compact = cfg.resolve_compact_enabled();

    let mode = if cli.dangerously_skip_permissions {
        "dangerously-skip-permissions"
    } else if cli.yolo || cfg.yolo.unwrap_or(false) {
        "yolo"
    } else if cli.accept_all || cfg.accept_all.unwrap_or(false) {
        "standard"
    } else if cli.read_only {
        "readonly"
    } else if cli.guarded {
        "guarded"
    } else if cli.restrictive || cfg.restrictive.unwrap_or(false) {
        "restrictive"
    } else {
        cfg.default_permission_mode.as_deref().unwrap_or("standard")
    };

    append_section(
        &mut output,
        "Directories",
        &[
            ("config", config_dir.display().to_string()),
            ("data", data_dir.display().to_string()),
            ("local data", paths.local_data_dir.display().to_string()),
            ("state", paths.state_dir.display().to_string()),
            ("cache", paths.cache_dir.display().to_string()),
            ("credentials", paths.credentials_dir.display().to_string()),
            ("sessions", sessions_dir.display().to_string()),
            (
                CHAT_HISTORY_FILE_LABEL,
                paths.chat_history_file().display().to_string(),
            ),
            ("config file", config_file.display().to_string()),
        ],
    );

    let mut model_entries = vec![
        ("provider", provider.to_string()),
        ("model", model.to_string()),
    ];
    if let Some(temp) = temperature {
        model_entries.push(("temperature", temp.to_string()));
    }
    append_section(&mut output, "Model", &model_entries);

    let fmt_opt = |v: Option<u64>| -> String {
        match v {
            Some(n) => n.to_string(),
            None => "— (no cap)".to_string(),
        }
    };

    #[cfg_attr(not(feature = "subagents"), allow(unused_mut))]
    let mut limit_entries: Vec<(&str, String)> = vec![
        chat_history_limit_entry(),
        ("max-tokens", max_tokens.to_string()),
        ("max-agent-turns", max_agent_turns.to_string()),
        ("context-window", context_window.to_string()),
        (
            "reserve-tokens",
            cfg.resolve_reserve_tokens(&model, &qm_map, context_window)
                .to_string(),
        ),
        ("max-read-lines", cfg.resolve_max_read_lines().to_string()),
        (
            "max-bash-output-lines",
            fmt_opt(cfg.resolve_max_bash_output_lines()),
        ),
        (
            "max-grep-results",
            cfg.resolve_max_grep_results().to_string(),
        ),
        (
            "max-find-results",
            cfg.resolve_max_find_results().to_string(),
        ),
        (
            "max-list-dir-entries",
            fmt_opt(cfg.resolve_max_list_dir_entries()),
        ),
    ];
    #[cfg(feature = "subagents")]
    {
        limit_entries.push((
            "subagent-max-read-lines",
            cfg.resolve_subagent_max_read_lines().to_string(),
        ));
        limit_entries.push((
            "subagent-max-grep-results",
            cfg.resolve_subagent_max_grep_results().to_string(),
        ));
        limit_entries.push((
            "subagent-max-find-results",
            cfg.resolve_subagent_max_find_results().to_string(),
        ));
        limit_entries.push((
            "subagent-max-list-dir-entries",
            fmt_opt(cfg.resolve_subagent_max_list_dir_entries()),
        ));
    }
    append_section(&mut output, "Limits", &limit_entries);

    append_section(
        &mut output,
        "Behavior",
        &[
            installed_build_entry(),
            chat_history_path_policy_entry(),
            ("permission-mode", mode.to_string()),
            ("shell", shell.to_string()),
            ("edit-system", edit_system.to_string()),
            ("sandbox", sandbox.to_string()),
            ("no-tools", no_tools.to_string()),
            ("tools", tools_allowlist),
            ("no-context-files", no_context_files.to_string()),
            ("compact", compact.to_string()),
        ],
    );

    append_section(
        &mut output,
        "Sandbox capabilities",
        &[
            ("backend", sandbox_capabilities.backend),
            ("status", sandbox_capabilities.status.to_string()),
            (
                "filesystem reads",
                sandbox_capabilities.filesystem_reads.to_string(),
            ),
            (
                "filesystem writes",
                sandbox_capabilities.filesystem_writes.to_string(),
            ),
            (
                "process namespace",
                sandbox_capabilities.process_namespace.to_string(),
            ),
            ("devices", sandbox_capabilities.devices.to_string()),
            ("environment", sandbox_capabilities.environment.to_string()),
            ("network", sandbox_capabilities.network.to_string()),
            (
                "requested network policy",
                sandbox_capabilities.requested_network_policy.to_string(),
            ),
        ],
    );

    append_section(
        &mut output,
        "JavaScript worker",
        &javascript_worker_entries(cli.tool_is_eligible(cfg, "js")),
    );

    #[cfg(feature = "advisor")]
    {
        let advisor_enabled = cli.resolve_advisor_enabled(cfg);
        let human_handoff = cli.resolve_advisor_human_handoff(cfg);
        let advisor_model = cli.resolve_advisor_model(cfg);
        let max_uses = cli
            .resolve_advisor_max_uses(cfg)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unlimited".to_string());
        append_section(
            &mut output,
            "Advisor",
            &[
                ("enabled", advisor_enabled.to_string()),
                ("model", advisor_model),
                ("human-handoff", human_handoff.to_string()),
                ("max-uses", max_uses),
                advisor_context_limit_entry(cli.resolve_advisor_kilobytes_limit(cfg)),
            ],
        );
    }

    write_output(io::stdout().lock(), &output)
}

#[cfg(test)]
mod tests {
    use std::io;

    use rig::OneOrMany;
    use rig::completion::{Message, Usage};
    use rig::message::AssistantContent;

    use super::{
        CHAT_HISTORY_FILE_LABEL, chat_history_limit_entry, chat_history_path_policy_entry,
        installed_build_entry, javascript_worker_compiled_entry, parse_git_status,
        render_headless_json, write_output,
    };

    #[tokio::test]
    async fn headless_turn_settles_registered_blocking_work_before_returning() {
        use std::time::Duration;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let mut turn = tokio::spawn(super::run_headless_turn(async move {
            std::mem::drop(crate::agent::runner::spawn_blocking_scoped(move || {
                let _ = started_tx.send(());
                // Dropping the sender during assertion unwinding also releases
                // the worker, so a failing negative control cannot hang Tokio.
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
            }));
            crate::agent::runner::HeadlessTurn {
                response: "done".to_owned(),
                usage: Usage::default(),
                interactions: Vec::new(),
                failure: None,
            }
        }));
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut turn)
                .await
                .is_err(),
            "headless result returned while registered blocking work was live"
        );
        release_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), turn)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.response, "done");
        assert!(result.failure.is_none());
    }

    struct BrokenPipeWriter;

    impl io::Write for BrokenPipeWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FlushBrokenPipeWriter;

    impl io::Write for FlushBrokenPipeWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    #[test]
    fn writing_config_output_treats_a_closed_pipe_as_success() {
        assert!(write_output(BrokenPipeWriter, CHAT_HISTORY_FILE_LABEL).is_ok());
    }

    #[test]
    fn flushing_config_output_treats_a_closed_pipe_as_success() {
        assert!(write_output(FlushBrokenPipeWriter, CHAT_HISTORY_FILE_LABEL).is_ok());
    }

    #[test]
    fn git_status_parser_preserves_nul_delimited_paths() {
        let parsed = parse_git_status(b" M src/main.rs\0?? path with spaces\nline.rs\0");
        assert_eq!(parsed.get("src/main.rs"), Some(b" M"));
        assert_eq!(parsed.get("path with spaces\nline.rs"), Some(b"??"));
    }

    #[test]
    fn headless_json_reports_tool_stats_files_usage_and_invocation_cost() {
        let root = std::env::temp_dir().join("mini-agent-headless-json-test");
        let absolute_edit = root.join("src/lib.rs").to_string_lossy().into_owned();
        let interactions = vec![
            Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::tool_call(
                    "read-1",
                    "read",
                    serde_json::json!({"path": "src/lib.rs"}),
                )),
            },
            Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::tool_call(
                    "edit-1",
                    "edit",
                    serde_json::json!({"path": absolute_edit}),
                )),
            },
            Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::tool_call(
                    "write-1",
                    "write",
                    serde_json::json!({"path": "./docs/report.md"}),
                )),
            },
        ];
        let usage = Usage {
            input_tokens: 1_000,
            output_tokens: 500,
            total_tokens: 1_500,
            cached_input_tokens: 100,
            cache_creation_input_tokens: 200,
            ..Usage::default()
        };

        let rendered = render_headless_json(
            &root,
            "finished",
            &interactions,
            usage,
            vec!["git-only.rs".to_string()],
            super::HeadlessPricing {
                anthropic_native: true,
                input_token_cost: 3.0,
                output_token_cost: 15.0,
            },
            super::HeadlessStopReason::Completed,
        )
        .expect("headless JSON should serialize");
        let value: serde_json::Value =
            serde_json::from_str(&rendered).expect("headless output should be one JSON value");

        assert_eq!(value["result"], "finished");
        assert_eq!(value["stop_reason"], "completed");
        assert_eq!(value["tool_calls"]["total"], 3);
        assert_eq!(value["tool_calls"]["by_name"]["edit"], 1);
        assert_eq!(value["tool_calls"]["by_name"]["read"], 1);
        assert_eq!(value["tool_calls"]["by_name"]["write"], 1);
        assert_eq!(value["usage"]["total_tokens"], 1_500);
        assert_eq!(
            value["files_changed"],
            serde_json::json!(["docs/report.md", "git-only.rs", "src/lib.rs"])
        );
        assert!((value["cost"].as_f64().unwrap() - 0.01128).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn workspace_change_snapshot_reports_only_invocation_changes() {
        let root = std::env::temp_dir().join(format!(
            "mini-agent-headless-change-snapshot-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let runner = crate::git::runner::GitRunner::discover().expect("Git should be available");
        runner
            .run(
                &root,
                "headless-output-test-init",
                ["init", "--quiet"],
                crate::git::runner::LOCAL_MUTATION_LIMITS,
            )
            .await
            .expect("temporary repository should initialize");

        let clean = super::capture_workspace_change_baseline(&root).await;
        std::fs::write(root.join("new file.rs"), "one").unwrap();
        assert_eq!(
            super::files_changed_since(&root, clean.as_ref(), &[]).await,
            ["new file.rs"]
        );

        let already_dirty = super::capture_workspace_change_baseline(&root).await;
        assert!(
            super::files_changed_since(&root, already_dirty.as_ref(), &[])
                .await
                .is_empty(),
            "an unchanged pre-existing dirty path is not attributed to this run"
        );
        std::fs::write(root.join("new file.rs"), "longer content").unwrap();
        assert_eq!(
            super::files_changed_since(&root, already_dirty.as_ref(), &[]).await,
            ["new file.rs"],
            "metadata detects another edit to a path that was already dirty"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "advisor")]
    #[test]
    fn config_reports_advisor_limits_without_overflow_across_the_u32_range() {
        for (kilobytes, expected) in [
            (0, "0 KB (0 head / 0 tail)"),
            (256, "256 KB (131072 head / 131072 tail)"),
            (4_194_304, "4194304 KB (2147483648 head / 2147483648 tail)"),
            (
                u32::MAX,
                "4294967295 KB (2199023255040 head / 2199023255040 tail)",
            ),
        ] {
            assert_eq!(
                super::advisor_context_limit_entry(kilobytes),
                ("context-limit", expected.to_owned())
            );
        }
    }

    #[test]
    fn config_reports_the_installed_package_version_and_build_target() {
        assert_eq!(
            installed_build_entry(),
            (
                "installed binary provenance",
                format!(
                    "Cargo-installed package {} version {} is runnable; debug assertions {} at compile time; target {}-{}",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION"),
                    if cfg!(debug_assertions) {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    std::env::consts::ARCH,
                    std::env::consts::OS,
                )
            )
        );
    }

    #[test]
    fn config_reports_the_production_chat_history_entry_limit() {
        assert_eq!(
            chat_history_limit_entry(),
            (
                "chat history entry limit",
                "bounded retention keeps newest 10000 entries and discards older entries"
                    .to_string()
            )
        );
    }

    #[test]
    fn config_reports_the_production_chat_history_path_policy() {
        assert_eq!(
            chat_history_path_policy_entry(),
            (
                "chat history path policy",
                "reject symlinked path components".to_string()
            )
        );
    }

    #[test]
    fn config_reports_whether_javascript_was_compiled() {
        assert_eq!(
            javascript_worker_compiled_entry(),
            ("compiled", cfg!(feature = "js").to_string())
        );
    }

    #[cfg(not(feature = "js"))]
    #[test]
    fn config_reports_complete_not_compiled_javascript_worker_section() {
        assert_eq!(
            super::javascript_worker_entries(false),
            vec![
                ("compiled", "false".to_string()),
                ("status", "not compiled".to_string()),
            ]
        );
    }

    #[cfg(feature = "js")]
    #[test]
    fn config_skips_worker_probe_when_javascript_is_not_requested() {
        let entries = super::javascript_worker_entries(false)
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();

        assert_eq!(entries["status"], "unavailable");
        assert_eq!(entries["reason"], "JavaScript tool was not requested");
    }

    #[cfg(feature = "js")]
    #[test]
    fn config_reports_fail_closed_worker_authority_and_windows_spawn_fence() {
        use crate::sandbox::worker::{
            WorkerBackend, WorkerContainmentAssurance, WorkerContainmentStatus,
        };

        let entries =
            super::javascript_worker_entries_for_status(WorkerContainmentStatus::Available {
                backend: WorkerBackend::WindowsLpac,
                assurance: WorkerContainmentAssurance::Enforced,
            })
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();

        assert_eq!(entries["compiled"], "true");
        assert_eq!(entries["backend"], "windows-lpac");
        assert_eq!(entries["status"], "available");
        assert_eq!(
            entries["filesystem authority"],
            "none ambient; parent broker only"
        );
        assert_eq!(
            entries["network authority"],
            "none ambient; parent broker only"
        );
        assert_eq!(entries["process creation"], "denied in worker");
        assert_eq!(entries["native process limit"], "1 worker process");
        assert_eq!(entries["native memory limit"], "256 MiB");
        assert_eq!(entries["native CPU limit"], "35 seconds");
        assert_eq!(entries["parent-brokered spawn"], "disabled on Windows");
        assert_eq!(
            entries["protocol version"],
            crate::extras::js::protocol::PROTOCOL_VERSION.to_string()
        );
    }

    #[cfg(feature = "js")]
    #[test]
    fn config_reports_unavailable_reason_and_weaker_macos_assurance() {
        use crate::sandbox::worker::{
            WorkerBackend, WorkerContainmentAssurance, WorkerContainmentStatus,
        };

        let entries =
            super::javascript_worker_entries_for_status(WorkerContainmentStatus::Unavailable {
                backend: WorkerBackend::Seatbelt,
                assurance: WorkerContainmentAssurance::DeprecatedBestEffort,
                reason: "unsupported macOS version".into(),
            })
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();

        assert_eq!(entries["status"], "unavailable");
        assert_eq!(entries["reason"], "unsupported macOS version");
        assert_eq!(
            entries["assurance"],
            "deprecated weaker best-effort backend class; inactive"
        );
        assert_eq!(
            entries["worker launch"],
            "disabled; no worker process starts"
        );
        assert_eq!(
            entries["filesystem authority"],
            "not granted; no worker process"
        );
        assert_eq!(
            entries["native memory limit"],
            "not active; no worker process"
        );
    }

    #[cfg(feature = "js")]
    #[test]
    fn config_reports_available_macos_limits_and_weaker_assurance() {
        use crate::sandbox::worker::{
            WorkerBackend, WorkerContainmentAssurance, WorkerContainmentStatus,
        };

        let entries =
            super::javascript_worker_entries_for_status(WorkerContainmentStatus::Available {
                backend: WorkerBackend::Seatbelt,
                assurance: WorkerContainmentAssurance::DeprecatedBestEffort,
            })
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();

        assert_eq!(entries["status"], "available");
        assert_eq!(entries["assurance"], "deprecated weaker best-effort");
        assert_eq!(
            entries["native memory limit"],
            "at most 40 GiB virtual address space"
        );
        assert_eq!(entries["native CPU limit"], "at most 35 seconds");
    }
}
