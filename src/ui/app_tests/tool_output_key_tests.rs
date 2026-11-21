use super::*;

#[test]
fn ctrl_t_folds_tool_output_and_plain_t_types() {
    assert!(is_tool_output_key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL
    )));
    for key in [
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT),
        KeyEvent::new(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ),
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
    ] {
        assert!(!is_tool_output_key(key), "{key:?}");
    }
    // Ctrl+T must not collide with the lazygit key.
    assert!(!is_lazygit_key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL
    )));
}
