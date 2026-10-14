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
}
