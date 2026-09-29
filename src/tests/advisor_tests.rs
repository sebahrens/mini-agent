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
    for (head, tail, expected) in [(
        exact.as_str(),
        exact.as_str(),
        format!("[User]: {exact}\n\n[... conversation omitted ...]\n\n[User]: {exact}"),
    )] {
        let msgs = [
            msg(MessageRole::User, head),
            msg(MessageRole::Assistant, "omitted middle"),
            msg(MessageRole::User, tail),
        ];
        assert_eq!(format_conversation(&msgs, 1), expected);
    }
    // One byte over the side budget is truncated at a UTF-8 boundary, not dropped.
    let rendered = format_conversation(
        &[
            msg(MessageRole::User, &too_large),
            msg(MessageRole::Assistant, "omitted middle"),
            msg(MessageRole::User, &exact),
        ],
        1,
    );
    assert!(rendered.starts_with("[User]: 界"), "{rendered}");
    assert!(rendered.contains("bytes omitted ...]"), "{rendered}");
    assert!(rendered.ends_with(&format!(
        "\n\n[... conversation omitted ...]\n\n[User]: {exact}"
    )));
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

#[test]
fn format_conversation_truncates_an_oversized_newest_or_oldest_message_instead_of_dropping_it() {
    // A 1-KB budget gives each side 512 bytes.
    let newest = format!("NEWEST-START {} NEWEST-END", "x".repeat(4000));
    let oldest = format!("TASK-START {} TASK-END", "界".repeat(2000));
    let msgs = [
        msg(MessageRole::User, &oldest),
        msg(MessageRole::Assistant, "omitted middle"),
        msg(MessageRole::ToolResult, &newest),
    ];
    let rendered = format_conversation(&msgs, 1);
    let (head, tail) = rendered
        .split_once("\n\n[... conversation omitted ...]\n\n")
        .expect("the middle is omitted");
    assert!(head.starts_with("[User]: TASK-START"), "{head}");
    assert!(head.ends_with("TASK-END"), "{head}");
    assert!(head.contains("bytes omitted ...]"), "{head}");
    assert!(head.len() <= 512, "{}", head.len());
    assert!(tail.starts_with("[ToolResult]: NEWEST-START"), "{tail}");
    assert!(tail.ends_with("NEWEST-END"), "{tail}");
    assert!(tail.len() <= 512, "{}", tail.len());
    assert!(!rendered.contains("omitted middle"));

    // A single oversized message appears once.
    let only = [msg(MessageRole::User, &newest)];
    let rendered = format_conversation(&only, 1);
    assert_eq!(rendered.matches("NEWEST-END").count(), 1, "{rendered}");
    assert!(rendered.len() <= 512);

    // A budget too small for the marker still omits rather than overflows.
    assert_eq!(
        format_conversation(&only, 0),
        "\n\n[... conversation omitted ...]\n\n"
    );
}
