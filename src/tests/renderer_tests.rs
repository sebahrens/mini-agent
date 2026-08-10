use std::cell::{Cell, RefCell};

use crate::ui::renderer::{
    base64_encode, copy_to_clipboard, dispatch_windows_open, is_nul_terminated_utf16, is_safe_url,
    windows_open_request,
};

#[test]
fn base64_encode_empty() {
    assert_eq!(base64_encode(b""), "");
}

#[test]
fn base64_encode_single_byte() {
    assert_eq!(base64_encode(b"f"), "Zg==");
}

#[test]
fn base64_encode_two_bytes() {
    assert_eq!(base64_encode(b"fo"), "Zm8=");
}

#[test]
fn base64_encode_three_bytes() {
    assert_eq!(base64_encode(b"foo"), "Zm9v");
}

#[test]
fn base64_encode_known_values() {
    assert_eq!(base64_encode(b"Hello"), "SGVsbG8=");
    assert_eq!(base64_encode(b"Hi!"), "SGkh");
    assert_eq!(base64_encode(b"ab"), "YWI=");
    assert_eq!(base64_encode(b"abc"), "YWJj");
    assert_eq!(base64_encode(b"Man"), "TWFu");
}

#[test]
fn base64_encode_long_input() {
    let input = "The quick brown fox jumps over the lazy dog. ".repeat(10);
    let encoded = base64_encode(input.as_bytes());
    assert!(encoded.len() > input.len());
    assert!(encoded.ends_with('=') || !encoded.contains('='));
}

#[test]
fn copy_to_clipboard_does_not_panic() {
    // Succeeds via an external tool or the OSC 52 fallback.
    copy_to_clipboard("test text").expect("copy should succeed");
}

#[test]
fn copy_to_clipboard_empty_string() {
    copy_to_clipboard("").expect("copy should succeed");
}

#[test]
fn safe_url_accepts_http_and_https() {
    assert!(is_safe_url("https://example.com"));
    assert!(is_safe_url("http://example.com/path?q=1#frag"));
    assert!(is_safe_url("https://user@example.com:8080/x"));
    assert!(is_safe_url(
        "https://example.com/a path?q=hello world#part two"
    ));
    assert!(is_safe_url("https://例え.テスト/資料?q=雪"));
}

#[test]
fn safe_url_rejects_non_http_schemes() {
    assert!(!is_safe_url("file:///etc/passwd"));
    assert!(!is_safe_url("javascript:alert(1)"));
    assert!(!is_safe_url("ftp://example.com"));
    assert!(!is_safe_url("example.com/no-scheme"));
    assert!(!is_safe_url(""));
}

#[test]
fn safe_url_rejects_missing_host() {
    assert!(!is_safe_url("https://"));
    assert!(!is_safe_url("http:///path"));
}

#[test]
fn safe_url_rejects_host_whitespace_and_non_space_control_chars() {
    assert!(!is_safe_url("https://exa mple.com/path"));
    assert!(!is_safe_url("https://example.com/a\tb"));
    assert!(!is_safe_url("https://example.com/\nevil"));
    assert!(!is_safe_url("https://example.com/\x07"));
}

#[test]
fn safe_url_rejects_overlong_urls() {
    let long = format!("https://example.com/{}", "a".repeat(2100));
    assert!(!is_safe_url(&long));
}

fn decoded_windows_target(url: &str) -> String {
    let request = windows_open_request(url).expect("valid URL should produce a launch request");
    assert_eq!(
        request.target.last(),
        Some(&0),
        "target must be NUL terminated"
    );
    assert_eq!(
        request.target.iter().filter(|&&unit| unit == 0).count(),
        1,
        "target must contain exactly one terminating NUL"
    );
    String::from_utf16(&request.target[..request.target.len() - 1]).unwrap()
}

#[test]
fn windows_open_request_preserves_safe_urls_as_one_data_target() {
    let urls = [
        "https://example.com/a path?q=hello world#part two",
        "https://例え.テスト/資料?q=雪#章",
        "https://example.com/path?first=1&second=2",
        "https://example.com/a|b^c(d)e",
        "https://example.com/%26%7C%5E%22%28%29%25",
        "https://example.com/path?q=\"quoted\"&rate=100%",
    ];

    for url in urls {
        let request = windows_open_request(url).expect("URL should be accepted");
        assert_eq!(
            request.verb,
            "open".encode_utf16().chain([0]).collect::<Vec<_>>()
        );
        assert_eq!(decoded_windows_target(url), url);
    }
}

#[test]
fn windows_open_request_has_no_shell_or_extra_command_channel() {
    let sentinel = "MINI_AGENT_URL_OPENER_SENTINEL";
    let url = format!(
        "https://example.com/path?x=1&echo {sentinel}|powershell^(Write-Output '{sentinel}'^)%25"
    );
    let request = windows_open_request(&url).expect("metacharacters remain safe URL data");

    assert_eq!(decoded_windows_target(&url), url);
    assert_eq!(request.parameters, None);
    assert_eq!(request.verb_text(), "open");
}

#[test]
fn windows_open_request_rejects_before_constructing_an_os_request() {
    for url in [
        "file:///C:/Windows/System32/calc.exe",
        "javascript:alert(1)",
        "https://example.com/path\0sentinel",
        "https://example.com/path\r\ncmd",
    ] {
        assert!(
            windows_open_request(url).is_none(),
            "unexpectedly accepted: {url:?}"
        );
    }
}

#[derive(Debug, PartialEq)]
struct CapturedWindowsOpen {
    operation: Vec<u16>,
    file: Vec<u16>,
    parameters: Option<Vec<u16>>,
    directory: Option<Vec<u16>>,
}

#[test]
fn windows_open_request_dispatch_passes_sentinel_url_only_as_shell_execute_file_data() {
    let sentinel = "MINI_AGENT_DISPATCH_SENTINEL";
    let url = format!("https://example.com/a&b|c^d?q=\"{sentinel}\"%25#(fragment)");
    let captured = RefCell::new(None);

    dispatch_windows_open(&url, |operation, file, parameters, directory| {
        captured.replace(Some(CapturedWindowsOpen {
            operation: operation.to_vec(),
            file: file.to_vec(),
            parameters: parameters.map(<[u16]>::to_vec),
            directory: directory.map(<[u16]>::to_vec),
        }));
        33
    })
    .expect("ShellExecuteW success code should pass");

    assert_eq!(
        captured.into_inner().unwrap(),
        CapturedWindowsOpen {
            operation: "open".encode_utf16().chain([0]).collect(),
            file: url.encode_utf16().chain([0]).collect(),
            parameters: None,
            directory: None,
        }
    );
}

#[test]
fn windows_open_request_dispatch_maps_shell_execute_boundary_codes() {
    for code in [33, isize::MAX] {
        assert!(
            dispatch_windows_open("https://example.com", |_, _, _, _| code).is_ok(),
            "code {code} should indicate success"
        );
    }

    for code in [isize::MIN, 0, 1, 31, 32] {
        let error = dispatch_windows_open("https://example.com", |_, _, _, _| code)
            .expect_err("code at or below 32 should fail");
        assert!(error.to_string().contains(&code.to_string()));
    }
}

#[test]
fn windows_open_request_dispatch_rejects_before_calling_shell_execute() {
    let called = Cell::new(false);
    let result = dispatch_windows_open("javascript:alert(1)", |_, _, _, _| {
        called.set(true);
        33
    });

    assert!(result.is_err());
    assert!(!called.get());
}

#[test]
fn windows_open_request_ffi_strings_require_one_trailing_nul() {
    assert!(is_nul_terminated_utf16(&[0]));
    assert!(is_nul_terminated_utf16(
        &"open".encode_utf16().chain([0]).collect::<Vec<_>>()
    ));

    assert!(!is_nul_terminated_utf16(&[]));
    assert!(!is_nul_terminated_utf16(&[b'o' as u16]));
    assert!(!is_nul_terminated_utf16(&[b'o' as u16, 0, b'p' as u16, 0,]));
}

#[test]
fn windows_open_request_source_has_no_cmd_interpreter_fallback() {
    let source: String = include_str!("../ui/renderer.rs")
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    for forbidden in [
        "command::new(\"cmd\")",
        "command::new(\"cmd.exe\")",
        "\"cmd\",&[\"/c\",\"start\"",
    ] {
        assert!(
            !source.contains(forbidden),
            "Windows URL opener must not reintroduce command interpreter syntax: {forbidden}"
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_open_request_shell_execute_uses_file_target_without_parameters() {
    let url = "https://example.com/a&b|c^d(quoted)%25?q=\"value\"#fragment";
    let request = windows_open_request(url).unwrap();

    assert_eq!(decoded_windows_target(url), url);
    assert!(request.parameters.is_none());
}

#[test]
fn chat_margin_reduces_content_width() {
    let mut r = crate::ui::renderer::Renderer::new().unwrap();
    let full = r.line_width();
    r.set_chat_margin(4);
    assert_eq!(r.line_width(), full.saturating_sub(4));
    // Zero margin leaves the width unchanged.
    r.set_chat_margin(0);
    assert_eq!(r.line_width(), full);
}

mod dirty {
    use crate::ui::feed::BlockStyle;
    use crate::ui::renderer::{BottomRedrawPlan, BottomSnapshot, PromptSnapshot, Renderer};
    use crate::ui::statusline::StatusSpan;

    fn bottom_snapshot() -> BottomSnapshot {
        BottomSnapshot {
            cols: 80,
            rows: 24,
            statusline_height: 1,
            input: String::new(),
            cursor_pos: 0,
            is_running: false,
            spinner_frame: 0,
            input_vscroll_offset: 0,
            prompt: PromptSnapshot::Input,
            statusline: vec![vec![StatusSpan::Text {
                text: "model".to_string(),
                fg: None,
                bg: None,
            }]],
            scroll_indicator: false,
            monochrome: false,
            input_bg: None,
            status_bg: None,
        }
    }

    #[test]
    fn fresh_renderer_needs_chat_redraw() {
        let r = Renderer::new().unwrap();
        assert!(r.chat_needs_redraw());
    }

    #[test]
    fn chat_clean_after_mark_clean() {
        let mut r = Renderer::new().unwrap();
        r.mark_chat_clean();
        assert!(!r.chat_needs_redraw());
    }

    #[test]
    fn feed_mut_mutation_triggers_chat_redraw() {
        let mut r = Renderer::new().unwrap();
        r.mark_chat_clean();
        r.feed_mut().push_block(BlockStyle::Plain, "hello");
        assert!(r.chat_needs_redraw());
    }

    #[test]
    fn scroll_triggers_chat_redraw() {
        let mut r = Renderer::new().unwrap();
        let visible = r.visible_lines();
        for i in 0..=visible {
            r.feed_mut()
                .push_line(BlockStyle::Plain, format!("line {i}"));
        }
        r.mark_chat_clean();
        assert!(!r.chat_needs_redraw());
        r.scroll_line_up();
        assert!(r.chat_needs_redraw());
    }

    #[test]
    fn no_op_scroll_does_not_trigger_chat_redraw() {
        let mut r = Renderer::new().unwrap();
        r.feed_mut().push_line(BlockStyle::Plain, "one line");
        r.mark_chat_clean();

        r.scroll_line_up();

        assert!(!r.chat_needs_redraw());
    }

    #[test]
    fn resize_marks_chat_dirty() {
        let mut r = Renderer::new().unwrap();
        r.mark_chat_clean();
        r.resize();
        assert!(r.chat_needs_redraw());
    }

    #[test]
    fn selection_change_triggers_chat_redraw() {
        let mut r = Renderer::new().unwrap();
        r.feed_mut().push_line(BlockStyle::Plain, "selectable");
        r.mark_chat_clean();
        assert!(!r.chat_needs_redraw());
        // Selection fields are public and mutated directly by callers.
        r.selection_active = true;
        r.selection_start = Some(0);
        r.selection_end = Some(0);
        assert!(r.chat_needs_redraw());
        r.mark_chat_clean();
        r.clear_selection();
        assert!(r.chat_needs_redraw());
    }

    #[test]
    fn invalidate_marks_chat_dirty() {
        let mut r = Renderer::new().unwrap();
        r.mark_chat_clean();
        r.invalidate();
        assert!(r.chat_needs_redraw());
    }

    #[test]
    fn bottom_plan_full_when_no_previous() {
        let next = bottom_snapshot();
        assert_eq!(
            Renderer::bottom_redraw_plan(None, &next, false),
            BottomRedrawPlan::Full
        );
    }

    #[test]
    fn bottom_plan_skip_when_unchanged() {
        let prev = bottom_snapshot();
        let next = bottom_snapshot();
        assert_eq!(
            Renderer::bottom_redraw_plan(Some(&prev), &next, false),
            BottomRedrawPlan::Skip
        );
    }

    #[test]
    fn bottom_plan_force_full() {
        let prev = bottom_snapshot();
        let next = bottom_snapshot();
        assert_eq!(
            Renderer::bottom_redraw_plan(Some(&prev), &next, true),
            BottomRedrawPlan::Full
        );
    }

    #[test]
    fn bottom_plan_statusline_only_on_statusline_change() {
        let prev = bottom_snapshot();
        let mut next = bottom_snapshot();
        next.statusline = vec![vec![StatusSpan::Text {
            text: "other model".to_string(),
            fg: None,
            bg: None,
        }]];
        assert_eq!(
            Renderer::bottom_redraw_plan(Some(&prev), &next, false),
            BottomRedrawPlan::StatuslineOnly
        );
    }

    #[test]
    fn bottom_plan_statusline_only_on_scroll_indicator_change() {
        let prev = bottom_snapshot();
        let mut next = bottom_snapshot();
        next.scroll_indicator = true;
        assert_eq!(
            Renderer::bottom_redraw_plan(Some(&prev), &next, false),
            BottomRedrawPlan::StatuslineOnly
        );
    }

    #[test]
    fn bottom_plan_full_on_input_change() {
        let prev = bottom_snapshot();
        let mut next = bottom_snapshot();
        next.input = "typed".to_string();
        assert_eq!(
            Renderer::bottom_redraw_plan(Some(&prev), &next, false),
            BottomRedrawPlan::Full
        );
    }

    #[test]
    fn bottom_plan_full_on_cursor_change() {
        let prev = bottom_snapshot();
        let mut next = bottom_snapshot();
        next.cursor_pos = 3;
        assert_eq!(
            Renderer::bottom_redraw_plan(Some(&prev), &next, false),
            BottomRedrawPlan::Full
        );
    }

    #[test]
    fn bottom_plan_full_on_prompt_mode_change() {
        let prev = bottom_snapshot();
        let mut next = bottom_snapshot();
        next.prompt = PromptSnapshot::Chain {
            question: "continue?".into(),
            but_mode: false,
        };
        assert_eq!(
            Renderer::bottom_redraw_plan(Some(&prev), &next, false),
            BottomRedrawPlan::Full
        );
    }

    #[test]
    fn bottom_plan_full_on_geometry_change() {
        let prev = bottom_snapshot();
        let mut next = bottom_snapshot();
        next.rows = 40;
        assert_eq!(
            Renderer::bottom_redraw_plan(Some(&prev), &next, false),
            BottomRedrawPlan::Full
        );
    }

    #[test]
    fn bottom_plan_full_on_spinner_frame_change() {
        let prev = bottom_snapshot();
        let mut next = bottom_snapshot();
        next.is_running = true;
        next.spinner_frame = 1;
        assert_eq!(
            Renderer::bottom_redraw_plan(Some(&prev), &next, false),
            BottomRedrawPlan::Full
        );
    }

    #[test]
    fn bottom_plan_full_on_input_scroll_change() {
        let prev = bottom_snapshot();
        let mut next = bottom_snapshot();
        next.input_vscroll_offset = 1;
        assert_eq!(
            Renderer::bottom_redraw_plan(Some(&prev), &next, false),
            BottomRedrawPlan::Full
        );
    }

    #[test]
    fn bottom_plan_full_when_statusline_and_input_change() {
        let prev = bottom_snapshot();
        let mut next = bottom_snapshot();
        next.input = "typed".to_string();
        next.statusline = Vec::new();
        assert_eq!(
            Renderer::bottom_redraw_plan(Some(&prev), &next, false),
            BottomRedrawPlan::Full
        );
    }
}
