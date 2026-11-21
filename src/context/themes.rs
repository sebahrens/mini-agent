use std::collections::HashMap;
use std::path::PathBuf;

use include_dir::{Dir, include_dir};

use crate::config::{ColorsConfig, SchemeType};
use crate::ui::feed::ForegroundPalette;
use crate::ui::input::InputEditor;
use crate::ui::renderer::Renderer;
use crate::ui::utils::{parse_color, to_ansi_256};

static EMBEDDED: Dir = include_dir!("$CARGO_MANIFEST_DIR/data/themes");

pub fn global_dir() -> PathBuf {
    crate::paths::process_paths_ref()
        .expect("startup must initialize application paths")
        .themes_dir()
}

pub fn load() -> HashMap<String, String> {
    let mut themes: HashMap<String, String> = HashMap::new();

    for (name, content) in crate::context::load_embedded_files(&EMBEDDED, "json") {
        themes.entry(name).or_insert(content);
    }
    for (name, content) in crate::context::load_dir_files(&global_dir(), "json") {
        themes.insert(name, content);
    }
    themes
}

pub fn ensure_global() -> anyhow::Result<()> {
    let dir = global_dir();
    if !dir.exists() {
        crate::context::copy_embedded_to(&EMBEDDED, &dir)?;
    }
    Ok(())
}

pub(crate) fn regen() -> anyhow::Result<crate::context::RegenReport> {
    crate::context::regen_embedded_to(&EMBEDDED, &global_dir())
}

/// (embedded file count, user-edited file names) for the global directory.
pub(crate) fn regen_preview() -> (usize, Vec<String>) {
    crate::context::edited_embedded_files(&EMBEDDED, &global_dir())
}

pub fn apply(
    content: &str,
    renderer: &mut Renderer,
    input: &mut InputEditor,
) -> Result<(), serde_json::Error> {
    let colors = serde_json::from_str::<ColorsConfig>(content)?;
    apply_colors(&colors, renderer, input);
    Ok(())
}

/// How many colours the terminal can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorDepth {
    TrueColor,
    Ansi256,
    Ansi16,
}

impl ColorDepth {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::TrueColor => "truecolor (24-bit)",
            Self::Ansi256 => "256 colours",
            Self::Ansi16 => "16 colours",
        }
    }
}

/// Decide the colour depth from the environment (`get` reads one variable).
/// Truecolor needs a positive sign: `COLORTERM=truecolor|24bit`, a `direct`
/// terminfo, a terminal known to support it, or Windows Terminal. Otherwise
/// `256color` in `TERM` gives 256 colours and anything else 16.
pub(crate) fn detect_color_depth(get: impl Fn(&str) -> Option<String>) -> ColorDepth {
    let term = get("TERM").unwrap_or_default().to_ascii_lowercase();
    let colorterm = get("COLORTERM").unwrap_or_default().to_ascii_lowercase();
    let program = get("TERM_PROGRAM").unwrap_or_default();
    let truecolor = matches!(colorterm.as_str(), "truecolor" | "24bit")
        || term.contains("direct")
        || ["kitty", "wezterm", "alacritty", "ghostty"]
            .iter()
            .any(|name| term.contains(name))
        || matches!(program.as_str(), "iTerm.app" | "WezTerm" | "ghostty" | "vscode")
        || get("WT_SESSION").is_some_and(|value| !value.is_empty())
        // A Windows console without TERM is a modern conhost.
        || (cfg!(windows) && term.is_empty());
    if truecolor {
        ColorDepth::TrueColor
    } else if term.contains("256color") {
        ColorDepth::Ansi256
    } else {
        ColorDepth::Ansi16
    }
}

#[cfg(test)]
thread_local! {
    static DEPTH_OVERRIDE: std::cell::Cell<Option<ColorDepth>> = const { std::cell::Cell::new(None) };
}

/// The detected depth. Unit tests see truecolor unless they override it.
pub(crate) fn color_depth() -> ColorDepth {
    #[cfg(test)]
    {
        DEPTH_OVERRIDE
            .with(|depth| depth.get())
            .unwrap_or(ColorDepth::TrueColor)
    }
    #[cfg(not(test))]
    {
        static DEPTH: std::sync::OnceLock<ColorDepth> = std::sync::OnceLock::new();
        *DEPTH.get_or_init(|| detect_color_depth(|name| std::env::var(name).ok()))
    }
}

/// Nearest of the 16 terminal colours.
fn to_ansi_16(color: crossterm::style::Color) -> crossterm::style::Color {
    use crossterm::style::Color;
    const TABLE: [(u8, u8, u8, Color); 16] = [
        (0, 0, 0, Color::Black),
        (128, 0, 0, Color::DarkRed),
        (0, 128, 0, Color::DarkGreen),
        (128, 128, 0, Color::DarkYellow),
        (0, 0, 128, Color::DarkBlue),
        (128, 0, 128, Color::DarkMagenta),
        (0, 128, 128, Color::DarkCyan),
        (192, 192, 192, Color::Grey),
        (128, 128, 128, Color::DarkGrey),
        (255, 0, 0, Color::Red),
        (0, 255, 0, Color::Green),
        (255, 255, 0, Color::Yellow),
        (0, 0, 255, Color::Blue),
        (255, 0, 255, Color::Magenta),
        (0, 255, 255, Color::Cyan),
        (255, 255, 255, Color::White),
    ];
    let Color::Rgb { r, g, b } = color else {
        return color;
    };
    let distance = |(tr, tg, tb): (u8, u8, u8)| {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
        d(r, tr) + d(g, tg) + d(b, tb)
    };
    TABLE
        .iter()
        .min_by_key(|(tr, tg, tb, _)| distance((*tr, *tg, *tb)))
        .map_or(color, |(_, _, _, named)| *named)
}

/// Bring `color` to what the terminal can show: untouched on truecolor, the
/// nearest 256- or 16-colour value otherwise. A theme with
/// `scheme_type = "ansi"` always uses 256 colours.
fn downgrade(
    color: crossterm::style::Color,
    scheme: &SchemeType,
    depth: ColorDepth,
) -> crossterm::style::Color {
    match depth {
        ColorDepth::Ansi16 => to_ansi_16(color),
        ColorDepth::Ansi256 => to_ansi_256(color),
        ColorDepth::TrueColor if matches!(scheme, SchemeType::Ansi) => to_ansi_256(color),
        ColorDepth::TrueColor => color,
    }
}

fn resolve_foregrounds(
    colors: &ColorsConfig,
    mut palette: ForegroundPalette,
    depth: ColorDepth,
) -> ForegroundPalette {
    let resolve = |value: &Option<compact_str::CompactString>, fallback| {
        let color = value.as_deref().and_then(parse_color).unwrap_or(fallback);
        downgrade(color, &colors.scheme_type, depth)
    };
    palette.user = resolve(&colors.user, palette.user);
    palette.agent = resolve(&colors.agent, palette.agent);
    palette.tool = resolve(&colors.tool, palette.tool);
    palette.result = resolve(&colors.result, palette.result);
    palette.code = resolve(&colors.code, palette.code);
    palette.reasoning = resolve(&colors.reasoning, palette.reasoning);
    palette.error = resolve(&colors.error, palette.error);
    palette.warning = resolve(&colors.warning, palette.warning);
    palette.diff_add = resolve(&colors.diff_add, palette.diff_add);
    palette.diff_del = resolve(&colors.diff_del, palette.diff_del);
    palette.status = resolve(&colors.status, palette.status);
    palette.system = resolve(&colors.system, palette.system);
    palette.permission = resolve(&colors.permission, palette.permission);
    palette.link = resolve(&colors.link, palette.link);
    palette.text = resolve(&colors.text, palette.text);
    palette
}

pub fn apply_colors(colors: &ColorsConfig, renderer: &mut Renderer, input: &mut InputEditor) {
    apply_colors_at_depth(colors, renderer, input, color_depth());
}

fn apply_colors_at_depth(
    colors: &ColorsConfig,
    renderer: &mut Renderer,
    input: &mut InputEditor,
    depth: ColorDepth,
) {
    let light = colors
        .chat_background
        .as_deref()
        .and_then(parse_color)
        .filter(|color| *color != crossterm::style::Color::Reset)
        .map(crate::ui::terminal::background_color_is_light)
        .or_else(|| {
            renderer
                .terminal_background()
                .map(crate::ui::terminal::background_color_is_light)
        })
        .or_else(|| {
            std::env::var("COLORFGBG")
                .ok()
                .and_then(|hint| crate::ui::terminal::background_hint_is_light(&hint))
        })
        .unwrap_or(false);
    let defaults: ColorsConfig = serde_json::from_str(if light {
        include_str!("../../data/themes/default-light.json")
    } else {
        include_str!("../../data/themes/default-dark.json")
    })
    .expect("embedded foreground palette must be valid");
    let palette = resolve_foregrounds(&defaults, ForegroundPalette::default(), depth);
    renderer.set_foreground_palette(resolve_foregrounds(colors, palette, depth));
    let chat_bg = colors.chat_background.as_deref().and_then(parse_color);
    let input_bg = colors.input_background.as_deref().and_then(parse_color);
    let status_bg = colors.status_background.as_deref().and_then(parse_color);
    let accent = colors.accent.as_deref().and_then(parse_color);
    let convert = |color: Option<crossterm::style::Color>| {
        color.map(|color| downgrade(color, &colors.scheme_type, depth))
    };
    renderer.set_background_colors(convert(chat_bg), convert(input_bg), convert(status_bg));
    // The theme accent recolours the welcome banner and picker highlights;
    // both the renderer and the input pickers must see the same value.
    let accent = convert(accent);
    renderer.set_accent_color(accent);
    input.set_accent_color(accent);
}

/// Restore the saved theme or use config colors with an actionable diagnostic.
/// A broken saved choice stays on disk so fixing its file restores it next run.
pub(crate) fn apply_initial(
    context: &mut crate::context::ContextFiles,
    colors: Option<&ColorsConfig>,
    renderer: &mut Renderer,
    input: &mut InputEditor,
) -> Option<String> {
    let warning = if let Some(name) = context.current_theme_name.as_deref() {
        match context.themes.get(name) {
            Some(content) => match apply(content, renderer, input) {
                Ok(()) => return None,
                Err(error) => Some(format!(
                    "could not apply saved theme '{name}': {error}; using config colors"
                )),
            },
            None => Some(format!(
                "saved theme '{name}' is unavailable; using config colors"
            )),
        }
    } else {
        None
    };
    if warning.is_some() {
        context.current_theme_name = None;
    }
    if let Some(colors) = colors {
        apply_colors(colors, renderer, input);
    } else {
        apply_colors(&ColorsConfig::default(), renderer, input);
    }
    warning
}

#[cfg(test)]
mod themes_tests {
    use super::*;
    use crossterm::style::Color;

    #[test]
    fn themes_default_uses_retained_osc_background_and_explicit_colors_win() {
        let mut renderer = Renderer::for_app_test().unwrap();
        renderer.set_terminal_background(Some(Color::White));
        apply_colors(
            &ColorsConfig::default(),
            &mut renderer,
            &mut InputEditor::new(),
        );
        assert_eq!(
            renderer.foreground_palette_for_test().text,
            Color::Rgb {
                r: 36,
                g: 41,
                b: 47
            }
        );
        renderer.set_terminal_background(Some(Color::Black));
        apply_colors(
            &ColorsConfig::default(),
            &mut renderer,
            &mut InputEditor::new(),
        );
        assert_eq!(
            renderer.foreground_palette_for_test().text,
            Color::Rgb {
                r: 230,
                g: 237,
                b: 243
            }
        );
        let explicit = ColorsConfig {
            chat_background: Some("#ffffff".into()),
            text: Some("#123456".into()),
            ..ColorsConfig::default()
        };
        apply_colors(&explicit, &mut renderer, &mut InputEditor::new());
        let palette = renderer.foreground_palette_for_test();
        assert_eq!(
            palette.tool,
            Color::Rgb {
                r: 137,
                g: 80,
                b: 0
            }
        );
        assert_eq!(
            palette.text,
            Color::Rgb {
                r: 18,
                g: 52,
                b: 86
            }
        );
        // Reset/reload reuses the renderer's terminal hint instead of querying
        // beside the running input reader. Named full palettes remain explicit.
        apply(
            include_str!("../../data/themes/default-light.json"),
            &mut renderer,
            &mut InputEditor::new(),
        )
        .unwrap();
        assert_eq!(
            renderer.foreground_palette_for_test().text,
            Color::Rgb {
                r: 36,
                g: 41,
                b: 47
            }
        );
        apply_colors(
            &ColorsConfig::default(),
            &mut renderer,
            &mut InputEditor::new(),
        );
        assert_eq!(
            renderer.foreground_palette_for_test().text,
            Color::Rgb {
                r: 230,
                g: 237,
                b: 243
            }
        );
    }

    #[test]
    fn themes_foregrounds_repaint_cached_roles_and_markdown_without_changing_text() {
        use crate::ui::feed::BlockStyle;
        let mut renderer = Renderer::for_app_test().unwrap();
        for (style, text) in [
            (BlockStyle::User, "user"),
            (BlockStyle::Tool, "tool"),
            (BlockStyle::ToolResult, "result"),
            (BlockStyle::DiffAdd, "+added"),
            (BlockStyle::DiffDel, "-removed"),
            (BlockStyle::Error, "error"),
            (BlockStyle::Agent, "answer\n\n```rust\nlet value = 1;\n```"),
        ] {
            renderer.feed_mut().push_block(style, text);
        }
        let before = renderer.feed().lines(80);
        assert!(std::sync::Arc::ptr_eq(&before, &renderer.feed().lines(80)));
        apply(
            r##"{"user":"#010203","tool":"#020304","result":"#030405",
            "diff_add":"#040506","diff_del":"#050607","error":"#060708",
            "agent":"#070809","code":"#08090a"}"##,
            &mut renderer,
            &mut InputEditor::new(),
        )
        .unwrap();
        let after = renderer.feed().lines(80);
        assert!(!std::sync::Arc::ptr_eq(&before, &after));
        assert_eq!(before.len(), after.len());
        for index in 0..before.len() {
            assert_eq!(before[index].text, after[index].text);
        }
        for (text, color) in [
            ("user", Color::Rgb { r: 1, g: 2, b: 3 }),
            ("tool", Color::Rgb { r: 2, g: 3, b: 4 }),
            ("result", Color::Rgb { r: 3, g: 4, b: 5 }),
            ("+added", Color::Rgb { r: 4, g: 5, b: 6 }),
            ("-removed", Color::Rgb { r: 5, g: 6, b: 7 }),
            ("error", Color::Rgb { r: 6, g: 7, b: 8 }),
            ("answer", Color::Rgb { r: 7, g: 8, b: 9 }),
            ("let value", Color::Rgb { r: 8, g: 9, b: 10 }),
        ] {
            assert!(
                (0..after.len())
                    .any(|index| after[index].text.contains(text) && after[index].color == color),
                "{text}"
            );
        }
        assert!(std::sync::Arc::ptr_eq(&after, &renderer.feed().lines(80)));
    }

    #[test]
    fn themes_light_hint_explicit_background_and_role_overrides_have_ordered_precedence() {
        let _environment =
            crate::tests::ScopedProcessEnv::set(&[("COLORFGBG", Some("0;15".into()))]);
        let mut renderer = Renderer::for_app_test().unwrap();
        apply("{}", &mut renderer, &mut InputEditor::new()).unwrap();
        let light = renderer.foreground_palette_for_test();
        assert_eq!(
            light.agent,
            Color::Rgb {
                r: 36,
                g: 41,
                b: 47
            }
        );
        apply(
            r##"{"chat_background":"#000000","agent":"#112233"}"##,
            &mut renderer,
            &mut InputEditor::new(),
        )
        .unwrap();
        let explicit = renderer.foreground_palette_for_test();
        assert_eq!(
            explicit.agent,
            Color::Rgb {
                r: 17,
                g: 34,
                b: 51
            }
        );
        assert_eq!(
            explicit.result,
            Color::Rgb {
                r: 171,
                g: 178,
                b: 191
            }
        );
        apply(
            r##"{"chat_background":"#ffffff","agent":"#ff0000","scheme_type":"ansi"}"##,
            &mut renderer,
            &mut InputEditor::new(),
        )
        .unwrap();
        assert_eq!(
            renderer.foreground_palette_for_test().agent,
            Color::AnsiValue(196)
        );
    }

    #[test]
    fn themes_monochrome_output_does_not_emit_rgb_foregrounds_or_backgrounds() {
        let mut renderer = Renderer::for_app_test().unwrap();
        renderer.set_monochrome(true);
        apply(
            include_str!("../../data/themes/default-light.json"),
            &mut renderer,
            &mut InputEditor::new(),
        )
        .unwrap();
        renderer
            .feed_mut()
            .push_block(crate::ui::feed::BlockStyle::Agent, "answer");
        renderer.render_viewport().unwrap();
        let captured = renderer.captured_output();
        assert!(
            !captured.contains("38;2;") && !captured.contains("48;2;"),
            "{captured}"
        );
        assert!(crate::ui::events::sanitize_output(&captured).contains("answer"));
    }

    #[test]
    fn themes_embedded_palettes_are_parseable_and_cover_foreground_roles() {
        for (_, content) in crate::context::load_embedded_files(&EMBEDDED, "json") {
            let colors: ColorsConfig = serde_json::from_str(&content).unwrap();
            for role in [
                &colors.user,
                &colors.agent,
                &colors.tool,
                &colors.result,
                &colors.code,
                &colors.reasoning,
                &colors.error,
                &colors.diff_add,
                &colors.diff_del,
                &colors.status,
                &colors.system,
                &colors.permission,
                &colors.link,
                &colors.text,
            ] {
                assert!(role.as_deref().and_then(parse_color).is_some());
            }
        }
    }

    #[test]
    fn themes_invalid_json_reports_location_and_preserves_renderer() {
        let mut renderer = Renderer::new().unwrap();
        renderer.set_background_colors(Some(Color::Blue), None, Some(Color::Red));
        let error = apply(
            "{\n  \"chat_background\":\n}",
            &mut renderer,
            &mut InputEditor::new(),
        )
        .unwrap_err();
        assert_eq!((error.line(), error.column()), (3, 1));
        assert!(
            error
                .to_string()
                .contains("expected value at line 3 column 1")
        );
        assert_eq!(
            renderer.background_colors_for_test(),
            (Some(Color::Blue), None, Some(Color::Red))
        );
    }

    #[test]
    fn themes_valid_json_applies_full_and_ansi_colors() {
        let mut renderer = Renderer::new().unwrap();
        apply(
            r##"{"chat_background":"#ff0000"}"##,
            &mut renderer,
            &mut InputEditor::new(),
        )
        .unwrap();
        assert_eq!(
            renderer.background_colors_for_test(),
            (Some(Color::Rgb { r: 255, g: 0, b: 0 }), None, None)
        );
        apply(
            r##"{"chat_background":"#ff0000","scheme_type":"ansi"}"##,
            &mut renderer,
            &mut InputEditor::new(),
        )
        .unwrap();
        assert_eq!(
            renderer.background_colors_for_test(),
            (Some(Color::AnsiValue(196)), None, None)
        );
    }

    #[test]
    fn themes_broken_saved_selection_warns_and_uses_config_without_rewriting_choice() {
        let root = crate::tests::OwnedTestDirectory::new("broken-startup-theme");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "MINI_AGENT_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        std::fs::create_dir_all(global_dir()).unwrap();
        std::fs::write(
            global_dir().join("broken.json"),
            "{\n  \"chat_background\":\n}",
        )
        .unwrap();
        crate::session::storage::save_theme_name(Some("broken")).unwrap();
        let mut context = crate::context::load(true);
        assert_eq!(context.current_theme_name.as_deref(), Some("broken"));
        let colors = ColorsConfig {
            chat_background: Some("#ff0000".into()),
            scheme_type: SchemeType::Ansi,
            ..Default::default()
        };
        let mut renderer = Renderer::new().unwrap();
        let warning = apply_initial(
            &mut context,
            Some(&colors),
            &mut renderer,
            &mut InputEditor::new(),
        )
        .unwrap();
        assert!(warning.contains("could not apply saved theme 'broken'"));
        assert!(warning.contains("line 3 column 1"));
        assert_eq!(context.current_theme_name, None);
        assert_eq!(
            renderer.background_colors_for_test(),
            (Some(Color::AnsiValue(196)), None, None)
        );
        assert_eq!(
            crate::session::storage::load_theme_name().as_deref(),
            Some("broken")
        );
        context.current_theme_name = Some("missing".into());
        assert!(
            apply_initial(
                &mut context,
                Some(&colors),
                &mut renderer,
                &mut InputEditor::new(),
            )
            .unwrap()
            .contains("saved theme 'missing' is unavailable")
        );
        assert_eq!(context.current_theme_name, None);
        context.current_theme_name = Some("broken".into());
        context.themes.insert("broken".into(), "{}".into());
        assert!(
            apply_initial(
                &mut context,
                Some(&colors),
                &mut renderer,
                &mut InputEditor::new(),
            )
            .is_none()
        );
        assert_eq!(context.current_theme_name.as_deref(), Some("broken"));
        assert_eq!(renderer.background_colors_for_test(), (None, None, None));
    }

    #[test]
    fn theme_accent_reaches_renderer_and_input_pickers() {
        use crate::ui::renderer::LineEntry;

        let mut renderer = Renderer::new().expect("renderer");
        let mut input = InputEditor::new();

        apply(
            r##"{"accent":"#FD5108","chat_background":"#111113"}"##,
            &mut renderer,
            &mut input,
        )
        .unwrap();

        let orange_rgb = Color::Rgb {
            r: 0xfd,
            g: 0x51,
            b: 0x08,
        };
        let orange = Some(orange_rgb);
        assert_eq!(renderer.accent_color(), orange);
        assert_eq!(input.accent_color(), orange);

        let welcome = LineEntry {
            text: "welcome".into(),
            color: Color::Cyan,
            theme_accent: true,
            cont: crate::ui::renderer::Cont::None,
            lead: 0,
            syntax_spans: None,
        };
        let markdown_heading = LineEntry {
            text: "heading".into(),
            color: Color::Cyan,
            theme_accent: false,
            cont: crate::ui::renderer::Cont::None,
            lead: 0,
            syntax_spans: None,
        };
        assert_eq!(renderer.line_color_for_test(&welcome), orange_rgb);
        assert_eq!(
            renderer.line_color_for_test(&markdown_heading),
            Color::Cyan,
            "unrelated cyan content must retain its semantic color"
        );
    }
}

#[cfg(test)]
mod color_depth_tests {
    use super::*;
    use crossterm::style::Color;

    fn depth(vars: &[(&str, &str)]) -> ColorDepth {
        detect_color_depth(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn depth_detection_table() {
        use ColorDepth::*;
        assert_eq!(
            depth(&[("COLORTERM", "truecolor"), ("TERM", "xterm")]),
            TrueColor
        );
        assert_eq!(depth(&[("COLORTERM", "24bit")]), TrueColor);
        assert_eq!(depth(&[("TERM", "xterm-direct")]), TrueColor);
        assert_eq!(depth(&[("TERM", "xterm-kitty")]), TrueColor);
        assert_eq!(
            depth(&[("TERM", "xterm-256color"), ("TERM_PROGRAM", "iTerm.app")]),
            TrueColor
        );
        assert_eq!(
            depth(&[("TERM", "xterm-256color"), ("WT_SESSION", "x")]),
            TrueColor
        );
        assert_eq!(depth(&[("TERM", "xterm-256color")]), Ansi256);
        assert_eq!(
            depth(&[("TERM", "screen-256color"), ("COLORTERM", "")]),
            Ansi256
        );
        if !cfg!(windows) {
            assert_eq!(depth(&[("TERM", "linux")]), Ansi16);
            assert_eq!(depth(&[]), Ansi16);
        }
    }

    #[test]
    fn a_256_colour_terminal_gets_no_rgb_in_the_palette() {
        for forced in [ColorDepth::Ansi256, ColorDepth::Ansi16] {
            let mut renderer = Renderer::for_app_test().unwrap();
            apply_colors_at_depth(
                &ColorsConfig {
                    chat_background: Some("#101010".into()),
                    accent: Some("#ff8800".into()),
                    ..Default::default()
                },
                &mut renderer,
                &mut InputEditor::new(),
                forced,
            );
            let palette = renderer.foreground_palette_for_test();
            for color in [
                palette.text,
                palette.user,
                palette.agent,
                palette.error,
                palette.link,
            ] {
                assert!(!matches!(color, Color::Rgb { .. }), "{forced:?}: {color:?}");
            }
        }
    }
}
