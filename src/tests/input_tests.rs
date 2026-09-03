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

// --- byte-offset regressions: Ctrl+U / Ctrl+K / Alt+Y with multi-byte text ---

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn alt(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}

#[test]
fn ctrl_u_deletes_to_line_start_with_cjk_text() {
    // "日本語 test": cursor placed after the space (byte 10, char 4).
    let mut editor = InputEditor::new();
    type_str(&mut editor, "日本語 test");
    for _ in 0..4 {
        editor.handle_key(press(KeyCode::Left));
    }
    assert_eq!(editor.cursor, "日本語 ".len());
    editor.handle_key(ctrl('u'));
    // Char-based slicing took only 10 *chars* and deleted everything.
    assert_eq!(editor.buffer.as_str(), "test");
    assert_eq!(editor.cursor, 0);
    // The killed text is yankable and intact.
    editor.handle_key(ctrl('y'));
    assert_eq!(editor.buffer.as_str(), "日本語 test");
    assert_eq!(editor.cursor, "日本語 ".len());
}

#[test]
fn ctrl_u_with_emoji_before_cursor() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "🦀🦀 rust");
    for _ in 0..4 {
        editor.handle_key(press(KeyCode::Left));
    }
    editor.handle_key(ctrl('u'));
    assert_eq!(editor.buffer.as_str(), "rust");
    assert_eq!(editor.cursor, 0);
}

#[test]
fn ctrl_k_deletes_to_end_with_cjk_text() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "日本語 test");
    for _ in 0..4 {
        editor.handle_key(press(KeyCode::Left));
    }
    editor.handle_key(ctrl('k'));
    // Char-based `skip(cursor)` skipped 10 chars and deleted nothing while
    // `take(cursor)` kept the whole buffer.
    assert_eq!(editor.buffer.as_str(), "日本語 ");
    assert_eq!(editor.cursor, "日本語 ".len());
    editor.handle_key(ctrl('y'));
    assert_eq!(editor.buffer.as_str(), "日本語 test");
}

#[test]
fn ctrl_k_with_emoji_after_cursor() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "go 🦀🦀");
    editor.handle_key(press(KeyCode::Home));
    editor.handle_key(press(KeyCode::Right));
    editor.handle_key(press(KeyCode::Right));
    editor.handle_key(ctrl('k'));
    assert_eq!(editor.buffer.as_str(), "go");
    assert_eq!(editor.cursor, 2);
}

#[test]
fn alt_y_rotates_kill_ring_with_multibyte_text() {
    let mut editor = InputEditor::new();
    // Kill "日本語" (Ctrl+W), then "abc" (Ctrl+W) -> ring = [abc, 日本語].
    type_str(&mut editor, "日本語");
    editor.handle_key(ctrl('w'));
    type_str(&mut editor, "abc");
    editor.handle_key(ctrl('w'));
    assert_eq!(editor.buffer.as_str(), "");

    type_str(&mut editor, "é ");
    editor.handle_key(ctrl('y'));
    assert_eq!(editor.buffer.as_str(), "é abc");
    // Alt+Y replaces the yanked text with the older kill entry. With the old
    // char-based slicing the 'é' prefix shifted the removal window by a byte.
    editor.handle_key(alt('y'));
    assert_eq!(editor.buffer.as_str(), "é 日本語");
    assert_eq!(editor.cursor, editor.buffer.len());
    // And rotating again brings the first entry back, intact.
    editor.handle_key(alt('y'));
    assert_eq!(editor.buffer.as_str(), "é abc");
    assert_eq!(editor.cursor, editor.buffer.len());
}

#[test]
fn alt_y_after_yanking_multibyte_text_mid_buffer() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "🦀🦀");
    editor.handle_key(ctrl('w'));
    type_str(&mut editor, "日本");
    editor.handle_key(ctrl('w'));
    type_str(&mut editor, "[]");
    editor.handle_key(press(KeyCode::Left));
    editor.handle_key(ctrl('y'));
    assert_eq!(editor.buffer.as_str(), "[日本]");
    editor.handle_key(alt('y'));
    assert_eq!(editor.buffer.as_str(), "[🦀🦀]");
    assert_eq!(editor.cursor, "[🦀🦀".len());
}
