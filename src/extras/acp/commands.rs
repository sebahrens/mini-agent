//! Local ACP commands are recognized only in one original text block.
//! Rendered resources, images, and model/tool text cannot become command authority.
use agent_client_protocol::schema::v1::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Command {
    Compact,
    Undo,
    Rename(String),
    #[cfg(feature = "goal")]
    Goal(String),
}

pub(super) fn classify(
    blocks: &[ContentBlock],
) -> Result<Option<Command>, agent_client_protocol::Error> {
    let [ContentBlock::Text(text)] = blocks else {
        return Ok(None);
    };
    let (name, arguments) = text
        .text
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((text.text.trim(), ""));
    let arguments = arguments.trim();
    Ok(match name {
        "/compact" | "/undo" => {
            if !arguments.is_empty() {
                return Err(agent_client_protocol::Error::new(
                    -32602,
                    "this ACP command takes no arguments",
                ));
            }
            Some(if name == "/compact" {
                Command::Compact
            } else {
                Command::Undo
            })
        }
        "/rename" => {
            if arguments.is_empty()
                || arguments.chars().count() > 200
                || arguments.chars().any(char::is_control)
            {
                return Err(agent_client_protocol::Error::new(
                    -32602,
                    "ACP session name must contain 1–200 non-control characters",
                ));
            }
            Some(Command::Rename(arguments.to_owned()))
        }
        #[cfg(feature = "goal")]
        "/goal" => Some(Command::Goal(arguments.to_owned())),
        _ => None,
    })
}

/// The arguments of a `/cron` command in the original sole text block, or
/// `None` for any other prompt. `/cron` only edits the session's schedule
/// store (see `super::cron`); it is answered without registering a turn, so
/// it is kept out of [`Command`], whose variants all run inside one.
pub(super) fn cron_arguments(blocks: &[ContentBlock]) -> Option<String> {
    let [ContentBlock::Text(text)] = blocks else {
        return None;
    };
    let text = text.text.trim();
    let (name, arguments) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    (name == "/cron").then(|| arguments.trim().to_owned())
}

/// Name and arguments of a `/name args` invocation in the original sole text
/// block, or `None` for any other prompt (rendered resources, images, several
/// blocks and model/tool text never become command authority).
pub(super) fn invocation(blocks: &[ContentBlock]) -> Option<(String, String)> {
    let [ContentBlock::Text(text)] = blocks else {
        return None;
    };
    let text = text.text.trim();
    let rest = text.strip_prefix('/')?;
    let (name, arguments) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (!name.is_empty()).then(|| (name.to_owned(), arguments.trim().to_owned()))
}

/// Expands a user-defined prompt command (`.mini-agent/commands`,
/// `.claude/commands`, global) for the session workspace, with the TUI's
/// trust rules and limits (64 KiB arguments and templates, 8 includes, 256 KiB
/// expansion). `Ok(None)` when no such command exists. The template's `mode`
/// and `model` are not applied over ACP: the editor owns those selections.
/// Includes need existing read permission; an approval-required read fails
/// closed because the expansion has no interactive prompt.
pub(super) async fn expand_custom(
    cli: &crate::cli::Cli,
    cfg: &crate::config::Config,
    workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
    blocks: &[ContentBlock],
) -> Result<Option<String>, agent_client_protocol::Error> {
    let Some((name, arguments)) = invocation(blocks) else {
        return Ok(None);
    };
    let (cli, cfg) = (cli.clone(), cfg.clone());
    let outcome = tokio::task::spawn_blocking(move || {
        let commands = crate::context::commands::load_for_workspace(&workspace);
        let Some(template) = commands.get(&name) else {
            return Ok(None);
        };
        let permission = if template.has_includes() {
            let authority = crate::permission::resolve_configured_execution_authority(&cli, &cfg)
                .map_err(|error| error.to_string())?
                .0;
            crate::permission::build_noninteractive_permission_with_cli(&cli, &cfg, authority)
                .map_err(|error| error.to_string())?
                .0
        } else {
            None
        };
        template
            .expand(&arguments, &workspace, &permission)
            .map(Some)
    })
    .await
    .map_err(|_| agent_client_protocol::Error::new(-32603, "custom command task failed"))?;
    outcome.map_err(|error| {
        agent_client_protocol::Error::new(-32602, format!("custom command: {error}"))
    })
}

pub(super) async fn announce(
    state: &super::AcpState,
    id: &SessionId,
    cx: &agent_client_protocol::ConnectionTo<agent_client_protocol::Client>,
) -> Result<(), agent_client_protocol::Error> {
    let workspace = state
        .sessions
        .lock()
        .await
        .get(id)
        .map(|session| session.workspace.clone());
    let custom = match workspace {
        Some(workspace) => tokio::task::spawn_blocking(move || {
            crate::context::commands::load_for_workspace(&workspace)
                .into_iter()
                .map(|(name, template)| (name, template.description, template.argument_hint))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default(),
        None => Vec::new(),
    };
    let mut commands = vec![
        AvailableCommand::new("compact", "Summarize the completed session context"),
        AvailableCommand::new(
            "undo",
            "Remove the last conversation turn; files and billed usage remain",
        ),
        AvailableCommand::new("rename", "Rename this saved session"),
        AvailableCommand::new(
            "cron",
            "Repeat a prompt on an interval while this session is idle: /cron <interval> <prompt>, /cron list, /cron stop <id>|all",
        ),
    ];
    #[cfg(feature = "goal")]
    commands.push(AvailableCommand::new(
        "goal",
        "Show, clear or create the session objective",
    ));
    #[cfg(not(feature = "goal"))]
    let _ = &mut commands;
    for (name, description, hint) in custom {
        let description = if description.is_empty() {
            "Custom prompt command".to_owned()
        } else {
            description
        };
        let mut command = AvailableCommand::new(name, description);
        if let Some(hint) = hint {
            command = command.input(AvailableCommandInput::Unstructured(
                UnstructuredCommandInput::new(hint),
            ));
        }
        commands.push(command);
    }
    cx.send_notification(SessionNotification::new(
        id.clone(),
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(commands)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acp53_local_commands_require_the_original_sole_text_block() {
        assert_eq!(
            classify(&[ContentBlock::Text(TextContent::new(
                "/rename literal $HOME @file"
            ))])
            .unwrap(),
            Some(Command::Rename("literal $HOME @file".into()))
        );
        assert_eq!(
            classify(&[
                ContentBlock::Text(TextContent::new("/undo")),
                ContentBlock::Text(TextContent::new("data"))
            ])
            .unwrap(),
            None
        );
        assert_eq!(
            classify(&[ContentBlock::ResourceLink(ResourceLink::new(
                "/undo",
                "file:///data"
            ))])
            .unwrap(),
            None
        );
        assert_eq!(
            classify(&[ContentBlock::Image(ImageContent::new(
                "L3VuZG8=",
                "image/png"
            ))])
            .unwrap(),
            None
        );
        assert!(classify(&[ContentBlock::Text(TextContent::new("/undo extra"))]).is_err());
        assert!(classify(&[ContentBlock::Text(TextContent::new("/rename x\u{1}"))]).is_err());
        assert_eq!(
            classify(&[ContentBlock::Text(TextContent::new("/unknown"))]).unwrap(),
            None
        );
    }

    #[test]
    fn cron_arguments_come_only_from_the_original_sole_text_block() {
        let text = |value: &str| ContentBlock::Text(TextContent::new(value));
        assert_eq!(
            cron_arguments(&[text("  /cron 5m  check CI ")]),
            Some("5m  check CI".into())
        );
        assert_eq!(cron_arguments(&[text("/cron")]), Some(String::new()));
        assert_eq!(cron_arguments(&[text("/cronjob")]), None);
        assert_eq!(cron_arguments(&[text("please /cron 5m x")]), None);
        assert_eq!(cron_arguments(&[text("/cron 5m x"), text("more")]), None);
        assert_eq!(
            cron_arguments(&[ContentBlock::ResourceLink(ResourceLink::new(
                "/cron 5m x",
                "file:///data"
            ))]),
            None
        );
        assert_eq!(classify(&[text("/cron 5m x")]).unwrap(), None);
    }

    #[test]
    fn custom_command_invocation_requires_the_original_sole_text_block() {
        let text = |value: &str| ContentBlock::Text(TextContent::new(value));
        assert_eq!(
            invocation(&[text("  /fix-issue 12  extra ")]),
            Some(("fix-issue".into(), "12  extra".into()))
        );
        assert_eq!(
            invocation(&[text("/deploy:ship")]),
            Some(("deploy:ship".into(), String::new()))
        );
        assert_eq!(invocation(&[text("/")]), None);
        assert_eq!(invocation(&[text("please /fix-issue 1")]), None);
        assert_eq!(invocation(&[text("/fix-issue 1"), text("more")]), None);
        assert_eq!(
            invocation(&[ContentBlock::ResourceLink(ResourceLink::new(
                "/fix-issue 1",
                "file:///data"
            ))]),
            None
        );
    }

    #[tokio::test]
    async fn unknown_custom_commands_and_plain_prompts_are_left_untouched() {
        let root = crate::tests::OwnedTestDirectory::new("acp-custom-unknown");
        let workspace =
            std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let cli = <crate::cli::Cli as clap::Parser>::parse_from(["mini-agent"]);
        let cfg = crate::config::Config::default();
        let text = |value: &str| vec![ContentBlock::Text(TextContent::new(value))];
        for prompt in ["/zz-no-such-command 1", "hello", "/tmp/file.txt"] {
            assert_eq!(
                expand_custom(&cli, &cfg, workspace.clone(), &text(prompt))
                    .await
                    .unwrap(),
                None,
                "{prompt}"
            );
        }
    }
}
