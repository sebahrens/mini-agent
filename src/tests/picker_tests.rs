use crate::ui::input::{InputEditor, Picker};
use crate::ui::pickers::file::{FilePicker, walk_files, walk_files_streaming};
use crate::ui::pickers::list::ListPicker;
use crate::ui::pickers::models::ModelsPicker;
use std::path::PathBuf;

#[test]
fn test_command_picker_paste_keeps_query_and_buffer_coherent() {
    let mut input = InputEditor::new();
    input.buffer = "/".into();
    input.cursor = 1;
    input.start_command_picker();

    input.handle_paste("mod".to_string());

    assert_eq!(input.buffer, "/mod");
    assert_eq!(input.cursor, 4);
    let Some(Picker::Command(picker)) = input.picker.as_ref() else {
        panic!("command picker should remain active");
    };
    assert_eq!(picker.query, "mod");
    assert!(picker.matches.contains(&"/model".to_string()));
}

#[test]
fn test_prefixed_picker_paste_keeps_unicode_query_and_cursor_coherent() {
    let mut input = InputEditor::new();
    input.set_prompt_names(vec!["café".to_string()]);
    input.buffer = "/prompt ".into();
    input.cursor = input.buffer.len();
    input.start_prompt_picker();

    input.handle_paste("café".to_string());

    assert_eq!(input.buffer, "/prompt café");
    assert_eq!(input.cursor, input.buffer.len());
    let Some(Picker::Prefixed(picker, "/prompt ")) = input.picker.as_ref() else {
        panic!("prompt picker should remain active");
    };
    assert_eq!(picker.query, "café");
    assert_eq!(picker.matches, vec!["café"]);
}

#[test]
fn test_multiline_paste_closes_picker_and_inserts_without_submitting() {
    let mut input = InputEditor::new();
    input.buffer = "/".into();
    input.cursor = 1;
    input.start_command_picker();

    input.handle_paste("mod\nnext".to_string());

    assert_eq!(input.buffer, "/mod\nnext");
    assert_eq!(input.cursor, input.buffer.len());
    assert!(input.picker.is_none());
}

#[test]
fn test_oversized_picker_paste_closes_picker_without_replaying_each_character() {
    let mut input = InputEditor::new();
    input.buffer = "/".into();
    input.cursor = 1;
    input.start_command_picker();
    let pasted = "x".repeat(257);

    input.handle_paste(pasted.clone());

    assert_eq!(input.buffer, format!("/{pasted}"));
    assert_eq!(input.cursor, input.buffer.len());
    assert!(input.picker.is_none());
}

#[test]
fn test_paste_without_picker_preserves_mid_buffer_behavior() {
    let mut input = InputEditor::new();
    input.load_text("ab");
    input.set_cursor(1);

    input.handle_paste("é\n".to_string());

    assert_eq!(input.buffer, "aé\nb");
    assert_eq!(input.cursor, "aé\n".len());
    assert!(input.picker.is_none());
}

#[test]
fn test_models_picker_starts_on_quick_group() {
    let mut picker = ModelsPicker::new();
    picker.set_groups(
        vec!["fast".to_string()],
        vec!["claude-opus-4-7".to_string()],
    );
    picker.activate();
    assert_eq!(picker.matches, vec!["fast".to_string()]);
}

#[test]
fn test_models_picker_tab_toggles_to_provider_group() {
    let mut picker = ModelsPicker::new();
    picker.set_groups(
        vec!["fast".to_string()],
        vec!["claude-opus-4-7".to_string()],
    );
    picker.activate();
    picker.toggle_group();
    assert_eq!(picker.matches, vec!["claude-opus-4-7".to_string()]);
}

#[test]
fn test_models_picker_starts_on_provider_when_quick_empty() {
    let mut picker = ModelsPicker::new();
    picker.set_groups(Vec::new(), vec!["claude-opus-4-7".to_string()]);
    picker.activate();
    assert_eq!(picker.matches, vec!["claude-opus-4-7".to_string()]);
}

#[test]
fn test_models_picker_fuzzy_subsequence_match() {
    let mut picker = ModelsPicker::new();
    picker.set_groups(
        Vec::new(),
        vec!["claude-opus-4-7".to_string(), "gpt-4o-mini".to_string()],
    );
    picker.activate();
    for c in "o47".chars() {
        picker.char_input(c);
    }
    assert_eq!(picker.selected_name(), Some("claude-opus-4-7"));
    assert!(!picker.matches.iter().any(|m| m == "gpt-4o-mini"));
}

#[test]
fn test_backspace_empty_query() {
    let mut picker = FilePicker::new();
    picker.test_set_cache(vec![PathBuf::from("test.rs")]);
    picker.backspace();
    assert!(picker.query.is_empty());
    assert_eq!(picker.cursor, 0);
}

#[test]
fn test_char_input_and_backspace_ascii() {
    let mut picker = FilePicker::new();
    picker.test_set_cache(vec![PathBuf::from("test.rs")]);
    picker.char_input('a');
    picker.char_input('b');
    picker.char_input('c');
    assert_eq!(picker.query, "abc");
    assert_eq!(picker.cursor, 3);

    picker.backspace();
    assert_eq!(picker.query, "ab");
    assert_eq!(picker.cursor, 2);

    picker.backspace();
    assert_eq!(picker.query, "a");
    assert_eq!(picker.cursor, 1);

    picker.backspace();
    assert_eq!(picker.query, "");
    assert_eq!(picker.cursor, 0);

    picker.backspace();
    assert_eq!(picker.query, "");
    assert_eq!(picker.cursor, 0);
}

#[test]
fn test_char_input_and_backspace_unicode() {
    let mut picker = FilePicker::new();
    picker.test_set_cache(vec![PathBuf::from("test.rs")]);

    picker.char_input('é');
    assert_eq!(picker.query, "é");
    assert_eq!(picker.cursor, 1);

    picker.char_input('ñ');
    assert_eq!(picker.query, "éñ");
    assert_eq!(picker.cursor, 2);

    picker.backspace();
    assert_eq!(picker.query, "é");
    assert_eq!(picker.cursor, 1);

    picker.backspace();
    assert_eq!(picker.query, "");
    assert_eq!(picker.cursor, 0);

    picker.char_input('a');
    picker.char_input('é');
    picker.char_input('b');
    assert_eq!(picker.query, "aéb");
    assert_eq!(picker.cursor, 3);

    picker.backspace();
    assert_eq!(picker.query, "aé");
    assert_eq!(picker.cursor, 2);

    picker.backspace();
    assert_eq!(picker.query, "a");
    assert_eq!(picker.cursor, 1);

    picker.backspace();
    assert_eq!(picker.query, "");
    assert_eq!(picker.cursor, 0);
}

#[test]
fn test_mid_query_insertion_unicode() {
    let mut picker = FilePicker::new();
    picker.test_set_cache(vec![PathBuf::from("test.rs")]);

    picker.char_input('a');
    picker.char_input('b');
    assert_eq!(picker.query, "ab");
    assert_eq!(picker.cursor, 2);

    picker.backspace();
    assert_eq!(picker.query, "a");
    assert_eq!(picker.cursor, 1);

    picker.char_input('é');
    assert_eq!(picker.query, "aé");
    assert_eq!(picker.cursor, 2);

    picker.char_input('c');
    assert_eq!(picker.query, "aéc");
    assert_eq!(picker.cursor, 3);

    picker.backspace();
    assert_eq!(picker.query, "aé");
    assert_eq!(picker.cursor, 2);

    picker.backspace();
    assert_eq!(picker.query, "a");
    assert_eq!(picker.cursor, 1);
}

#[test]
fn test_deactivate_and_reactivate() {
    let mut picker = FilePicker::new();
    picker.test_set_cache(vec![PathBuf::from("test.rs")]);
    picker.char_input('h');
    picker.char_input('i');
    assert_eq!(picker.query, "hi");

    picker.deactivate();
    assert!(!picker.active);

    picker.activate();
    assert!(picker.active);
    assert_eq!(picker.query, "");
    assert_eq!(picker.cursor, 0);
}

#[test]
fn test_backspace_cursor_never_negative() {
    let mut picker = FilePicker::new();
    picker.test_set_cache(vec![PathBuf::from("test.rs")]);
    for _ in 0..10 {
        picker.backspace();
    }
    assert_eq!(picker.cursor, 0);
    assert!(picker.query.is_empty());
}

#[test]
fn test_emoji_handling() {
    let mut picker = FilePicker::new();
    picker.test_set_cache(vec![PathBuf::from("test.rs")]);

    picker.char_input('🔥');
    assert_eq!(picker.query, "🔥");
    assert_eq!(picker.cursor, 1);

    picker.char_input('x');
    assert_eq!(picker.query, "🔥x");
    assert_eq!(picker.cursor, 2);

    picker.backspace();
    assert_eq!(picker.query, "🔥");
    assert_eq!(picker.cursor, 1);

    picker.backspace();
    assert_eq!(picker.query, "");
    assert_eq!(picker.cursor, 0);
}

// ── ListPicker tests ───────────────────────────────────────────────

#[test]
fn test_list_picker_filter() {
    let mut picker = ListPicker::new();
    picker.set_items(vec![
        "alpha".to_string(),
        "beta".to_string(),
        "gamma".to_string(),
    ]);
    picker.activate();
    assert_eq!(picker.matches.len(), 3);

    picker.char_input('a');
    assert_eq!(picker.matches, vec!["alpha", "beta", "gamma"]);

    picker.char_input('l');
    assert_eq!(picker.matches, vec!["alpha"]);
}

#[test]
fn test_list_picker_navigation() {
    let mut picker = ListPicker::new();
    picker.set_items(vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    picker.activate();
    assert_eq!(picker.selected, 0);

    picker.select_next();
    assert_eq!(picker.selected, 1);

    picker.select_prev();
    assert_eq!(picker.selected, 0);

    picker.select_prev();
    assert_eq!(picker.selected, 2);
}

#[test]
fn test_list_picker_backspace_and_char_unicode() {
    let mut picker = ListPicker::new();
    picker.set_items(vec!["test".to_string()]);

    picker.char_input('é');
    assert_eq!(picker.query, "é");
    assert_eq!(picker.cursor, 1);

    picker.char_input('ñ');
    assert_eq!(picker.query, "éñ");
    assert_eq!(picker.cursor, 2);

    picker.backspace();
    assert_eq!(picker.query, "é");
    assert_eq!(picker.cursor, 1);

    picker.backspace();
    assert_eq!(picker.query, "");
    assert_eq!(picker.cursor, 0);

    picker.backspace();
    assert_eq!(picker.query, "");
    assert_eq!(picker.cursor, 0);
}

#[test]
fn test_list_picker_reactivate_resets_state() {
    let mut picker = ListPicker::new();
    picker.set_items(vec!["a".to_string(), "b".to_string()]);
    picker.char_input('a');
    picker.char_input('b');
    assert_eq!(picker.query, "ab");

    picker.deactivate();
    assert!(!picker.active);

    picker.activate();
    assert!(picker.active);
    assert_eq!(picker.query, "");
    assert_eq!(picker.cursor, 0);
    assert_eq!(picker.selected, 0);
}

#[test]
fn test_static_commands_prepopulated() {
    let mut picker = ListPicker::with_static_commands();
    picker.activate();
    assert!(picker.matches.len() > 5);

    picker.char_input('m');
    picker.char_input('o');
    picker.char_input('d');
    assert!(picker.matches.contains(&"/model".to_string()));
}

// ── walk_files tests ────────────────────────────────────────────────

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn with_temp_dir<F>(f: F)
where
    F: FnOnce(&Path),
{
    let n = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("zerostack_test_{}_{}", std::process::id(), n));
    fs::create_dir_all(&dir).unwrap();
    let canonical = dir.canonicalize().unwrap();
    f(&canonical);
    let _ = fs::remove_dir_all(&canonical);
}

#[test]
fn test_walk_files_includes_directories() {
    with_temp_dir(|root| {
        fs::create_dir(root.join("subdir")).unwrap();
        fs::write(root.join("file.txt"), b"hello").unwrap();

        let files = walk_files(&root.to_string_lossy());
        let names: Vec<&str> = files.iter().map(|p| p.to_str().unwrap()).collect();

        assert!(
            names.contains(&"file.txt"),
            "walk_files should include files"
        );
        assert!(
            names.contains(&"subdir"),
            "walk_files should include directories, got: {:?}",
            names
        );
    });
}

#[test]
fn test_walk_files_includes_nested_dirs() {
    with_temp_dir(|root| {
        fs::create_dir_all(root.join("a").join("b")).unwrap();
        fs::write(root.join("a").join("b").join("deep.txt"), b"deep").unwrap();

        let files = walk_files(&root.to_string_lossy());

        assert!(files.contains(&Path::new("a").to_path_buf()));
        assert!(files.contains(&Path::new("a").join("b")));
        assert!(files.contains(&Path::new("a").join("b").join("deep.txt")));
    });
}

#[test]
fn test_walk_files_skips_dotfiles() {
    with_temp_dir(|root| {
        fs::write(root.join(".hidden"), b"secret").unwrap();
        fs::write(root.join("visible.txt"), b"hello").unwrap();

        let files = walk_files(&root.to_string_lossy());
        let names: Vec<&str> = files.iter().map(|p| p.to_str().unwrap()).collect();

        assert!(!names.contains(&".hidden"));
        assert!(names.contains(&"visible.txt"));
    });
}

#[test]
fn test_walk_files_skips_files_in_dot_dirs() {
    with_temp_dir(|root| {
        fs::create_dir_all(root.join(".secret").join("nested")).unwrap();
        fs::write(
            root.join(".secret").join("nested").join("file.txt"),
            b"hidden",
        )
        .unwrap();
        fs::write(root.join(".secret").join("secret_file.txt"), b"hidden").unwrap();
        fs::write(root.join("public.txt"), b"visible").unwrap();

        let files = walk_files(&root.to_string_lossy());
        let names: Vec<&str> = files.iter().map(|p| p.to_str().unwrap()).collect();

        assert!(!names.contains(&".secret"));
        assert!(!names.contains(&".secret/nested"));
        assert!(!names.contains(&".secret/nested/file.txt"));
        assert!(!names.contains(&".secret/secret_file.txt"));
        assert!(names.contains(&"public.txt"));
    });
}

#[test]
fn test_walk_files_root_is_sorted_and_stripped() {
    with_temp_dir(|root| {
        fs::write(root.join("z.txt"), b"z").unwrap();
        fs::write(root.join("c.txt"), b"c").unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();

        let files = walk_files(&root.to_string_lossy());
        let names: Vec<&str> = files.iter().map(|p| p.to_str().unwrap()).collect();

        let root_idx = names.iter().position(|n| n.is_empty());
        assert!(
            root_idx.is_some(),
            "root entry (empty string) should be present"
        );

        let file_indices: Vec<usize> = names
            .iter()
            .enumerate()
            .filter(|(_, n)| n.ends_with(".txt"))
            .map(|(i, _)| i)
            .collect();
        assert!(
            file_indices.windows(2).all(|w| w[0] < w[1]),
            "files should be sorted"
        );
    });
}

#[test]
fn test_walk_files_empty_directory() {
    with_temp_dir(|root| {
        let files = walk_files(&root.to_string_lossy());
        let names: Vec<&str> = files.iter().map(|p| p.to_str().unwrap()).collect();

        assert_eq!(names.len(), 1, "only root entry expected in empty dir");
        assert!(names.contains(&""), "root entry should be present");
    });
}

// ── walk_files_streaming tests ───────────────────────────────────────

#[test]
fn test_walk_files_streaming_batches_match_walk_files() {
    with_temp_dir(|root| {
        for i in 0..30 {
            fs::write(root.join(format!("file{:02}.txt", i)), b"x").unwrap();
        }

        let mut batches: Vec<Vec<std::path::PathBuf>> = Vec::new();
        walk_files_streaming(
            &root.to_string_lossy(),
            &std::sync::atomic::AtomicBool::new(false),
            |batch| {
                batches.push(batch);
                true
            },
        );

        assert!(
            batches.len() > 1,
            "31 entries should arrive in multiple batches"
        );
        assert!(batches.iter().all(|b| b.len() <= 25));

        let streamed: Vec<&std::path::PathBuf> = batches.iter().flatten().collect();
        let full = walk_files(&root.to_string_lossy());
        assert_eq!(
            streamed,
            full.iter().collect::<Vec<_>>(),
            "streamed batches should equal the full walk, in order"
        );
    });
}

#[test]
fn test_walk_files_streaming_cancel_stops_immediately() {
    with_temp_dir(|root| {
        for i in 0..10 {
            fs::write(root.join(format!("file{}.txt", i)), b"x").unwrap();
        }

        let cancel = std::sync::atomic::AtomicBool::new(true);
        let mut files = Vec::new();
        walk_files_streaming(&root.to_string_lossy(), &cancel, |batch| {
            files.extend(batch);
            true
        });
        assert!(
            files.is_empty(),
            "a pre-set cancel flag should prevent any results"
        );
    });
}

#[test]
fn test_walk_files_streaming_emit_false_stops_early() {
    with_temp_dir(|root| {
        for i in 0..60 {
            fs::write(root.join(format!("file{:02}.txt", i)), b"x").unwrap();
        }

        let mut files = Vec::new();
        walk_files_streaming(
            &root.to_string_lossy(),
            &std::sync::atomic::AtomicBool::new(false),
            |batch| {
                files.extend(batch);
                false // refuse every batch: stop after the first one
            },
        );
        assert!(
            files.len() <= 25,
            "refusing the first batch should stop the walk, got {} files",
            files.len()
        );
    });
}

// --- byte-offset regressions: `rfind('@')` returns a byte index ---

mod file_picker_multibyte {
    use super::*;
    use crate::ui::pickers::handlers::handle_file_key;
    use compact_str::CompactString;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    /// Buffer "日本語 @" + typed query, picker active with a fixed cache.
    fn picker_after_at(prefix: &str, query: &str) -> (CompactString, usize, FilePicker) {
        let mut picker = FilePicker::new();
        picker.test_set_cache(vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("README.md"),
        ]);
        let mut buffer: CompactString = format!("{prefix}@").into();
        let mut cursor = buffer.len();
        for c in query.chars() {
            assert!(handle_file_key(
                &mut buffer,
                &mut cursor,
                &mut picker,
                key(KeyCode::Char(c))
            ));
        }
        (buffer, cursor, picker)
    }

    #[test]
    fn file_picker_char_input_after_cjk_prefix_keeps_bytes_coherent() {
        let (buffer, cursor, picker) = picker_after_at("日本語 ", "ma");
        assert_eq!(buffer, "日本語 @ma");
        assert_eq!(cursor, buffer.len());
        assert_eq!(picker.query, "ma");
    }

    #[test]
    fn file_picker_backspace_on_empty_query_removes_only_the_at_after_cjk() {
        let (mut buffer, mut cursor, mut picker) = picker_after_at("日本語 ", "");
        assert!(handle_file_key(
            &mut buffer,
            &mut cursor,
            &mut picker,
            key(KeyCode::Backspace)
        ));
        // Char-based slicing with the byte index of '@' (10) kept 10 *chars*,
        // i.e. the '@' survived and text was duplicated.
        assert_eq!(buffer, "日本語 ");
        assert_eq!(cursor, buffer.len());
        assert!(buffer.is_char_boundary(cursor));
    }

    #[test]
    fn file_picker_ctrl_h_on_empty_query_removes_only_the_at_after_emoji() {
        let (mut buffer, mut cursor, mut picker) = picker_after_at("🦀 ", "");
        let ctrl_h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
        assert!(handle_file_key(
            &mut buffer,
            &mut cursor,
            &mut picker,
            ctrl_h
        ));
        assert_eq!(buffer, "🦀 ");
        assert_eq!(cursor, buffer.len());
    }

    #[test]
    fn file_picker_esc_drops_at_and_query_after_cjk() {
        let (mut buffer, mut cursor, mut picker) = picker_after_at("日本語 ", "ma");
        assert!(handle_file_key(
            &mut buffer,
            &mut cursor,
            &mut picker,
            key(KeyCode::Esc)
        ));
        assert_eq!(buffer, "日本語 ");
        assert_eq!(cursor, buffer.len());
    }

    #[test]
    fn file_picker_esc_with_multibyte_query_and_tail() {
        let mut picker = FilePicker::new();
        picker.test_set_cache(vec![PathBuf::from("café.rs")]);
        let mut buffer: CompactString = "é @ tail".into();
        let mut cursor = "é @".len();
        for c in "caf".chars() {
            handle_file_key(&mut buffer, &mut cursor, &mut picker, key(KeyCode::Char(c)));
        }
        assert_eq!(buffer, "é @caf tail");
        handle_file_key(&mut buffer, &mut cursor, &mut picker, key(KeyCode::Esc));
        assert_eq!(buffer, "é  tail");
        assert_eq!(cursor, "é ".len());
    }

    #[test]
    fn file_picker_enter_inserts_path_after_cjk_prefix() {
        let (mut buffer, mut cursor, mut picker) = picker_after_at("日本語 ", "main");
        assert_eq!(picker.selected_path(), Some(&PathBuf::from("src/main.rs")));
        assert!(handle_file_key(
            &mut buffer,
            &mut cursor,
            &mut picker,
            key(KeyCode::Enter)
        ));
        assert_eq!(buffer, "日本語 src/main.rs");
        assert_eq!(cursor, buffer.len());
    }

    #[test]
    fn file_picker_enter_keeps_multibyte_tail_after_query() {
        let mut picker = FilePicker::new();
        picker.test_set_cache(vec![PathBuf::from("README.md")]);
        let mut buffer: CompactString = "🦀 @ 日本".into();
        let mut cursor = "🦀 @".len();
        for c in "read".chars() {
            handle_file_key(&mut buffer, &mut cursor, &mut picker, key(KeyCode::Char(c)));
        }
        assert_eq!(buffer, "🦀 @read 日本");
        handle_file_key(&mut buffer, &mut cursor, &mut picker, key(KeyCode::Enter));
        assert_eq!(buffer, "🦀 README.md 日本");
        assert_eq!(cursor, "🦀 README.md".len());
    }
}

// --- slash picker interaction contract ---

mod slash_picker_contract {
    use super::*;
    use crate::ui::pickers::handlers::handle_file_key;
    use crate::ui::pickers::list::{available_commands, match_rank};
    use crate::ui::pickers::{PickerWindow, picker_window};
    use compact_str::CompactString;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// Route a key the way `App::handle_key_event` does: an active picker sees
    /// it first, and a key the picker does not consume reaches the editor.
    fn press(
        input: &mut InputEditor,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<CompactString> {
        let key = KeyEvent::new(code, modifiers);
        if input.picker.as_ref().is_some_and(Picker::active) && input.handle_picker_key(key) {
            return None;
        }
        input.handle_key(key)
    }

    fn typed(input: &mut InputEditor, text: &str) {
        for c in text.chars() {
            assert_eq!(press(input, KeyCode::Char(c), KeyModifiers::NONE), None);
        }
    }

    fn command_picker_open(input: &InputEditor) -> bool {
        matches!(input.picker.as_ref(), Some(Picker::Command(p)) if p.active)
    }

    fn highlighted(input: &InputEditor) -> Option<String> {
        match input.picker.as_ref() {
            Some(Picker::Command(p)) if p.active => p.selected_name().map(str::to_string),
            _ => None,
        }
    }

    #[test]
    fn backspace_on_the_bare_slash_removes_it_and_a_new_slash_reopens_completion() {
        for (code, modifiers) in [
            (KeyCode::Backspace, KeyModifiers::NONE),
            (KeyCode::Char('h'), KeyModifiers::CONTROL),
        ] {
            let mut input = InputEditor::new();
            typed(&mut input, "/");
            assert!(command_picker_open(&input));

            assert_eq!(press(&mut input, code, modifiers), None);
            assert_eq!(input.buffer, "");
            assert_eq!(input.cursor, 0);
            assert!(!command_picker_open(&input));

            typed(&mut input, "/mo");
            assert!(
                command_picker_open(&input),
                "a new slash must reopen completion"
            );
            assert_eq!(input.buffer, "/mo");
        }
    }

    #[test]
    fn backspace_with_a_query_keeps_the_slash_and_the_picker() {
        let mut input = InputEditor::new();
        typed(&mut input, "/m");
        press(&mut input, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(input.buffer, "/");
        assert_eq!(input.cursor, 1);
        assert!(command_picker_open(&input));
    }

    #[test]
    fn enter_on_a_command_typed_in_full_runs_it_in_one_press() {
        let mut input = InputEditor::new();
        typed(&mut input, "/help");
        assert_eq!(highlighted(&input).as_deref(), Some("/help"));

        let submitted = press(&mut input, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(submitted.as_deref(), Some("/help"));
        assert!(!command_picker_open(&input));
        assert_eq!(input.buffer, "");
    }

    #[test]
    fn enter_on_a_partial_command_inserts_the_highlight_without_submitting() {
        let mut input = InputEditor::new();
        typed(&mut input, "/hel");
        assert_eq!(press(&mut input, KeyCode::Enter, KeyModifiers::NONE), None);
        assert_eq!(input.buffer, "/help ");
        assert_eq!(input.cursor, input.buffer.len());
        assert!(!command_picker_open(&input));
    }

    #[test]
    fn enter_on_a_command_with_an_argument_picker_opens_that_picker() {
        let mut input = InputEditor::new();
        typed(&mut input, "/queue");
        assert_eq!(press(&mut input, KeyCode::Enter, KeyModifiers::NONE), None);
        assert_eq!(input.buffer, "/queue ");
        assert!(matches!(input.picker.as_ref(), Some(Picker::Prefixed(p, "/queue ")) if p.active));
    }

    #[test]
    fn tab_inserts_the_highlight_and_shift_tab_moves_it_back() {
        let mut input = InputEditor::new();
        typed(&mut input, "/re");
        assert_eq!(highlighted(&input).as_deref(), Some("/reasoning"));

        press(&mut input, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(highlighted(&input).as_deref(), Some("/redo"));
        press(&mut input, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(highlighted(&input).as_deref(), Some("/reasoning"));
        press(&mut input, KeyCode::Down, KeyModifiers::NONE);
        press(&mut input, KeyCode::Tab, KeyModifiers::SHIFT);
        assert_eq!(highlighted(&input).as_deref(), Some("/reasoning"));

        assert_eq!(press(&mut input, KeyCode::Tab, KeyModifiers::NONE), None);
        assert_eq!(input.buffer, "/reasoning ");
        assert!(!command_picker_open(&input));
    }

    #[test]
    fn tab_without_matches_leaves_the_input_and_picker_alone() {
        let mut input = InputEditor::new();
        typed(&mut input, "/zzz");
        press(&mut input, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(input.buffer, "/zzz");
        assert!(command_picker_open(&input));
    }

    #[test]
    fn tab_inserts_the_highlight_in_the_argument_picker() {
        let mut input = InputEditor::new();
        input.set_prompt_names(vec!["alpha".to_string(), "beta".to_string()]);
        typed(&mut input, "/prompt");
        press(&mut input, KeyCode::Enter, KeyModifiers::NONE);
        typed(&mut input, "be");
        press(&mut input, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(input.buffer, "/prompt beta");
        assert!(!input.picker.as_ref().is_some_and(Picker::active));
    }

    #[test]
    fn tab_inserts_the_highlighted_path_in_the_file_picker() {
        let mut picker = FilePicker::new();
        picker.test_set_cache(vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("README.md"),
        ]);
        let mut buffer: CompactString = "see @".into();
        let mut cursor = buffer.len();
        for c in "main".chars() {
            handle_file_key(
                &mut buffer,
                &mut cursor,
                &mut picker,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            );
        }
        assert!(handle_file_key(
            &mut buffer,
            &mut cursor,
            &mut picker,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
        ));
        assert_eq!(buffer, "see src/main.rs");
        assert_eq!(cursor, buffer.len());
        assert!(!picker.active);
    }

    #[test]
    fn prefix_matches_rank_before_substring_matches() {
        let mut picker = ListPicker::with_static_commands();
        picker.activate();
        for c in "re".chars() {
            picker.char_input(c);
        }
        assert_eq!(picker.selected_name(), Some("/reasoning"));
        let position = |name: &str| picker.matches.iter().position(|m| m == name).unwrap();
        assert!(position("/rewind") < position("/compress"));

        let mut picker = ListPicker::with_static_commands();
        picker.activate();
        for c in "mod".chars() {
            picker.char_input(c);
        }
        assert_eq!(&picker.matches[..2], ["/mode", "/model"]);
    }

    #[test]
    fn match_rank_orders_exact_prefix_boundary_then_substring() {
        assert_eq!(match_rank("/model", "model"), Some(0));
        assert_eq!(match_rank("/model", "/mod"), Some(1));
        assert_eq!(match_rank("/model-subagent", "sub"), Some(2));
        assert_eq!(match_rank("/compress", "re"), Some(3));
        assert_eq!(match_rank("/compress", "xyz"), None);
        assert_eq!(match_rank("Café", "caf"), Some(1));
    }

    #[test]
    fn ties_keep_the_callers_item_order() {
        let mut picker = ListPicker::new();
        picker.set_items(vec!["zeta".to_string(), "alpha".to_string()]);
        picker.activate();
        assert_eq!(picker.matches, ["zeta", "alpha"]);
    }

    #[test]
    fn command_list_is_sorted_unique_and_includes_feature_commands() {
        let commands = available_commands();
        let mut sorted = commands.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(commands, sorted);
        #[cfg(feature = "goal")]
        assert!(commands.contains(&"/goal"));
    }

    #[test]
    fn picker_window_ends_above_the_floor_and_follows_the_selection() {
        let window = |top_row, start, end| PickerWindow {
            top_row,
            start,
            end,
        };
        assert_eq!(picker_window(26, 0, 38, 0), window(16, 0, 10));
        assert_eq!(picker_window(26, 0, 3, 2), window(23, 0, 3));
        assert_eq!(picker_window(26, 0, 38, 20), window(16, 15, 25));
        assert_eq!(picker_window(4, 1, 38, 0), window(1, 0, 3));
        assert_eq!(picker_window(0, 0, 38, 5), window(0, 5, 5));
    }
}
