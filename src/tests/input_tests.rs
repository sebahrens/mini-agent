use crate::ui::input::InputEditor;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn type_str(editor: &mut InputEditor, s: &str) {
    for c in s.chars() {
        editor.handle_key(press(KeyCode::Char(c)));
    }
}

#[test]
fn typing_ascii_keeps_cursor_in_sync() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello");
    assert_eq!(editor.buffer.as_str(), "hello");
    assert_eq!(editor.cursor, 5);
}

#[test]
fn typing_multibyte_chars_does_not_panic() {
    // Regression for bug where `cursor += 1` (char step) was used with
    // `CompactString::insert(byte_idx, ch)` (byte boundary required).
    // Two Norwegian characters in a row were enough to trigger a panic.
    let mut editor = InputEditor::new();
    type_str(&mut editor, "på "); // used to panic on the space after 'å'
    assert_eq!(editor.buffer.as_str(), "på ");
    assert_eq!(editor.cursor, editor.buffer.len()); // cursor in bytes
}

#[test]
fn typing_mixed_ascii_and_multibyte() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hei på deg så fin dag æøå");
    assert_eq!(editor.buffer.as_str(), "hei på deg så fin dag æøå");
    assert_eq!(editor.cursor, editor.buffer.len());
}

#[test]
fn backspace_after_multibyte_does_not_panic() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "å");
    editor.handle_key(press(KeyCode::Backspace));
    assert_eq!(editor.buffer.as_str(), "");
    assert_eq!(editor.cursor, 0);
}

#[test]
fn left_arrow_steps_one_char_not_one_byte() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "aåb");
    // cursor is after 'b', byte-idx 4 (a=1 + å=2 + b=1)
    assert_eq!(editor.cursor, 4);
    editor.handle_key(press(KeyCode::Left));
    // after 'å' → byte-idx 3
    assert_eq!(editor.cursor, 3);
    editor.handle_key(press(KeyCode::Left));
    // after 'a' → byte-idx 1 (skips the 2 bytes of 'å')
    assert_eq!(editor.cursor, 1);
}

#[test]
fn right_arrow_steps_one_char_not_one_byte() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "aåb");
    editor.cursor = 0;
    editor.handle_key(press(KeyCode::Right));
    assert_eq!(editor.cursor, 1); // after 'a'
    editor.handle_key(press(KeyCode::Right));
    assert_eq!(editor.cursor, 3); // after 'å' (skipped 2 bytes)
}

#[test]
fn enter_returns_buffer_and_resets() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hei på");
    let out = editor.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(out.as_str(), "hei på");
    assert_eq!(editor.cursor, 0);
    assert_eq!(editor.buffer.as_str(), "");
}

#[test]
fn semantic_interrupt_routing_preserves_btw_and_validation_isolation() {
    use crate::ui::{InterruptTarget, interrupt_target};

    assert_eq!(interrupt_target(1, true, true), InterruptTarget::Btw);
    assert_eq!(interrupt_target(0, true, true), InterruptTarget::Validation);
    assert_eq!(interrupt_target(0, false, true), InterruptTarget::MainRun);
    assert_eq!(interrupt_target(0, false, false), InterruptTarget::Exit);
}

#[test]
fn clipboard_shortcuts_precede_interrupt_and_literal_input_routing() {
    use crate::ui::{ClipboardShortcut, clipboard_shortcut};

    let copy = KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let paste = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
    let altgr_paste = KeyEvent::new(
        KeyCode::Char('v'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    );
    let modified_copy = KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::ALT,
    );

    assert_eq!(
        clipboard_shortcut(copy, true),
        Some(ClipboardShortcut::CopySelection)
    );
    assert_eq!(clipboard_shortcut(copy, false), None);
    assert_eq!(clipboard_shortcut(interrupt, true), None);
    assert_eq!(
        clipboard_shortcut(paste, true),
        Some(ClipboardShortcut::Paste)
    );
    assert_eq!(clipboard_shortcut(paste, false), None);
    assert_eq!(clipboard_shortcut(altgr_paste, true), None);
    assert_eq!(clipboard_shortcut(modified_copy, true), None);
}

#[test]
fn clipboard_paste_payload_is_inserted_once_at_the_cursor() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "ab");
    editor.cursor = 1;

    editor.handle_paste("☃\r\nline".to_string());

    assert_eq!(editor.buffer.as_str(), "a☃\r\nlineb");
    assert_eq!(editor.cursor, "a☃\r\nline".len());
}
