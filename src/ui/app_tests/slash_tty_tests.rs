use super::slash_command_needs_tty;

#[test]
fn slash_command_needs_tty_matches_only_stdin_consumers() {
    assert!(slash_command_needs_tty("/tutor"));
    assert!(slash_command_needs_tty("/login openai"));
    assert!(slash_command_needs_tty("/init"));
    assert!(slash_command_needs_tty("  /init  "));
    assert!(!slash_command_needs_tty("/init force"));
    assert!(!slash_command_needs_tty("/undo"));
    assert!(!slash_command_needs_tty("/memory editor"));
    assert!(!slash_command_needs_tty(""));
}
