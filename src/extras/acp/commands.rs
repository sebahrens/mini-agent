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

pub(super) fn announce(
    id: &SessionId,
    cx: &agent_client_protocol::ConnectionTo<agent_client_protocol::Client>,
) -> Result<(), agent_client_protocol::Error> {
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
}
