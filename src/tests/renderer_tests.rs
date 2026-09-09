use std::cell::{Cell, RefCell};

use crate::ui::renderer::{
    MAX_CLIPBOARD_BYTES, base64_encode, dispatch_windows_open, is_nul_terminated_utf16,
    is_safe_url, normalize_internal_clipboard_newlines, normalize_windows_clipboard_newlines,
    osc52_request, validate_clipboard_text, windows_open_request,
};

#[cfg(unix)]
mod clipboard_process_tests {
    use crate::ui::renderer::run_clipboard_command;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    use std::path::PathBuf;
    use std::time::Duration;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("clipboard-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn command(&self, script: &str) -> tokio::process::Command {
            let mut command = tokio::process::Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    &format!("echo $$ > \"$1\"; {script}"),
                    "clipboard-fixture",
                ])
                .arg(self.0.join("pid"))
                .arg(self.0.join("input"))
                .arg(self.0.join("owner.pid"))
                .arg(self.0.join("request"))
                .arg(self.0.join("served"))
                .current_dir(&self.0);
            command
        }

        fn pid(&self, name: &str) -> Option<i32> {
            std::fs::read_to_string(self.0.join(name))
                .ok()?
                .trim()
                .parse()
                .ok()
        }

        async fn started(&self) -> i32 {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Some(pid) = self.pid("pid") {
                        return pid;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("clipboard fixture must start")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(pid) = self.pid("pid") {
                crate::sandbox::kill_process_group_if_live(pid as u32);
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn assert_reaped(pid: i32) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while kill(Pid::from_raw(pid), None).is_ok() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("clipboard helper must be reaped");
    }

    #[tokio::test]
    async fn clipboard_helpers_receive_exact_input_and_keep_successful_owners() {
        for (input, owner) in [
            ("", false),
            ("snowman ☃\nsecond line", false),
            ("owned", true),
        ] {
            let fixture = Fixture::new();
            let script = if owner {
                "/bin/cat > \"$2\"; (while [ ! -f \"$4\" ]; do /bin/sleep 0.01; done; echo served > \"$5\") </dev/null >/dev/null 2>&1 & echo $! > \"$3\""
            } else {
                "/bin/cat > \"$2\""
            };
            assert!(
                run_clipboard_command(fixture.command(script), input, Duration::from_secs(2)).await
            );
            assert_eq!(
                std::fs::read_to_string(fixture.0.join("input")).unwrap(),
                input
            );
            assert_reaped(fixture.started().await).await;
            if owner {
                fixture.pid("owner.pid").expect("clipboard owner published");
                // Require work after the copy completes: kill(pid, 0) alone
                // would also accept a recently killed zombie owner.
                std::fs::write(fixture.0.join("request"), "copy completed").unwrap();
                tokio::time::timeout(Duration::from_secs(3), async {
                    while !fixture.0.join("served").exists() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("successful owner must still serve the clipboard");
            }
        }
    }

    #[tokio::test]
    async fn clipboard_helpers_bound_input_exit_and_reap_failed_or_cancelled_copies() {
        let input = "x".repeat(1024 * 1024);
        let missing = Fixture::new();
        assert!(
            !run_clipboard_command(
                tokio::process::Command::new(missing.0.join("missing-helper")),
                &input,
                Duration::from_secs(2),
            )
            .await
        );
        assert!(missing.pid("pid").is_none());
        for (mode, script) in [
            ("input-stall", "exec /bin/sleep 30"),
            (
                "exit-stall",
                "/bin/cat >/dev/null; /bin/sleep 30 & echo $! > \"$3\"; wait",
            ),
            ("closed-input", "exec 0<&-; exec /bin/sleep 30"),
            ("nonzero", "/bin/cat >/dev/null; exit 7"),
            ("cancel", "exec /bin/sleep 30"),
        ] {
            let fixture = Fixture::new();
            if mode == "cancel" {
                {
                    let copy = run_clipboard_command(
                        fixture.command(script),
                        &input,
                        Duration::from_secs(30),
                    );
                    tokio::pin!(copy);
                    tokio::select! {
                        result = &mut copy => panic!("copy completed before cancellation: {result}"),
                        _ = fixture.started() => {}
                    }
                }
            } else {
                let result = tokio::time::timeout(
                    Duration::from_secs(3),
                    run_clipboard_command(
                        fixture.command(script),
                        &input,
                        Duration::from_millis(250),
                    ),
                )
                .await
                .expect("clipboard transfer must be bounded");
                assert!(!result, "{mode} must not report a confirmed copy");
            }
            assert_reaped(fixture.started().await).await;
            if let Some(descendant) = fixture.pid("owner.pid") {
                assert_reaped(descendant).await;
            }
        }
    }
}

#[test]
fn base64_encode_exact_vectors_cover_padding_and_binary_alphabet() {
    for (input, expected) in [
        (b"".as_slice(), ""),
        (b"f".as_slice(), "Zg=="),
        (b"fo".as_slice(), "Zm8="),
        (b"foo".as_slice(), "Zm9v"),
        (b"foob".as_slice(), "Zm9vYg=="),
        (b"fooba".as_slice(), "Zm9vYmE="),
        (b"foobar".as_slice(), "Zm9vYmFy"),
        (b"\x00\x80\xff".as_slice(), "AID/"),
        (b"\xfb\xef\xbe".as_slice(), "++++"),
        (b"\xff\xff\xff".as_slice(), "////"),
    ] {
        assert_eq!(base64_encode(input), expected, "input: {input:?}");
    }
}

#[test]
fn base64_encode_long_input_preserves_every_block_without_line_wrapping() {
    let mut input = b"abc".repeat(256);
    input.push(b'd');
    let expected = "YWJj".repeat(256) + "ZA==";
    assert_eq!(base64_encode(&input), expected);
}

#[test]
fn clipboard_text_validation_rejects_nul_and_oversize() {
    assert!(validate_clipboard_text("before\0after").is_err());
    let oversized = "x".repeat(MAX_CLIPBOARD_BYTES / 2);
    assert!(validate_clipboard_text(&oversized).is_err());
}

#[test]
fn windows_clipboard_newline_codec_preserves_internal_lf_text() {
    for internal in ["one\ntwo", "one\r\ntwo", "one\rtwo", "no newline"] {
        let windows = normalize_windows_clipboard_newlines(internal);
        let remainder = windows.replace("\r\n", "");
        assert!(!remainder.contains('\r') && !remainder.contains('\n'));
        assert_eq!(
            normalize_internal_clipboard_newlines(windows),
            internal.replace("\r\n", "\n").replace('\r', "\n")
        );
    }
}

#[test]
fn osc52_fallback_is_an_exact_unconfirmed_request() {
    assert_eq!(
        osc52_request("snowman ☃\nline 2"),
        "\x1b]52;c;c25vd21hbiDimIMKbGluZSAy\x07"
    );
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
    use crate::ui::feed::{BlockStyle, Feed};
    use crate::ui::renderer::{BottomRedrawPlan, BottomSnapshot, PromptSnapshot, Renderer};

    fn bottom_snapshot() -> BottomSnapshot {
        BottomSnapshot {
            cols: 80,
            rows: 24,
            statusline_height: 1,
            input_hash: 0,
            cursor_pos: 0,
            is_running: false,
            spinner_frame: 0,
            input_vscroll_offset: 0,
            prompt: PromptSnapshot::Input,
            statusline_key: 0,
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
    fn feed_retention_resets_scroll_and_selection_indices() {
        let mut r = Renderer::new().unwrap();
        let (max_blocks, _, _) = Feed::retention_limits_for_test();
        for index in 0..max_blocks {
            r.feed_mut()
                .push_line(BlockStyle::Plain, format!("line {index}"));
        }
        r.scroll_line_up();
        assert!(r.is_scrolling());
        r.selection_active = true;
        r.selection_start = Some(2);
        r.selection_end = Some(4);
        r.feed_mut().push_line(BlockStyle::Plain, "evicts line 0");

        r.reconcile_feed_retention();

        assert!(!r.is_scrolling());
        assert!(!r.selection_active);
        assert_eq!(r.selection_start, None);
        assert_eq!(r.selection_end, None);
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
        next.statusline_key = 1;
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
        next.input_hash = 1;
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
        next.input_hash = 1;
        next.statusline_key = 1;
        assert_eq!(
            Renderer::bottom_redraw_plan(Some(&prev), &next, false),
            BottomRedrawPlan::Full
        );
    }

    #[test]
    fn statusline_cache_reuses_spans_until_its_input_key_changes() {
        let mut renderer = Renderer::new().unwrap();
        let first = renderer.cached_statusline(7, || vec![Vec::new()]);
        let second = renderer.cached_statusline(7, || panic!("cache hit rebuilt statusline"));
        let third = renderer.cached_statusline(8, || vec![Vec::new(), Vec::new()]);

        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert!(!std::sync::Arc::ptr_eq(&second, &third));
        assert_eq!(renderer.statusline_builds(), 2);
    }
}

// --- scroll / input-row arithmetic must never underflow ---

mod viewport_math {
    use crate::ui::renderer::{clamp_scroll_offset, input_top_row, scroll_percent};

    #[test]
    fn clamp_scroll_offset_stays_within_scrollable_range() {
        assert_eq!(clamp_scroll_offset(0, 100, 20), 0);
        assert_eq!(clamp_scroll_offset(80, 100, 20), 80);
        // Viewport grew after scrolling to the top (input shrank via Ctrl+U):
        // the old offset exceeds the new range and would have underflowed.
        assert_eq!(clamp_scroll_offset(80, 100, 40), 60);
        // Content fits entirely: nothing to scroll.
        assert_eq!(clamp_scroll_offset(5, 10, 20), 0);
        assert_eq!(clamp_scroll_offset(usize::MAX, 0, 0), 0);
    }

    #[test]
    fn scroll_percent_is_saturating_and_bounded() {
        assert_eq!(scroll_percent(0, 100, 20), 100);
        assert_eq!(scroll_percent(80, 100, 20), 0);
        assert_eq!(scroll_percent(40, 100, 20), 50);
        // Stale offset larger than the range (the debug-panic case).
        assert_eq!(scroll_percent(80, 100, 40), 0);
        assert_eq!(scroll_percent(usize::MAX, 100, 40), 0);
        // Viewport taller than the content.
        assert_eq!(scroll_percent(3, 10, 20), 0);
        assert_eq!(scroll_percent(3, 0, 0), 0);
        for offset in 0..=200usize {
            let pct = scroll_percent(offset, 150, 30);
            assert!(pct <= 100, "offset {offset} -> {pct}");
        }
    }

    #[test]
    fn input_top_row_saturates_on_short_terminals() {
        // 24 rows, 1 reserved, 1-line input -> input on row 23.
        assert_eq!(input_top_row(24, 1, 1), 23);
        assert_eq!(input_top_row(24, 1, 5), 19);
        // rows <= reserve used to underflow u16.
        assert_eq!(input_top_row(1, 1, 1), 1);
        assert_eq!(input_top_row(0, 3, 1), 1);
        assert_eq!(input_top_row(2, 1, 5), 1);
        assert_eq!(input_top_row(24, 1, usize::MAX), 1);
    }
}
