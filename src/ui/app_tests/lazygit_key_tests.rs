use super::*;

#[test]
fn lazygit_is_ctrl_o_and_ctrl_h_stays_backspace() {
    assert!(is_lazygit_key(KeyEvent::new(
        KeyCode::Char('o'),
        KeyModifiers::CONTROL
    )));
    for key in [
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('\u{8}'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
    ] {
        assert!(!is_lazygit_key(key), "{key:?}");
    }
    // The shared idle helper preserves Ctrl+D shell-style behavior.
    assert_eq!(idle_interrupt(false, false), IdleInterrupt::ClearDraft);
    assert_eq!(idle_interrupt(true, false), IdleInterrupt::EditorKey);
    assert_eq!(idle_interrupt(false, true), IdleInterrupt::Exit);
    assert_eq!(idle_interrupt(true, true), IdleInterrupt::Exit);
    assert_eq!(
        ctrl_c_action(false, InterruptTarget::MainRun, true),
        CtrlCAction::Interrupt(InterruptTarget::MainRun)
    );
    assert_eq!(
        ctrl_c_action(false, InterruptTarget::Exit, false),
        CtrlCAction::ClearDraft
    );
    assert_eq!(
        ctrl_c_action(false, InterruptTarget::Exit, true),
        CtrlCAction::ArmExit
    );
    assert_eq!(
        ctrl_c_action(true, InterruptTarget::Exit, true),
        CtrlCAction::Exit
    );
    assert_eq!(
        ctrl_c_feedback(CtrlCAction::Interrupt(InterruptTarget::MainRun)),
        "interrupt requested; press Ctrl-C again to exit"
    );
    assert_eq!(
        ctrl_c_feedback(CtrlCAction::Exit),
        "exit requested; shutting down..."
    );
    let mut input = InputEditor::new();
    input.load_text("draft");
    input.discard_draft();
    assert_eq!(input.buffer, "");
    input.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
    assert_eq!(input.buffer, "draft", "Ctrl+Y recovers a cleared draft");
    input.set_cursor(0);
    input.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(input.buffer, "raft", "Ctrl+D deletes forward");

    // Nothing is warmed after /quit or /exit (mini-agent-bgg91).
    assert!(is_quit_command("/quit"));
    assert!(is_quit_command(" /exit now"));
    assert!(!is_quit_command("/quiet"));
    assert!(!is_quit_command("/model x"));

    // Ctrl+H reaches the editor as backspace.
    let mut input = InputEditor::new();
    input.load_text("ab");
    assert_eq!(
        input.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(input.buffer, "a");
}
