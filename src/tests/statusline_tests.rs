use crate::config::types::{StatusLineConfig, StatusLineLine, StatusLineSegment};
use crate::session::{GitStatus, Session};
use crate::ui::statusline::{self, StatusContext, StatusSpan};

fn ctx() -> StatusContext<'static> {
    StatusContext {
        workspace: std::path::Path::new("/var/lib/zerostack-demo/project"),
        loop_label: None,
        cron_label: None,
        goal_label: None,
        prompt_name: None,
        reasoning_effort: None,
        perm_mode: None,
        chain_label: None,
        background_jobs: 0,
        todos: Some((0, 0)),
        queued_inputs: 0,
        btw_cost: 0.0,
        btw_in: 0,
        btw_out: 0,
        sandbox_label: None,
    }
}

fn seg(item: &str) -> StatusLineSegment {
    StatusLineSegment {
        item: item.into(),
        ..Default::default()
    }
}

fn line_text(spans: &[StatusSpan]) -> String {
    spans
        .iter()
        .map(|s| match s {
            StatusSpan::Text { text, .. } => text.clone(),
            StatusSpan::Flex => "\u{0}FLEX\u{0}".to_string(),
        })
        .collect()
}

#[test]
fn default_statusline_shows_core_items() {
    let session = Session::new("openrouter", "deepseek/deepseek-v4-pro", 1_048_576, "");
    let lines = statusline::build_lines(&statusline::default_spec(), &session, &ctx());
    assert_eq!(lines.len(), 1);
    let text = line_text(&lines[0]);
    assert!(text.contains("deepseek/deepseek-v4-pro"), "{text}");
    assert!(text.contains("/1.0M"), "{text}"); // context max
}

#[test]
fn cache_hit_ratio_uses_normalized_prompt_tokens() {
    let mut session = Session::new("anthropic", "claude", 200_000, "");
    session.total_cached_input_tokens = 75;
    session.total_real_input_tokens = 100;
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("cache_hit_ratio")],
        }],
    };

    assert_eq!(
        line_text(&statusline::build_lines(&spec, &session, &ctx())[0]),
        "cache:75%"
    );
}

#[test]
fn background_job_count_is_visible_and_cache_bound() {
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("background_jobs")],
        }],
    };
    let session = Session::new("openrouter", "m", 1000, "");
    let empty = ctx();
    assert!(statusline::build_lines(&spec, &session, &empty)[0].is_empty());
    let empty_key = statusline::cache_key(&session, &empty);
    let mut running = ctx();
    running.background_jobs = 2;
    assert_eq!(
        line_text(&statusline::build_lines(&spec, &session, &running)[0]),
        "jobs:2"
    );
    assert!(
        line_text(&statusline::build_lines(&statusline::default_spec(), &session, &running)[0])
            .contains("jobs:2")
    );
    assert_ne!(empty_key, statusline::cache_key(&session, &running));
}

#[test]
fn flex_separator_is_preserved() {
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("model"), seg("flex_separator"), seg("context_max")],
        }],
    };
    let session = Session::new("openrouter", "m", 1000, "");
    let lines = statusline::build_lines(&spec, &session, &ctx());
    assert!(matches!(lines[0][1], StatusSpan::Flex));
}

#[test]
fn skipped_optional_item_drops_adjacent_separator() {
    // `cost` resolves to nothing (0 and not always-shown), so the separator
    // around it should be trimmed rather than left dangling.
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![
                seg("model"),
                StatusLineSegment {
                    item: "separator".into(),
                    text: Some(" | ".into()),
                    ..Default::default()
                },
                seg("cost"),
            ],
        }],
    };
    let mut session = Session::new("openrouter", "m", 1000, "");
    session.total_cost = 0.0;
    session.set_input_output_prices(Some((0.0, 0.0)));
    let lines = statusline::build_lines(&spec, &session, &ctx());
    assert_eq!(
        line_text(&lines[0]),
        "m",
        "trailing separator should be dropped"
    );
}

#[test]
fn cost_shown_when_always_flag_set() {
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("cost")],
        }],
    };
    let mut session = Session::new("openrouter", "m", 1000, "");
    session.show_cost_always = true;
    session.set_input_output_prices(Some((0.0, 0.0)));
    let lines = statusline::build_lines(&spec, &session, &ctx());
    assert_eq!(line_text(&lines[0]), "$0.0000");
}

#[test]
fn format_changes_lists_nonzero_parts() {
    let g = GitStatus {
        staged: 2,
        modified: 3,
        deleted: 0,
        untracked: 1,
        ahead: 0,
        behind: 0,
    };
    assert_eq!(statusline::format_changes(&g).as_deref(), Some("+2 ~3 ?1"));
    assert_eq!(statusline::format_changes(&GitStatus::default()), None);
}

#[test]
fn format_status_shows_sync_and_dirty() {
    let g = GitStatus {
        modified: 1,
        ahead: 2,
        behind: 1,
        ..Default::default()
    };
    assert_eq!(statusline::format_status(&g), "\u{2191}2 \u{2193}1 *");
    assert_eq!(statusline::format_status(&GitStatus::default()), "\u{2713}");
}

#[test]
fn statusline_cache_key_distinguishes_missing_and_clean_git_status() {
    let mut session = Session::new("openrouter", "m", 1000, "");
    let missing = statusline::cache_key(&session, &ctx());
    session.git_status = Some(GitStatus::default());

    assert_ne!(missing, statusline::cache_key(&session, &ctx()));
}

#[test]
fn fmt_tokens_scales() {
    assert_eq!(statusline::fmt_tokens(0), "0");
    assert_eq!(statusline::fmt_tokens(12_000), "12k");
    assert_eq!(statusline::fmt_tokens(1_048_576), "1.0M");
}

#[test]
fn powerline_glyph_resolves_names_and_passthrough() {
    assert_eq!(statusline::powerline_glyph("pl_right"), "\u{e0b0}");
    assert_eq!(statusline::powerline_glyph("round_left"), "\u{e0b6}");
    // Unknown names pass through unchanged so any literal glyph works.
    assert_eq!(statusline::powerline_glyph("\u{e0b4}"), "\u{e0b4}");
    assert_eq!(statusline::powerline_glyph(">"), ">");
}

#[test]
fn left_right_caps_wrap_the_item() {
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![StatusLineSegment {
                item: "model".into(),
                color: Some("white".into()),
                bg: Some("blue".into()),
                left: Some("pl_round_left".into()),
                right: Some("pl_round_right".into()),
                ..Default::default()
            }],
        }],
    };
    let session = Session::new("openrouter", "m", 1000, "");
    let lines = statusline::build_lines(&spec, &session, &ctx());
    assert_eq!(lines[0].len(), 3); // left cap, model, right cap
    assert_eq!(line_text(&lines[0]), "\u{e0b6}m\u{e0b4}");
}

#[test]
fn caps_skipped_when_item_is_hidden() {
    // cost resolves to nothing, so its caps must not appear either.
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![StatusLineSegment {
                item: "cost".into(),
                left: Some("pl_round_left".into()),
                right: Some("pl_round_right".into()),
                ..Default::default()
            }],
        }],
    };
    let mut session = Session::new("openrouter", "m", 1000, "");
    session.set_input_output_prices(Some((0.0, 0.0)));
    let lines = statusline::build_lines(&spec, &session, &ctx());
    assert!(lines[0].is_empty());
}

#[test]
fn light_blue_color_parses_to_rgb() {
    use crate::ui::utils::parse_color;
    assert_eq!(
        parse_color("light_blue"),
        Some(crossterm::style::Color::Rgb {
            r: 0x5f,
            g: 0xaf,
            b: 0xff
        })
    );
}

#[test]
fn auto_icon_prepends_item_glyph() {
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![StatusLineSegment {
                item: "git_branch".into(),
                icon: Some(crate::config::types::IconSpec::Auto(true)),
                ..Default::default()
            }],
        }],
    };
    let mut session = Session::new("openrouter", "m", 1000, "");
    session.git_branch = Some("main".into());
    let lines = statusline::build_lines(&spec, &session, &ctx());
    assert_eq!(line_text(&lines[0]), "\u{e0a0} main");
}

#[test]
fn custom_icon_name_resolves() {
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![StatusLineSegment {
                item: "cwd".into(),
                icon: Some(crate::config::types::IconSpec::Custom("folder".into())),
                ..Default::default()
            }],
        }],
    };
    let session = Session::new("openrouter", "m", 1000, "");
    let text = line_text(&statusline::build_lines(&spec, &session, &ctx())[0]);
    assert!(text.starts_with("\u{f07b} "), "{text}");
}

#[test]
fn item_icon_known_and_unknown() {
    assert_eq!(statusline::item_icon("git_branch"), Some("\u{e0a0}"));
    assert_eq!(statusline::item_icon("tokens_input"), None);
}

#[test]
fn cwd_full_vs_cwd_folder() {
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("cwd"), seg("separator"), seg("cwd_full")],
        }],
    };
    let mut session = Session::new("openrouter", "m", 1000, "");
    session.working_dir = "/stale/imported/workspace".into();
    let lines = statusline::build_lines(&spec, &session, &ctx());
    // cwd = folder name, cwd_full = full path (no $HOME prefix here, unchanged)
    assert_eq!(
        line_text(&lines[0]),
        "project /var/lib/zerostack-demo/project"
    );
}

#[test]
fn always_shows_zero_tokens() {
    let hidden = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("tokens_input")],
        }],
    };
    let forced = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![StatusLineSegment {
                item: "tokens_input".into(),
                always: Some(true),
                ..Default::default()
            }],
        }],
    };
    let session = Session::new("openrouter", "m", 1000, ""); // total_input_tokens == 0
    assert!(line_text(&statusline::build_lines(&hidden, &session, &ctx())[0]).is_empty());
    assert_eq!(
        line_text(&statusline::build_lines(&forced, &session, &ctx())[0]),
        "in:0"
    );
}

#[test]
fn provider_model_short_message_count() {
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![
                seg("provider"),
                seg("separator"),
                seg("model_short"),
                seg("separator"),
                seg("message_count"),
            ],
        }],
    };
    let mut session = Session::new("openrouter", "deepseek/deepseek-v4-pro", 1000, "");
    session.add_message(crate::session::MessageRole::User, "hi");
    let text = line_text(&statusline::build_lines(&spec, &session, &ctx())[0]);
    assert_eq!(text, "openrouter deepseek-v4-pro 1");
}

#[test]
fn external_item_values_cannot_drive_the_terminal_or_break_the_row() {
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("session_name"), seg("separator"), seg("git_branch")],
        }],
    };
    let mut session = Session::new("openrouter", "m", 1000, "");
    session.name = "evil\x1b]0;pwned\x07name\nsecond".into();
    session.git_branch = Some("feat\x1b[2Jx".into());
    let text = line_text(&statusline::build_lines(&spec, &session, &ctx())[0]);
    assert_eq!(text, "evilname second featx");
}

#[test]
fn reasoning_shows_only_when_enabled() {
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("reasoning")],
        }],
    };
    let mut session = Session::new("openrouter", "m", 1000, "");
    assert!(line_text(&statusline::build_lines(&spec, &session, &ctx())[0]).is_empty());
    session.reasoning_enabled = true;
    assert_eq!(
        line_text(&statusline::build_lines(&spec, &session, &ctx())[0]),
        "reasoning"
    );
}

fn first_fg(spans: &[StatusSpan]) -> Option<crossterm::style::Color> {
    spans.iter().find_map(|s| match s {
        StatusSpan::Text { fg, .. } => Some(*fg),
        StatusSpan::Flex => None,
    })?
}

#[test]
fn context_percentage_is_not_clamped() {
    // Past the window is a real over-budget state, so it must be reported
    // honestly rather than capped at 100%.
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("context_percentage")],
        }],
    };
    let mut session = Session::new("openrouter", "m", 1000, "");
    session.set_calibration(1100, 0); // 110% of the window
    assert_eq!(
        line_text(&statusline::build_lines(&spec, &session, &ctx())[0]),
        "110%"
    );
}

#[test]
fn context_percentage_color_warns_as_it_fills() {
    use crossterm::style::Color;
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("context_percentage")],
        }],
    };
    // Green tier keeps the configured (here: default/none) color.
    let mut session = Session::new("openrouter", "m", 1000, "");
    session.set_calibration(500, 0); // 50%
    assert_eq!(
        first_fg(&statusline::build_lines(&spec, &session, &ctx())[0]),
        None
    );

    // Amber as it approaches full.
    session.set_calibration(950, 0); // 95%
    assert_eq!(
        first_fg(&statusline::build_lines(&spec, &session, &ctx())[0]),
        Some(Color::Yellow)
    );

    // Red once it crosses the window.
    session.set_calibration(1100, 0); // 110%
    assert_eq!(
        first_fg(&statusline::build_lines(&spec, &session, &ctx())[0]),
        Some(Color::Red)
    );
}

#[test]
fn session_age_is_short_duration() {
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("session_age")],
        }],
    };
    let session = Session::new("openrouter", "m", 1000, ""); // created_at = now
    let text = line_text(&statusline::build_lines(&spec, &session, &ctx())[0]);
    assert!(text.ends_with('s') || text.ends_with('m'), "got {text:?}");
}

#[test]
fn the_goal_item_shows_progress_and_a_parked_status() {
    let session = Session::new("anthropic", "claude", 200_000, "");
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("goal")],
        }],
    };

    // No goal: the segment renders nothing at all.
    assert_eq!(
        line_text(&statusline::build_lines(&spec, &session, &ctx())[0]),
        "",
        "an absent goal must not occupy the status line"
    );

    let mut running = ctx();
    running.goal_label = Some("goal 3/50");
    assert!(
        line_text(&statusline::build_lines(&spec, &session, &running)[0]).contains("goal 3/50")
    );

    let mut parked = ctx();
    parked.goal_label = Some("goal 7/50 paused");
    assert!(line_text(&statusline::build_lines(&spec, &session, &parked)[0]).contains("paused"));
}

#[test]
fn goal_changes_refresh_the_cached_statusline() {
    let session = Session::new("anthropic", "claude", 200_000, "");
    let mut running = ctx();
    running.goal_label = Some("goal 3/50");
    let mut paused = ctx();
    paused.goal_label = Some("goal 3/50 paused");
    let cleared = ctx();
    let keys = [
        statusline::cache_key(&session, &running),
        statusline::cache_key(&session, &paused),
        statusline::cache_key(&session, &cleared),
    ];
    assert_ne!(keys[0], keys[1], "pause must rebuild the statusline");
    assert_ne!(keys[1], keys[2], "clear must rebuild the statusline");
    assert_ne!(keys[0], keys[2]);
}

#[test]
fn reasoning_effort_sits_next_to_the_model_when_configured() {
    let session = Session::new("openai", "gpt-5", 400_000, "");
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("reasoning_effort")],
        }],
    };
    assert!(line_text(&statusline::build_lines(&spec, &session, &ctx())[0]).is_empty());
    let mut high = ctx();
    high.reasoning_effort = Some("high");
    assert_eq!(
        line_text(&statusline::build_lines(&spec, &session, &high)[0]),
        "effort:high"
    );
    let default_line =
        line_text(&statusline::build_lines(&statusline::default_spec(), &session, &high)[0]);
    assert!(default_line.contains("gpt-5 effort:high"), "{default_line}");
    let unset_default =
        line_text(&statusline::build_lines(&statusline::default_spec(), &session, &ctx())[0]);
    assert!(!unset_default.contains("effort"), "{unset_default}");
    assert_ne!(
        statusline::cache_key(&session, &ctx()),
        statusline::cache_key(&session, &high)
    );
}

/// cfib7: a degraded default sandbox is visible for the whole session, in the
/// built-in layout and in custom layouts that never mention `sandbox`.
#[test]
fn degraded_sandbox_sets_a_persistent_red_status_segment() {
    use crate::sandbox::Sandbox;

    let degraded = Sandbox::new(false, "bwrap").with_unavailable_default_fallback();
    let label = statusline::sandbox_label(&degraded);
    assert_eq!(label, Some(statusline::SANDBOX_OFF_LABEL));
    // A deliberate `--no-sandbox` is not a degraded state.
    assert_eq!(
        statusline::sandbox_label(&Sandbox::new(false, "bwrap")),
        None
    );

    let session = Session::new("openai", "gpt-5", 400_000, "");
    let mut off = ctx();
    off.sandbox_label = label;

    let contained =
        line_text(&statusline::build_lines(&statusline::default_spec(), &session, &ctx())[0]);
    assert!(!contained.contains("sandbox"), "{contained}");
    let default_spans = &statusline::build_lines(&statusline::default_spec(), &session, &off)[0];
    assert!(
        default_spans.iter().any(|span| matches!(
            span,
            StatusSpan::Text { text, fg: Some(crossterm::style::Color::Red), .. }
                if text == "sandbox:off"
        )),
        "{default_spans:?}"
    );
    assert_eq!(line_text(default_spans).matches("sandbox:off").count(), 1);

    let custom = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("model")],
        }],
    };
    assert_eq!(
        line_text(&statusline::build_lines(&custom, &session, &off)[0]),
        "gpt-5 sandbox:off"
    );
    assert_eq!(
        line_text(&statusline::build_lines(&custom, &session, &ctx())[0]),
        "gpt-5"
    );
    let placed = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("sandbox"), seg("model")],
        }],
    };
    assert_eq!(
        line_text(&statusline::build_lines(&placed, &session, &off)[0]),
        "sandbox:offgpt-5"
    );
    assert_ne!(
        statusline::cache_key(&session, &ctx()),
        statusline::cache_key(&session, &off)
    );
}

#[test]
fn mode_cycle_status_item_shows_standard_guarded_and_yolo_and_invalidates_cache() {
    let session = Session::new("provider", "model", 128_000, "");
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("mode")],
        }],
    };
    let mut context = ctx();
    let mut previous = None;
    for mode in ["standard", "guarded", "yolo"] {
        context.perm_mode = Some(mode);
        assert_eq!(
            line_text(&statusline::build_lines(&spec, &session, &context)[0]),
            format!("mode:{mode}")
        );
        let key = statusline::cache_key(&session, &context);
        assert_ne!(Some(key), previous);
        previous = Some(key);
    }
}

#[test]
fn labels_distinguish_context_cumulative_tokens_and_unknown_cost() {
    let mut session = Session::new("p", "m", 1_000_000, "");
    session.total_input_tokens = 134_000;
    session.total_output_tokens = 1_000;
    session.total_real_input_tokens = 134_000;
    session.total_cached_input_tokens = 113_900;
    session.set_calibration(9_000, 1_000);
    let text =
        line_text(&statusline::build_lines(&statusline::default_spec(), &session, &ctx())[0]);
    assert!(text.contains("ctx 10k/1.0M 1%"), "{text}");
    assert!(text.contains("in:134k out:1k cache:85%"), "{text}");
    assert!(text.contains("cost:?"), "{text}");
    assert!(
        text.contains("calls:?"),
        "old totals cannot imply an exact call count: {text}"
    );
    assert!(!text.contains('⇑') && !text.contains('⇓'));
    session.set_input_output_prices(Some((0.0, 0.0)));
    session.show_cost_always = true;
    let free =
        line_text(&statusline::build_lines(&statusline::default_spec(), &session, &ctx())[0]);
    assert!(free.contains("$0.0000") && !free.contains("cost:?"));
}

#[test]
fn reported_calls_refresh_cache_even_with_known_free_usage() {
    let mut session = Session::new("p", "m", 1000, "");
    session.set_input_output_prices(Some((0.0, 0.0)));
    session.initialize_spend_account(None).unwrap();
    let before = statusline::cache_key(&session, &ctx());
    let receipt = crate::spend::Receipt::new(
        &session.id,
        "p",
        "m",
        crate::spend::RequestKind::Title,
        crate::event::UsageDelta {
            input_tokens: 1,
            ..Default::default()
        },
        crate::spend::PriceSnapshot::capture(
            Some((0.0, 0.0)),
            crate::pricing::CachePrices::default(),
            false,
        ),
    );
    session.spend_account.observe(&receipt).unwrap();
    assert_ne!(before, statusline::cache_key(&session, &ctx()));
    let text =
        line_text(&statusline::build_lines(&statusline::default_spec(), &session, &ctx())[0]);
    assert!(text.contains("calls:1"), "{text}");
    let same = statusline::cache_key(&session, &ctx());
    assert!(!session.spend_account.observe(&receipt).unwrap());
    assert_eq!(same, statusline::cache_key(&session, &ctx()));
}

#[test]
fn default_narrow_layout_preserves_complete_safety_and_prompt_items() {
    let mut session = Session::new("p", "provider/very-long-model-name", 1_000_000, "");
    session.total_input_tokens = 134_000;
    session.total_output_tokens = 1_000;
    session.git_branch = Some("feature/long-branch-name".into());
    let mut context = ctx();
    context.prompt_name = Some("code");
    context.perm_mode = Some("yolo");
    let spans = &statusline::build_lines(&statusline::default_spec(), &session, &context)[0];
    assert_eq!(statusline::fit_line(spans, 500), *spans);
    let fitted = statusline::fit_line(spans, 59);
    let text = line_text(&fitted);
    assert!(
        text.contains("prompt:code") && text.contains("mode:yolo"),
        "{text}"
    );
    assert!(
        !text.contains("in:134k") && !text.contains("out:1k"),
        "{text}"
    );
    assert!(!text.contains("prompt:cod\0"));
    let visible: String = fitted
        .iter()
        .filter_map(|span| match span {
            StatusSpan::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(crate::ui::utils::display_width(&visible) <= 59);
    // An extremely long surviving safety item is visibly marked, even at 30 columns.
    context.sandbox_label = Some("sandbox:off-unavailable-backend-with-long-label");
    let spans = &statusline::build_lines(&statusline::default_spec(), &session, &context)[0];
    let text = line_text(&statusline::fit_line(spans, 29)).replace("\0FLEX\0", "");
    assert!(text.ends_with('…'), "{text}");
    assert!(crate::ui::utils::display_width(&text) <= 29);
}

#[test]
fn custom_priority_compact_forms_and_unicode_are_cell_bounded_and_inert() {
    let mut model = seg("model");
    model.priority = Some(200);
    model.min = Some("m\x1b[2J\n".into());
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![
                model,
                seg("separator"),
                seg("tokens_input"),
                seg("separator"),
                seg("prompt"),
            ],
        }],
    };
    let mut session = Session::new("p", "very-long-model", 1000, "");
    session.total_input_tokens = 1000;
    let mut context = ctx();
    context.prompt_name = Some("e\u{301}界😀");
    let spans = &statusline::build_lines(&spec, &session, &context)[0];
    let compact = line_text(&statusline::fit_line(spans, 15));
    assert!(compact.contains("prompt:e\u{301}界😀"), "{compact}");
    assert!(!compact.contains('\x1b') && !compact.contains('\n'));
    for width in 0..=40 {
        let fitted = statusline::fit_line(spans, width);
        let text = line_text(&fitted).replace("\0FLEX\0", "");
        assert!(
            crate::ui::utils::display_width(&text) <= width,
            "{width}: {text}"
        );
        if width == 0 {
            assert!(text.is_empty());
        }
    }
    assert_eq!(statusline::ellipsize("界界界", 4), "界…");
    assert_eq!(statusline::ellipsize("e\u{301}long", 2), "e\u{301}…");
}

#[test]
fn safety_state_cannot_be_hidden_by_a_compact_form_or_low_priority() {
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![
                StatusLineSegment {
                    item: "model".into(),
                    priority: Some(255),
                    ..Default::default()
                },
                seg("separator"),
                StatusLineSegment {
                    item: "sandbox".into(),
                    priority: Some(0),
                    min: Some("".into()),
                    ..Default::default()
                },
            ],
        }],
    };
    let session = Session::new("p", "a-very-long-model-name", 1000, "");
    let mut context = ctx();
    context.sandbox_label = Some("sandbox:off");
    let spans = &statusline::build_lines(&spec, &session, &context)[0];
    assert_eq!(line_text(&statusline::fit_line(spans, 11)), "sandbox:off");
}

#[test]
fn imported_history_and_rekeyed_accounts_keep_call_count_incomplete() {
    let mut session = Session::new("p", "m", 1000, "");
    session.add_message(
        crate::session::MessageRole::Assistant,
        "imported reply with missing usage",
    );
    session.initialize_spend_account(None).unwrap();
    assert_eq!(session.spend_account.model_call_count(), Some((0, false)));
    let receipt = crate::spend::Receipt::new(
        &session.id,
        "p",
        "m",
        crate::spend::RequestKind::Turn,
        crate::event::UsageDelta {
            input_tokens: 1,
            ..Default::default()
        },
        crate::spend::PriceSnapshot::capture(
            Some((0.0, 0.0)),
            crate::pricing::CachePrices::default(),
            false,
        ),
    );
    session.spend_account.observe(&receipt).unwrap();
    session.id = "forked-logical-session".into();
    session.initialize_spend_account(None).unwrap();
    assert_eq!(session.spend_account.model_call_count(), Some((1, false)));
    let text =
        line_text(&statusline::build_lines(&statusline::default_spec(), &session, &ctx())[0]);
    assert!(text.contains("calls:≥1"));
}

#[test]
fn combined_context_warning_color_survives_its_compact_form() {
    let mut session = Session::new("p", "m", 1000, "");
    session.overhead_tokens = 1100;
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("context")],
        }],
    };
    let spans = &statusline::build_lines(&spec, &session, &ctx())[0];
    let compact = statusline::fit_line(spans, 8);
    assert_eq!(line_text(&compact), "ctx 110%");
    assert_eq!(first_fg(&compact), Some(crossterm::style::Color::Red));
}

#[test]
fn cache_only_and_subagent_only_legacy_history_never_claims_zero_calls() {
    for variant in 0..4 {
        let mut session = Session::new("p", "m", 1000, "");
        match variant {
            0 => session.total_cached_input_tokens = 100,
            1 => session.total_cache_creation_input_tokens = 100,
            2 => session.total_cost = 0.1,
            _ => session.add_message(
                crate::session::MessageRole::SubagentToolCall,
                "historical nested request",
            ),
        }
        let spec = StatusLineConfig {
            lines: vec![StatusLineLine {
                segments: vec![seg("calls")],
            }],
        };
        assert_eq!(
            line_text(&statusline::build_lines(&spec, &session, &ctx())[0]),
            "calls:?"
        );
        session.initialize_spend_account(None).unwrap();
        assert_eq!(session.spend_account.model_call_count(), Some((0, false)));
        assert_eq!(
            line_text(&statusline::build_lines(&spec, &session, &ctx())[0]),
            "calls:?"
        );
    }
}

#[test]
fn busy67_numeric_progress_is_sticky_and_invalidates_cached_status() {
    let session = Session::new("provider", "model", 128_000, "");
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("todos"), seg("queue")],
        }],
    };
    let mut live = ctx();
    let empty = statusline::cache_key(&session, &live);
    assert!(statusline::build_lines(&spec, &session, &live)[0].is_empty());
    live.todos = Some((2, 3));
    live.queued_inputs = 4;
    assert_ne!(empty, statusline::cache_key(&session, &live));
    let text = line_text(&statusline::build_lines(&spec, &session, &live)[0]);
    assert!(text.contains("todo:2/3") && text.contains("queue:4"));
    let before = statusline::cache_key(&session, &live);
    live.todos = None;
    assert_ne!(before, statusline::cache_key(&session, &live));
    assert!(line_text(&statusline::build_lines(&spec, &session, &live)[0]).contains("todo:?"));
    let before = statusline::cache_key(&session, &live);
    live.queued_inputs = 0;
    assert_ne!(before, statusline::cache_key(&session, &live));
    assert!(!line_text(&statusline::build_lines(&spec, &session, &live)[0]).contains("queue:"));
}

#[test]
fn profile100_host_full_is_a_persistent_red_status_segment() {
    let sandbox = crate::sandbox::Sandbox::new(true, "bwrap").with_general_profile(
        crate::sandbox::SandboxProfile::HostFull,
        Vec::new(),
        Vec::new(),
    );
    let label = statusline::sandbox_label(&sandbox);
    assert_eq!(label, Some("sandbox:host-full"));
    let session = Session::new("openai", "fixture", 128_000, "");
    let mut current = ctx();
    current.sandbox_label = label;
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("model")],
        }],
    };
    let spans = &statusline::build_lines(&spec, &session, &current)[0];
    assert!(spans.iter().any(|span| matches!(span, StatusSpan::Text {text, fg:Some(crossterm::style::Color::Red), ..} if text == "sandbox:host-full")));
    assert_eq!(line_text(spans).matches("sandbox:host-full").count(), 1);
}

fn cron_summary(armed: usize, next_in: Option<u64>) -> crate::ui::cron::CronSummary {
    crate::ui::cron::CronSummary {
        armed,
        next_in: next_in.map(std::time::Duration::from_secs),
        running: 0,
        parked: 0,
        suspended: false,
    }
}

#[test]
fn the_cron_item_shows_the_countdown_and_each_held_state() {
    let session = Session::new("anthropic", "claude", 200_000, "");
    let spec = StatusLineConfig {
        lines: vec![StatusLineLine {
            segments: vec![seg("cron")],
        }],
    };
    let render = |label: Option<&str>| {
        let mut ctx = ctx();
        ctx.cron_label = label;
        line_text(&statusline::build_lines(&spec, &session, &ctx)[0])
    };

    assert_eq!(statusline::cron_label(cron_summary(0, None), false), None);
    assert_eq!(render(None), "", "no schedules must not occupy the line");

    let armed = statusline::cron_label(cron_summary(2, Some(210)), false);
    assert_eq!(armed.as_deref(), Some("⟳ 2 in 4m"));
    assert_eq!(render(armed.as_deref()), "⟳ 2 in 4m");
    assert_eq!(
        statusline::cron_label(cron_summary(1, Some(20)), false).as_deref(),
        Some("⟳ 1 in <1m")
    );
    assert_eq!(
        statusline::cron_label(cron_summary(1, Some(3900)), false).as_deref(),
        Some("⟳ 1 in 1h5m")
    );

    let parked = crate::ui::cron::CronSummary {
        parked: 1,
        ..cron_summary(2, Some(120))
    };
    assert_eq!(
        statusline::cron_label(parked, false).as_deref(),
        Some("⟳ 2 parked")
    );
    // A scheduled turn holding a prompt outranks a parked fire.
    assert_eq!(
        statusline::cron_label(parked, true).as_deref(),
        Some("⟳ 2 waiting")
    );
    let suspended = crate::ui::cron::CronSummary {
        suspended: true,
        ..parked
    };
    let suspended = statusline::cron_label(suspended, true);
    assert_eq!(suspended.as_deref(), Some("⟳ 2 suspended"));
    assert_eq!(render(suspended.as_deref()), "⟳ 2 suspended");
}

#[test]
fn the_cron_item_shows_a_running_self_paced_fire_and_then_its_wakeup_countdown() {
    // In flight: the entry has no deadline until its turn calls
    // `schedule_wakeup`, so the item says `running` rather than `armed`.
    let running = crate::ui::cron::CronSummary {
        running: 1,
        ..cron_summary(1, None)
    };
    assert_eq!(
        statusline::cron_label(running, false).as_deref(),
        Some("⟳ 1 running")
    );
    // A prompt held by that turn still reads `waiting`.
    assert_eq!(
        statusline::cron_label(running, true).as_deref(),
        Some("⟳ 1 waiting")
    );
    // Once the wakeup is chosen it is an ordinary countdown.
    let mut store = crate::ui::cron::CronStore::default();
    let now = tokio::time::Instant::now();
    store.arm_dynamic("watch".into(), now).unwrap();
    store.begin_fire("c1", now, 24).unwrap();
    assert_eq!(
        statusline::cron_label(store.summary(now), false).as_deref(),
        Some("⟳ 1 running")
    );
    store.schedule_wakeup("c1", std::time::Duration::from_secs(300), now);
    assert_eq!(
        statusline::cron_label(store.summary(now), false).as_deref(),
        Some("⟳ 1 in 5m")
    );
}

#[test]
fn the_cron_item_refreshes_only_when_its_minute_bucket_changes() {
    let session = Session::new("anthropic", "claude", 200_000, "");
    let key = |seconds: u64| {
        let label = statusline::cron_label(cron_summary(1, Some(seconds)), false);
        let mut ctx = ctx();
        ctx.cron_label = label.as_deref();
        statusline::cache_key(&session, &ctx)
    };
    // 4m00s and 3m01s both round up to `in 4m`: the cached spans are reused.
    assert_eq!(key(240), key(181));
    // Crossing into the next minute bucket rebuilds them.
    assert_ne!(key(181), key(180));
    // Arming the first schedule and clearing the last both rebuild.
    assert_ne!(key(240), statusline::cache_key(&session, &ctx()));
}

#[test]
fn the_default_layout_places_the_cron_item() {
    let session = Session::new("anthropic", "claude", 200_000, "");
    let mut ctx = ctx();
    ctx.cron_label = Some("⟳ 1 in 5m");
    let text = line_text(&statusline::build_lines(&statusline::default_spec(), &session, &ctx)[0]);
    assert!(text.contains("⟳ 1 in 5m"), "{text}");
    assert_eq!(statusline::item_icon("cron"), Some("\u{f073}"));
}

#[test]
fn the_cron_countdown_repaints_exactly_when_its_minute_bucket_rolls_over() {
    use std::time::Duration;
    let repaint = |left: u64| statusline::cron_label_repaint_in(cron_summary(1, Some(left)), false);
    // 4m30s shows "in 5m" until 4m00s left, so it repaints in 30s.
    assert_eq!(repaint(270), Some(Duration::from_secs(30)));
    // Exactly on a boundary the next change is a full minute away.
    assert_eq!(repaint(240), Some(Duration::from_secs(60)));
    // After the repaint the label really changed bucket.
    let label = |left: u64| statusline::cron_label(cron_summary(1, Some(left)), false);
    assert_ne!(label(270), label(240));
    // Under a minute the label stays "in <1m" until the deadline fires.
    assert_eq!(repaint(59), None);
    // No countdown shown: no repaint timer.
    assert_eq!(
        statusline::cron_label_repaint_in(cron_summary(0, None), false),
        None
    );
    assert_eq!(
        statusline::cron_label_repaint_in(cron_summary(1, Some(600)), true),
        None
    );
    let parked = crate::ui::cron::CronSummary {
        parked: 1,
        ..cron_summary(1, Some(600))
    };
    assert_eq!(statusline::cron_label_repaint_in(parked, false), None);
    let suspended = crate::ui::cron::CronSummary {
        suspended: true,
        ..cron_summary(1, Some(600))
    };
    assert_eq!(statusline::cron_label_repaint_in(suspended, false), None);
}
