use crate::ui::input::{InputEditor, Picker};
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
fn tab_opens_finite_argument_picker_after_paste_or_history_recall() {
    let mut editor = InputEditor::new();
    editor.load_text("/permissions ma");

    editor.handle_key(press(KeyCode::Tab));

    assert_eq!(editor.buffer.as_str(), "/permissions ma");
    match editor.picker.as_ref() {
        Some(Picker::Prefixed(picker, prefix)) => {
            assert_eq!(*prefix, "/permissions ");
            assert_eq!(picker.matches().collect::<Vec<_>>(), ["manual"]);
        }
        _ => panic!("Tab should open the permissions argument picker"),
    }
}

#[test]
fn command_picker_hands_permissions_to_its_argument_picker() {
    let mut editor = InputEditor::new();
    editor.handle_key(press(KeyCode::Char('/')));
    for character in "permissions".chars() {
        assert!(editor.handle_picker_key(press(KeyCode::Char(character))));
    }

    assert!(editor.handle_picker_key(press(KeyCode::Tab)));

    assert_eq!(editor.buffer.as_str(), "/permissions ");
    match editor.picker.as_ref() {
        Some(Picker::Prefixed(picker, prefix)) => {
            assert_eq!(*prefix, "/permissions ");
            assert_eq!(
                picker.matches().collect::<Vec<_>>(),
                ["yolo", "auto", "manual", "rules", "forget"]
            );
        }
        _ => panic!("the command picker should hand off to the permissions picker"),
    }
}

#[test]
fn every_finite_argument_command_supports_shell_style_tab_completion() {
    for command in crate::ui::pickers::handlers::FINITE_ARGUMENT_COMMANDS {
        let mut editor = InputEditor::new();
        editor.load_text(command);
        editor.handle_key(press(KeyCode::Tab));
        assert!(
            matches!(editor.picker, Some(Picker::Prefixed(_, _))),
            "{command} did not open an argument picker"
        );
    }
}

#[test]
fn tab_after_vim_space_lists_on_and_off() {
    let mut editor = InputEditor::new();
    editor.load_text("/vim ");
    editor.handle_key(press(KeyCode::Tab));
    match editor.picker.as_ref() {
        Some(Picker::Prefixed(picker, prefix)) => {
            assert_eq!(*prefix, "/vim ");
            assert_eq!(picker.matches().collect::<Vec<_>>(), ["on", "off"]);
        }
        _ => panic!("Tab after `/vim ` should list its values"),
    }
}

#[cfg(feature = "subagents")]
#[test]
fn tasks_tab_completion_offers_verbs_and_live_task_ids() {
    let mut editor = InputEditor::new();
    editor.set_task_ids(vec!["3f2a9c10".to_string()]);
    editor.load_text("/tasks");
    editor.handle_key(press(KeyCode::Tab));
    match editor.picker.as_ref() {
        Some(Picker::Prefixed(picker, prefix)) => {
            assert_eq!(*prefix, "/tasks ");
            assert_eq!(
                picker.matches().collect::<Vec<_>>(),
                ["result", "cancel", "clear", "3f2a9c10"]
            );
        }
        _ => panic!("Tab should open the tasks picker"),
    }
}

#[test]
fn model_tab_completion_uses_provider_models_for_singular_command() {
    let mut editor = InputEditor::new();
    editor.set_quick_model_names(vec!["quick".to_string()]);
    editor.set_live_model_names(vec!["provider/model".to_string()]);
    editor.load_text("/model");

    editor.handle_key(press(KeyCode::Tab));

    assert_eq!(editor.buffer.as_str(), "/model ");
    match editor.picker.as_ref() {
        Some(Picker::Models(picker)) => {
            assert_eq!(picker.prefix, "/model ");
            assert_eq!(picker.matches, ["provider/model"]);
        }
        _ => panic!("Tab should open the singular model picker"),
    }
}

#[test]
fn tab_keeps_indentation_behavior_for_non_command_text() {
    let mut editor = InputEditor::new();
    editor.load_text("ordinary text");
    editor.handle_key(press(KeyCode::Tab));
    assert_eq!(editor.buffer.as_str(), "ordinary text  ");
    assert!(editor.picker.is_none());
}

#[test]
fn keys73_actual_vim_line_delete_undo_and_unicode_motions() {
    let mut editor = InputEditor::new();
    editor.load_text("雪猫 foo\nsecond\nlast");
    editor.set_vim_enabled(true);
    editor.set_cursor(0);
    editor.handle_key(press(KeyCode::Esc));
    assert_eq!(editor.vim_label(), "vim:NORMAL");
    type_str(&mut editor, "w");
    assert_eq!(&editor.buffer[editor.cursor..], "foo\nsecond\nlast");
    type_str(&mut editor, "b");
    assert_eq!(editor.cursor, 0);
    type_str(&mut editor, "jdd");
    assert_eq!(editor.buffer.as_str(), "雪猫 foo\nlast");
    type_str(&mut editor, "u");
    assert_eq!(editor.buffer.as_str(), "雪猫 foo\nsecond\nlast");
    assert!(editor.buffer.is_char_boundary(editor.cursor));
    editor.set_cursor(editor.buffer.len());
    type_str(&mut editor, "dd");
    assert_eq!(editor.buffer.as_str(), "雪猫 foo\nsecond");
    type_str(&mut editor, "u");
    assert_eq!(editor.buffer.as_str(), "雪猫 foo\nsecond\nlast");
    type_str(&mut editor, "i雪");
    assert!(editor.buffer.ends_with("last雪"));
    assert_eq!(editor.vim_label(), "vim:INSERT");
}

#[test]
fn keys73_actual_vim_literal_search_cancel_and_repeat() {
    let mut editor = InputEditor::new();
    editor.load_text("雪[. foo 雪[.");
    editor.set_vim_enabled(true);
    editor.set_cursor(0);
    editor.handle_key(press(KeyCode::Esc));
    type_str(&mut editor, "/[.");
    assert_eq!(editor.vim_label(), "vim:SEARCH");
    assert_eq!(editor.buffer.as_str(), "雪[. foo 雪[.");
    editor.handle_key(press(KeyCode::Enter));
    assert_eq!(&editor.buffer[editor.cursor..], "[. foo 雪[.");
    type_str(&mut editor, "n");
    assert_eq!(&editor.buffer[editor.cursor..], "[.");
    let cursor = editor.cursor;
    type_str(&mut editor, "/unknown");
    editor.handle_key(press(KeyCode::Esc));
    assert_eq!(editor.cursor, cursor);
    assert_eq!(editor.vim_label(), "vim:NORMAL");
    type_str(&mut editor, "n");
    assert_eq!(&editor.buffer[editor.cursor..], "[. foo 雪[.");
}

#[test]
fn keys73_actual_vim_paste_undo_does_not_cross_load_or_submission() {
    let mut editor = InputEditor::new();
    editor.load_text("before");
    editor.set_vim_enabled(true);
    editor.handle_paste("雪\r\nnext".to_owned());
    assert_eq!(editor.buffer.as_str(), "before雪\nnext");
    editor.handle_key(press(KeyCode::Esc));
    type_str(&mut editor, "u");
    assert_eq!(editor.buffer.as_str(), "before");
    type_str(&mut editor, "iX");
    editor.load_text("another context");
    editor.handle_key(press(KeyCode::Esc));
    type_str(&mut editor, "u");
    assert_eq!(editor.buffer.as_str(), "another context");
    let submitted = editor.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(submitted.as_str(), "another context");
    editor.handle_key(press(KeyCode::Esc));
    type_str(&mut editor, "u");
    assert!(editor.buffer.is_empty());
    editor.set_vim_enabled(false);
    assert!(!editor.vim_enabled());
    assert_eq!(editor.vim_label(), "");
    type_str(&mut editor, "hjkl");
    assert_eq!(editor.buffer.as_str(), "hjkl");
}

#[test]
fn keys73_actual_editor_remap_replaces_defaults_and_keeps_altgr() {
    use crate::config::keybindings::KeyBindings;
    let settings = std::collections::BTreeMap::from([
        ("line_start".into(), vec!["f2".into()]),
        ("delete_backward".into(), vec!["f3".into()]),
        ("submit".into(), vec!["ctrl+s".into()]),
        ("undo".into(), vec!["ctrl+z".into()]),
    ]);
    let mut editor = InputEditor::new();
    editor.keybindings = KeyBindings::from_overrides(Some(&settings)).unwrap();
    editor.load_text("雪猫end");
    editor.handle_key(ctrl('a'));
    assert_eq!(editor.cursor, editor.buffer.len());
    editor.handle_key(press(KeyCode::F(2)));
    assert_eq!(editor.cursor, 0);
    editor.handle_key(press(KeyCode::Right));
    editor.handle_key(press(KeyCode::Backspace));
    assert_eq!(editor.buffer.as_str(), "雪猫end");
    editor.handle_key(press(KeyCode::F(3)));
    assert_eq!(editor.buffer.as_str(), "猫end");
    editor.handle_key(ctrl('z'));
    assert_eq!(editor.buffer.as_str(), "雪猫end");
    editor.set_cursor(editor.buffer.len());
    editor.handle_key(KeyEvent::new(
        KeyCode::Char('€'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ));
    assert_eq!(editor.buffer.as_str(), "雪猫end€");
    assert_eq!(editor.handle_key(press(KeyCode::Enter)), None);
    assert_eq!(editor.buffer.as_str(), "雪猫end€");
    assert_eq!(editor.handle_key(ctrl('s')).unwrap(), "雪猫end€");
    assert!(editor.buffer.is_empty());
    editor.handle_key(ctrl('z'));
    assert!(editor.buffer.is_empty());
}

#[test]
fn keys73_undo_refuses_oversized_history_and_blank_remapped_submission() {
    use crate::config::keybindings::KeyBindings;
    let mut editor = InputEditor::new();
    editor.keybindings = KeyBindings::from_overrides(Some(&std::collections::BTreeMap::from([
        ("submit".into(), vec!["ctrl+s".into()]),
        ("undo".into(), vec!["ctrl+z".into()]),
    ])))
    .unwrap();
    type_str(&mut editor, "  ");
    assert_eq!(editor.handle_key(ctrl('s')), None);
    editor.handle_key(ctrl('z'));
    assert!(editor.buffer.is_empty());
    type_str(&mut editor, "small");
    // A large paste is a chip; build the oversized buffer directly.
    editor.buffer = "x".repeat(2 * 1024 * 1024 + 1).into();
    editor.cursor = editor.buffer.len();
    editor.handle_key(press(KeyCode::Char('y')));
    let oversized_len = editor.buffer.len();
    editor.handle_key(ctrl('z'));
    assert_eq!(editor.buffer.len(), oversized_len);
    editor.load_text("fresh");
    editor.set_vim_enabled(true);
    editor.handle_key(press(KeyCode::Esc));
    editor.handle_operator_draft_key(press(KeyCode::Char('h')));
    assert_eq!(editor.buffer, "freshh");
    editor.handle_operator_draft_key(press(KeyCode::Enter));
    assert!(editor.buffer.is_empty());
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

    assert_eq!(editor.buffer.as_str(), "a☃\nlineb");
    assert_eq!(editor.cursor, "a☃\nline".len());
}

#[test]
fn enter_on_whitespace_only_input_submits_nothing() {
    let mut editor = InputEditor::new();
    editor.handle_key(press(KeyCode::Tab));
    editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    assert_eq!(editor.buffer.as_str(), "  \n");
    assert_eq!(editor.handle_key(press(KeyCode::Enter)), None);
    assert_eq!(editor.buffer.as_str(), "");
    // Nothing blank lands in history either.
    editor.handle_key(press(KeyCode::Up));
    assert_eq!(editor.buffer.as_str(), "");
}

// --- bracketed paste sanitizing (mini-agent-el3xe) ---

#[test]
fn paste_normalizes_line_endings_keeps_tabs_and_drops_controls() {
    use crate::ui::input::sanitize_paste;
    assert_eq!(sanitize_paste("a\rb\r\nc\nd"), "a\nb\nc\nd");
    assert_eq!(sanitize_paste("\r\r\n"), "\n\n");
    assert_eq!(sanitize_paste("x\ty"), "x\ty");
    assert_eq!(sanitize_paste("red \x1b[31mtext\x1b[0m!"), "red text!");
    assert_eq!(sanitize_paste("\x1b]0;title\x07ok"), "ok");
    assert_eq!(sanitize_paste("\x1b]8;;https://x\x1b\\link"), "link");
    assert_eq!(sanitize_paste("\x1b(Bplain\x1bc"), "plain");
    assert_eq!(sanitize_paste("\u{9b}2Jgone\u{7}\u{0}"), "gone");
    assert_eq!(sanitize_paste("bell\x07 back\x08"), "bell back");
    assert_eq!(sanitize_paste("日本 é ☃"), "日本 é ☃");
}

#[test]
fn a_carriage_return_paste_becomes_lines_and_is_not_sent_raw() {
    let mut editor = InputEditor::new();
    editor.handle_paste("one\rtwo\t3\x1b[2J".to_string());
    assert_eq!(editor.buffer.as_str(), "one\ntwo\t3");
    assert_eq!(editor.cursor, editor.buffer.len());
    // A paste made only of controls changes nothing.
    editor.handle_paste("\x1b[A".to_string());
    assert_eq!(editor.buffer.as_str(), "one\ntwo\t3");
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

// --- soft-wrapped input rows (mini-agent-h7vgt) ---

#[test]
fn up_and_down_move_by_wrapped_rows_then_reach_history() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "earlier");
    editor.handle_key(press(KeyCode::Enter));
    editor.set_wrap_width(8);
    // Rows: "hello " | "world " | "again".
    type_str(&mut editor, "hello world again");
    assert_eq!(editor.cursor, editor.buffer.len());
    editor.handle_key(press(KeyCode::Up));
    assert_eq!(
        editor.cursor,
        "hello world ".len() - 1,
        "row end, not next row"
    );
    editor.handle_key(press(KeyCode::Up));
    assert_eq!(editor.cursor, "hello".len());
    editor.handle_key(press(KeyCode::Down));
    assert_eq!(editor.cursor, "hello world".len());
    editor.handle_key(press(KeyCode::Down));
    assert_eq!(editor.cursor, "hello world again".len());
    // From the first row Up recalls history, as with unwrapped input.
    editor.set_cursor(0);
    editor.handle_key(press(KeyCode::Up));
    assert_eq!(editor.buffer.as_str(), "earlier");
}

/// cttyn.19: Up/Down over an unchanged multi-line draft reuse one wrap
/// layout; an edit or a new width wraps once more.
#[test]
fn row_movement_wraps_a_large_draft_once_per_buffer_and_width() {
    let mut editor = InputEditor::new();
    editor.set_wrap_width(40);
    let draft = (0..400)
        .map(|line| format!("line {line} of a long pasted draft that soft wraps at forty"))
        .collect::<Vec<_>>()
        .join("\n");
    editor.buffer = draft.as_str().into();
    editor.set_cursor(editor.buffer.len() / 2);
    for _ in 0..50 {
        editor.handle_key(press(KeyCode::Up));
    }
    for _ in 0..30 {
        editor.handle_key(press(KeyCode::Down));
    }
    assert_eq!(editor.wrap_layouts(), 1);
    let cursor = editor.cursor;
    // Same moves as an uncached layout would make.
    let rows = crate::ui::input::wrap::wrap_rows(&editor.buffer, 40);
    let (row, _) = crate::ui::input::wrap::cursor_position(&editor.buffer, &rows, cursor);
    let (start_row, _) =
        crate::ui::input::wrap::cursor_position(&editor.buffer, &rows, editor.buffer.len() / 2);
    assert_eq!(row + 20, start_row);
    // An edit changes the buffer: one more layout, then reuse again.
    type_str(&mut editor, "y");
    editor.handle_key(press(KeyCode::Up));
    editor.handle_key(press(KeyCode::Down));
    assert_eq!(editor.wrap_layouts(), 2);
    // A new width: one more.
    editor.set_wrap_width(60);
    editor.handle_key(press(KeyCode::Up));
    editor.handle_key(press(KeyCode::Up));
    assert_eq!(editor.wrap_layouts(), 3);
}

// --- word motion on Alt/Ctrl+arrows and Alt+Backspace (mini-agent-rmcpm) ---

fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

#[test]
fn alt_and_ctrl_arrows_move_by_word() {
    for modifiers in [KeyModifiers::ALT, KeyModifiers::CONTROL] {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "héllo wörld  end");
        editor.handle_key(with(KeyCode::Left, modifiers));
        assert_eq!(editor.cursor, "héllo wörld  ".len(), "{modifiers:?}");
        editor.handle_key(with(KeyCode::Left, modifiers));
        assert_eq!(editor.cursor, "héllo ".len(), "{modifiers:?}");
        editor.handle_key(with(KeyCode::Right, modifiers));
        assert_eq!(editor.cursor, "héllo wörld".len(), "{modifiers:?}");
        editor.handle_key(with(KeyCode::Right, modifiers));
        assert_eq!(editor.cursor, editor.buffer.len(), "{modifiers:?}");
    }
}

#[test]
fn alt_backspace_deletes_the_previous_word_onto_the_kill_ring() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "keep this");
    editor.handle_key(with(KeyCode::Backspace, KeyModifiers::ALT));
    assert_eq!(editor.buffer.as_str(), "keep ");
    editor.handle_key(ctrl('y'));
    assert_eq!(editor.buffer.as_str(), "keep this");
}

#[test]
fn word_commands_treat_newlines_and_tabs_as_separators() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "foo bar");
    editor.handle_key(shift_enter());
    type_str(&mut editor, "baz");
    editor.handle_key(ctrl('w'));
    assert_eq!(editor.buffer.as_str(), "foo bar\n");
    editor.handle_key(alt('b'));
    assert_eq!(editor.cursor, "foo ".len());
    editor.handle_key(alt('f'));
    assert_eq!(editor.cursor, "foo bar".len());
}

// --- line-edge keys, history recall, unbound chords, @ triggers ---

fn shift_enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
}

/// "first\nsecond" with the caret inside "second".
fn two_line_editor() -> InputEditor {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "first");
    editor.handle_key(shift_enter());
    type_str(&mut editor, "second");
    editor.cursor = "first\nsec".len();
    editor
}

#[test]
fn home_and_end_move_to_the_current_line_edges() {
    let mut editor = two_line_editor();
    editor.handle_key(press(KeyCode::Home));
    assert_eq!(editor.cursor, "first\n".len());
    editor.handle_key(press(KeyCode::End));
    assert_eq!(editor.cursor, "first\nsecond".len());

    editor.cursor = 2;
    editor.handle_key(press(KeyCode::End));
    assert_eq!(editor.cursor, "first".len());
    editor.handle_key(press(KeyCode::Home));
    assert_eq!(editor.cursor, 0);
}

#[test]
fn ctrl_a_and_ctrl_e_stay_put_at_the_line_edge() {
    let mut editor = two_line_editor();
    editor.handle_key(ctrl('a'));
    editor.handle_key(ctrl('a'));
    assert_eq!(
        editor.cursor,
        "first\n".len(),
        "Ctrl+A must not leave the line"
    );

    editor.cursor = 2;
    editor.handle_key(ctrl('e'));
    editor.handle_key(ctrl('e'));
    assert_eq!(
        editor.cursor,
        "first".len(),
        "Ctrl+E must not leave the line"
    );
}

#[test]
fn history_up_places_the_caret_at_the_end_of_the_recalled_prompt() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "earlier prompt");
    assert!(editor.handle_key(press(KeyCode::Enter)).is_some());

    editor.handle_key(press(KeyCode::Up));
    assert_eq!(editor.buffer.as_str(), "earlier prompt");
    assert_eq!(editor.cursor, editor.buffer.len());
    type_str(&mut editor, "!");
    assert_eq!(editor.buffer.as_str(), "earlier prompt!");
}

#[test]
fn unbound_alt_and_ctrl_chords_do_not_insert_their_letter() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "a");
    for key in [alt('l'), alt('x'), alt('Q'), ctrl('g'), ctrl('o')] {
        editor.handle_key(key);
        assert_eq!(editor.buffer.as_str(), "a", "{key:?} must not type");
        assert_eq!(editor.cursor, 1);
    }
    assert!(editor.picker.is_none());
}

#[test]
fn altgr_characters_still_insert() {
    // Windows reports AltGr+Q on a German layout as Ctrl+Alt+'@'.
    let mut editor = InputEditor::new();
    editor.handle_key(KeyEvent::new(
        KeyCode::Char('@'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ));
    assert_eq!(editor.buffer.as_str(), "@");
    assert!(file_picker_open(&editor));
}

fn file_picker_open(editor: &InputEditor) -> bool {
    matches!(editor.picker.as_ref(), Some(crate::ui::input::Picker::File(p)) if p.active)
}

#[test]
fn at_opens_the_file_picker_after_newline_paren_and_quotes() {
    for prefix in ["", "see ", "(", "\"", "'", "`", "x\t"] {
        let mut editor = InputEditor::new();
        editor.handle_paste(prefix.to_string());
        type_str(&mut editor, "@");
        assert!(file_picker_open(&editor), "after {prefix:?}");
    }
    let mut editor = InputEditor::new();
    type_str(&mut editor, "line");
    editor.handle_key(shift_enter());
    type_str(&mut editor, "@");
    assert!(file_picker_open(&editor), "after a newline");

    let mut editor = InputEditor::new();
    type_str(&mut editor, "user@");
    assert!(!file_picker_open(&editor), "mid-word @ is an address");
}

#[test]
fn pasting_a_trailing_at_opens_the_file_picker() {
    let mut editor = InputEditor::new();
    editor.handle_paste("look at @".to_string());
    assert_eq!(editor.buffer.as_str(), "look at @");
    assert!(file_picker_open(&editor));

    let mut editor = InputEditor::new();
    editor.handle_paste("user@".to_string());
    assert!(!file_picker_open(&editor));
}

#[test]
fn lone_ctrl_j_inserts_newline_and_does_not_submit() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "ab");
    let submitted = editor.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
    assert!(submitted.is_none());
    type_str(&mut editor, "cd");
    assert_eq!(editor.buffer.as_str(), "ab\ncd");
}

#[test]
fn backslash_enter_inserts_newline_and_consumes_the_backslash() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "line one\\");
    assert!(editor.handle_key(press(KeyCode::Enter)).is_none());
    type_str(&mut editor, "two");
    assert_eq!(editor.buffer.as_str(), "line one\ntwo");
}

#[test]
fn doubled_backslash_enter_still_submits() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "path\\\\");
    let submitted = editor.handle_key(press(KeyCode::Enter));
    assert_eq!(submitted.as_deref(), Some("path\\\\"));
}

// --- grapheme editing (mini-agent-nvnkx.85) ---

const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
const FLAG: &str = "\u{1F1E9}\u{1F1EA}";

#[test]
fn backspace_removes_a_whole_zwj_family_flag_and_skin_tone_emoji() {
    for cluster in [FAMILY, FLAG, "\u{1F44D}\u{1F3FD}", "e\u{301}"] {
        let mut editor = InputEditor::new();
        type_str(&mut editor, &format!("a{cluster}"));
        editor.handle_key(press(KeyCode::Backspace));
        assert_eq!(editor.buffer.as_str(), "a", "{cluster:?}");
        editor.handle_key(press(KeyCode::Backspace));
        assert!(editor.buffer.is_empty());
    }
    let mut editor = InputEditor::new();
    editor.load_text(FAMILY);
    editor.handle_key(press(KeyCode::Backspace));
    assert!(editor.buffer.is_empty());
}

#[test]
fn arrows_and_delete_step_over_whole_grapheme_clusters() {
    let mut editor = InputEditor::new();
    editor.load_text("e\u{301}");
    editor.handle_key(press(KeyCode::Left));
    assert_eq!(editor.cursor, 0);
    editor.handle_key(press(KeyCode::Right));
    assert_eq!(editor.cursor, editor.buffer.len());

    let mut editor = InputEditor::new();
    editor.load_text(&format!("{FLAG}{FLAG}x"));
    editor.handle_key(ctrl('a'));
    editor.handle_key(press(KeyCode::Right));
    assert_eq!(editor.cursor, FLAG.len());
    editor.handle_key(press(KeyCode::Delete));
    assert_eq!(editor.buffer.as_str(), format!("{FLAG}x"));
    editor.handle_key(ctrl('a'));
    editor.handle_key(ctrl('d'));
    assert_eq!(editor.buffer.as_str(), "x");
}

#[test]
fn vim_x_and_h_l_move_by_grapheme() {
    let mut editor = InputEditor::new();
    editor.load_text(&format!("{FAMILY}z"));
    editor.set_vim_enabled(true);
    editor.set_cursor(0);
    editor.handle_key(press(KeyCode::Esc));
    type_str(&mut editor, "l");
    assert_eq!(editor.cursor, FAMILY.len());
    type_str(&mut editor, "h");
    assert_eq!(editor.cursor, 0);
    type_str(&mut editor, "x");
    assert_eq!(editor.buffer.as_str(), "z");
}

// --- Ctrl+U / Ctrl+K kill to the line edge (mini-agent-nvnkx.86) ---

#[test]
fn ctrl_k_and_ctrl_u_kill_only_the_current_line() {
    let mut editor = InputEditor::new();
    editor.load_text("a\nbc\nd");
    editor.set_cursor("a\nb".len());
    editor.handle_key(ctrl('k'));
    assert_eq!(editor.buffer.as_str(), "a\nb\nd");
    editor.handle_key(ctrl('u'));
    assert_eq!(editor.buffer.as_str(), "a\n\nd");
    assert_eq!(editor.cursor, 2);
    editor.handle_key(ctrl('y'));
    assert_eq!(editor.buffer.as_str(), "a\nb\nd");
}

#[test]
fn ctrl_k_and_ctrl_u_at_a_line_edge_join_lines() {
    let mut editor = InputEditor::new();
    editor.load_text("one\ntwo");
    editor.set_cursor(3);
    editor.handle_key(ctrl('k'));
    assert_eq!(editor.buffer.as_str(), "onetwo");
    editor.handle_key(ctrl('a'));
    editor.handle_key(ctrl('u'));
    assert_eq!(editor.buffer.as_str(), "onetwo");
    editor.load_text("x\n\ny");
    editor.set_cursor(2);
    editor.handle_key(ctrl('u'));
    assert_eq!(editor.buffer.as_str(), "x\ny");
}

// --- tabs stay in the prompt text (mini-agent-nvnkx.87) ---

#[test]
fn pasted_tabs_reach_the_submitted_text_and_draw_at_four_columns() {
    let mut editor = InputEditor::new();
    editor.handle_paste("a:\n\tcmd".to_owned());
    assert_eq!(editor.buffer.as_str(), "a:\n\tcmd");
    let submitted = editor.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(submitted.contains('\t'));

    use crate::ui::input::wrap::{cursor_position, expand_tabs, wrap_rows};
    let text = "a\tb";
    let rows = wrap_rows(text, 20);
    assert_eq!(cursor_position(text, &rows, 2), (0, 5));
    assert_eq!(expand_tabs("\tx"), "    x");
    // A tab counts four columns when wrapping.
    assert_eq!(wrap_rows("\t\tab", 8).len(), 2);
}

// --- large pastes become chips (mini-agent-nvnkx.88) ---

#[test]
fn a_large_paste_is_one_chip_expanded_on_submit() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "see ");
    let log: String = (0..500).map(|i| format!("line {i}\n")).collect();
    let log = log.trim_end().to_owned();
    editor.handle_paste(log.clone());
    assert_eq!(editor.buffer.as_str(), "see [Pasted text #1 +500 lines]");
    assert_eq!(editor.cursor, editor.buffer.len());
    type_str(&mut editor, " ok");
    let submitted = editor.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(submitted.as_str(), format!("see {log} ok"));
    // History keeps the full text.
    editor.handle_key(press(KeyCode::Up));
    assert_eq!(editor.buffer.as_str(), format!("see {log} ok"));
}

#[test]
fn a_paste_chip_is_atomic_for_cursor_and_backspace() {
    let mut editor = InputEditor::new();
    editor.handle_paste("x\n".repeat(30));
    let chip = editor.buffer.clone();
    assert!(chip.starts_with("[Pasted text #1"), "{chip}");
    editor.handle_key(press(KeyCode::Left));
    assert_eq!(editor.cursor, 0);
    editor.handle_key(press(KeyCode::Right));
    assert_eq!(editor.cursor, chip.len());
    editor.set_cursor(5);
    assert_eq!(editor.cursor, 0);
    editor.handle_key(ctrl('e'));
    editor.handle_key(press(KeyCode::Backspace));
    assert!(editor.buffer.is_empty());
    // A small paste stays text.
    editor.handle_paste("one\ntwo".to_owned());
    assert_eq!(editor.buffer.as_str(), "one\ntwo");
    // A long single line becomes a chip too.
    editor.clear_buffer();
    editor.handle_paste("y".repeat(2000));
    assert_eq!(editor.buffer.as_str(), "[Pasted text #1 2000 chars]");
}

// --- prompt undo is bound by default (mini-agent-nvnkx.90) ---

#[test]
fn ctrl_underscore_undoes_a_word_kill_by_default() {
    for key in [
        KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('7'), KeyModifiers::CONTROL),
        KeyEvent::new(
            KeyCode::Char('_'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ),
    ] {
        let mut editor = InputEditor::new();
        type_str(&mut editor, "abc def");
        editor.handle_key(ctrl('w'));
        assert_eq!(editor.buffer.as_str(), "abc ");
        editor.handle_key(key);
        assert_eq!(editor.buffer.as_str(), "abc def", "{key:?}");
    }
}

#[test]
fn typing_coalesces_into_word_sized_undo_steps() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "abc def");
    let undo = KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL);
    editor.handle_key(undo);
    assert_eq!(editor.buffer.as_str(), "abc ");
    editor.handle_key(undo);
    assert_eq!(editor.buffer.as_str(), "abc");
    editor.handle_key(undo);
    assert_eq!(editor.buffer.as_str(), "");
}

// --- Vim operators, counts, registers and dot repeat (mini-agent-nvnkx.89) ---

fn vim_editor(text: &str) -> InputEditor {
    let mut editor = InputEditor::new();
    editor.load_text(text);
    editor.set_vim_enabled(true);
    editor.set_cursor(0);
    editor.handle_key(press(KeyCode::Esc));
    editor
}

#[test]
fn vim_cw_changes_the_word_and_esc_returns_to_normal() {
    let mut editor = vim_editor("foo bar");
    type_str(&mut editor, "cwx");
    editor.handle_key(press(KeyCode::Esc));
    assert_eq!(editor.buffer.as_str(), "x bar");
    assert_eq!(editor.vim_label(), "vim:NORMAL");
}

#[test]
fn vim_counts_operators_and_motions() {
    let mut editor = vim_editor("a\nb\nc\nd");
    type_str(&mut editor, "2dd");
    assert_eq!(editor.buffer.as_str(), "c\nd");
    let mut editor = vim_editor("one two three four");
    type_str(&mut editor, "d2w");
    assert_eq!(editor.buffer.as_str(), "three four");
    type_str(&mut editor, "3x");
    assert_eq!(editor.buffer.as_str(), "ee four");
    type_str(&mut editor, "$");
    type_str(&mut editor, "dw");
    assert_eq!(editor.buffer.as_str(), "ee four");
    type_str(&mut editor, "0dw");
    assert_eq!(editor.buffer.as_str(), "four");
    type_str(&mut editor, "d$");
    assert_eq!(editor.buffer.as_str(), "");
}

#[test]
fn vim_p_puts_the_last_deleted_line_and_dot_repeats() {
    let mut editor = vim_editor("one\ntwo\nthree");
    type_str(&mut editor, "dd");
    assert_eq!(editor.buffer.as_str(), "two\nthree");
    type_str(&mut editor, "p");
    assert_eq!(editor.buffer.as_str(), "two\none\nthree");
    type_str(&mut editor, "P");
    assert_eq!(editor.buffer.as_str(), "two\none\none\nthree");
    let mut editor = vim_editor("a b c d");
    type_str(&mut editor, "dw");
    assert_eq!(editor.buffer.as_str(), "b c d");
    type_str(&mut editor, ".");
    assert_eq!(editor.buffer.as_str(), "c d");
    // Dot repeats a change with its typed text, once.
    let mut editor = vim_editor("aa bb cc");
    type_str(&mut editor, "cwX");
    editor.handle_key(press(KeyCode::Esc));
    type_str(&mut editor, "w.");
    assert_eq!(editor.buffer.as_str(), "X X cc");
    // One undo step reverts the repeat.
    type_str(&mut editor, "u");
    assert_eq!(editor.buffer.as_str(), "X bb cc");
}

#[test]
fn vim_yank_put_open_replace_find_and_goto() {
    let mut editor = vim_editor("foo bar");
    type_str(&mut editor, "ywP");
    assert_eq!(editor.buffer.as_str(), "foo foo bar");
    let mut editor = vim_editor("l1\nl2");
    type_str(&mut editor, "yyjp");
    assert_eq!(editor.buffer.as_str(), "l1\nl2\nl1");
    let mut editor = vim_editor("l1\nl2");
    type_str(&mut editor, "oX");
    editor.handle_key(press(KeyCode::Esc));
    assert_eq!(editor.buffer.as_str(), "l1\nX\nl2");
    type_str(&mut editor, "OY");
    editor.handle_key(press(KeyCode::Esc));
    assert_eq!(editor.buffer.as_str(), "l1\nY\nX\nl2");
    let mut editor = vim_editor("abcd");
    type_str(&mut editor, "rz");
    assert_eq!(editor.buffer.as_str(), "zbcd");
    type_str(&mut editor, "2rq");
    assert_eq!(editor.buffer.as_str(), "qqcd");
    type_str(&mut editor, "~");
    assert_eq!(editor.buffer.as_str(), "qQcd");
    let mut editor = vim_editor("hello, world");
    type_str(&mut editor, "dt,");
    assert_eq!(editor.buffer.as_str(), ", world");
    type_str(&mut editor, "fwD");
    assert_eq!(editor.buffer.as_str(), ", ");
    let mut editor = vim_editor("a\nb\nc");
    type_str(&mut editor, "Gx");
    assert_eq!(editor.buffer.as_str(), "a\nb\n");
    type_str(&mut editor, "ggdG");
    assert_eq!(editor.buffer.as_str(), "");
    let mut editor = vim_editor("foo bar baz");
    type_str(&mut editor, "wdiw");
    assert_eq!(editor.buffer.as_str(), "foo  baz");
    type_str(&mut editor, "daw");
    assert_eq!(editor.buffer.as_str(), "foo");
    type_str(&mut editor, "0e");
    assert_eq!(editor.cursor, 2);
    type_str(&mut editor, "^");
    assert_eq!(editor.cursor, 0);
    let mut editor = vim_editor("Ab");
    type_str(&mut editor, "2~");
    assert_eq!(editor.buffer.as_str(), "aB");
    type_str(&mut editor, "C!");
    assert_eq!(editor.buffer.as_str(), "aB!");
}
