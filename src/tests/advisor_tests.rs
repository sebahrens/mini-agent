use crate::extras::advisor::format_conversation;
use crate::session::{MessageRole, SessionMessage};

fn msg(role: MessageRole, content: &str) -> SessionMessage {
    SessionMessage {
        role,
        content: content.into(),
        estimated_tokens: 0,
        tool_call_id: None,
        tool: None,
    }
}

#[test]
fn format_conversation_empty_and_complete_transcripts() {
    assert_eq!(format_conversation(&[], 0), "");
    assert_eq!(
        format_conversation(&[msg(MessageRole::User, "hello")], 1),
        "[User]: hello"
    );
    let msgs = [
        msg(MessageRole::User, "hello"),
        msg(MessageRole::Assistant, "hi there"),
    ];
    assert_eq!(
        format_conversation(&msgs, 1),
        "[User]: hello\n\n[Assistant]: hi there"
    );
    assert_eq!(
        format_conversation(&msgs, 0),
        "\n\n[... conversation omitted ...]\n\n"
    );
}

#[test]
fn format_conversation_retains_real_head_and_tail_at_utf8_byte_boundary() {
    // The 1-KB budget gives each side 512 bytes. The User prefix is eight bytes.
    let exact = "界".repeat(168);
    let too_large = format!("{exact}x");
    for (head, tail, expected) in [
        (
            exact.as_str(),
            exact.as_str(),
            format!("[User]: {exact}\n\n[... conversation omitted ...]\n\n[User]: {exact}"),
        ),
        (
            too_large.as_str(),
            exact.as_str(),
            format!("\n\n[... conversation omitted ...]\n\n[User]: {exact}"),
        ),
        (
            exact.as_str(),
            too_large.as_str(),
            format!("[User]: {exact}\n\n[... conversation omitted ...]\n\n"),
        ),
    ] {
        let msgs = [
            msg(MessageRole::User, head),
            msg(MessageRole::Assistant, "omitted middle"),
            msg(MessageRole::User, tail),
        ];
        assert_eq!(format_conversation(&msgs, 1), expected);
    }
}

#[test]
fn format_conversation_overlapping_head_and_tail_emit_each_message_once() {
    // Each side fits two messages (412 bytes), but all three exceed 512 bytes.
    let a = "a".repeat(197);
    let b = "b".repeat(197);
    let c = "c".repeat(197);
    let msgs = [
        msg(MessageRole::User, &a),
        msg(MessageRole::User, &b),
        msg(MessageRole::User, &c),
    ];
    assert_eq!(
        format_conversation(&msgs, 1),
        format!("[User]: {a}\n\n[User]: {b}\n\n[User]: {c}")
    );
}
