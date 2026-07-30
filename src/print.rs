use std::fmt::Write as FmtWrite;
use std::io::{self, Write as IoWrite};

use crate::cli;
use crate::config;
use crate::session;

const CHAT_HISTORY_FILE_LABEL: &str = "chat history file";
const CHAT_HISTORY_ENTRY_LIMIT_LABEL: &str = "chat history entry limit";
const CHAT_HISTORY_PATH_POLICY_LABEL: &str = "chat history path policy";

fn chat_history_limit_entry() -> (&'static str, String) {
    (
        CHAT_HISTORY_ENTRY_LIMIT_LABEL,
        format!(
            "{} entries (bounded retention)",
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
                &s.id[..8],
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
    let shell = cli.resolve_shell(cfg);
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
            cfg.resolve_reserve_tokens(&model, &qm_map).to_string(),
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
                (
                    "context-limit",
                    format!(
                        "{} KB ({} head / {} tail)",
                        cli.resolve_advisor_kilobytes_limit(cfg),
                        cli.resolve_advisor_kilobytes_limit(cfg) * 1024 / 2,
                        cli.resolve_advisor_kilobytes_limit(cfg) * 1024 / 2,
                    ),
                ),
            ],
        );
    }

    write_output(io::stdout().lock(), &output)
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{
        CHAT_HISTORY_FILE_LABEL, chat_history_limit_entry, chat_history_path_policy_entry,
        write_output,
    };

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
    fn config_reports_the_production_chat_history_entry_limit() {
        assert_eq!(
            chat_history_limit_entry(),
            (
                "chat history entry limit",
                "10000 entries (bounded retention)".to_string()
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
}
