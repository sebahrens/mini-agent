use std::collections::HashMap;
use std::path::PathBuf;

use include_dir::{Dir, include_dir};

use crate::config::{ColorsConfig, SchemeType};
use crate::ui::feed::ForegroundPalette;
use crate::ui::renderer::Renderer;
use crate::ui::utils::{parse_color, to_ansi_256};

static EMBEDDED: Dir = include_dir!("$CARGO_MANIFEST_DIR/data/themes");

pub fn global_dir() -> PathBuf {
    crate::paths::process_paths()
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

pub fn apply(content: &str, renderer: &mut Renderer) -> Result<(), serde_json::Error> {
    let colors = serde_json::from_str::<ColorsConfig>(content)?;
    apply_colors(&colors, renderer);
    Ok(())
}

fn resolve_foregrounds(colors: &ColorsConfig, mut palette: ForegroundPalette) -> ForegroundPalette {
    let resolve = |value: &Option<compact_str::CompactString>, fallback| {
        let color = value.as_deref().and_then(parse_color).unwrap_or(fallback);
        if matches!(colors.scheme_type, SchemeType::Ansi) {
            to_ansi_256(color)
        } else {
            color
        }
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

fn apply_colors(colors: &ColorsConfig, renderer: &mut Renderer) {
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
    let palette = resolve_foregrounds(&defaults, ForegroundPalette::default());
    renderer.set_foreground_palette(resolve_foregrounds(colors, palette));
    let chat_bg = colors.chat_background.as_deref().and_then(parse_color);
    let input_bg = colors.input_background.as_deref().and_then(parse_color);
    let status_bg = colors.status_background.as_deref().and_then(parse_color);
    if matches!(colors.scheme_type, SchemeType::Ansi) {
        renderer.set_background_colors(
            chat_bg.map(to_ansi_256),
            input_bg.map(to_ansi_256),
            status_bg.map(to_ansi_256),
        );
    } else {
        renderer.set_background_colors(chat_bg, input_bg, status_bg);
    }
}

/// Restore the saved theme or use config colors with an actionable diagnostic.
/// A broken saved choice stays on disk so fixing its file restores it next run.
pub(crate) fn apply_initial(
    context: &mut crate::context::ContextFiles,
    colors: Option<&ColorsConfig>,
    renderer: &mut Renderer,
) -> Option<String> {
    let warning = if let Some(name) = context.current_theme_name.as_deref() {
        match context.themes.get(name) {
            Some(content) => match apply(content, renderer) {
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
        apply_colors(colors, renderer);
    } else {
        apply_colors(&ColorsConfig::default(), renderer);
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
        apply_colors(&ColorsConfig::default(), &mut renderer);
        assert_eq!(
            renderer.foreground_palette_for_test().text,
            Color::Rgb {
                r: 36,
                g: 41,
                b: 47
            }
        );
        renderer.set_terminal_background(Some(Color::Black));
        apply_colors(&ColorsConfig::default(), &mut renderer);
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
        apply_colors(&explicit, &mut renderer);
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
        apply_colors(&ColorsConfig::default(), &mut renderer);
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
        apply("{}", &mut renderer).unwrap();
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
        let error = apply("{\n  \"chat_background\":\n}", &mut renderer).unwrap_err();
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
        apply(r##"{"chat_background":"#ff0000"}"##, &mut renderer).unwrap();
        assert_eq!(
            renderer.background_colors_for_test(),
            (Some(Color::Rgb { r: 255, g: 0, b: 0 }), None, None)
        );
        apply(
            r##"{"chat_background":"#ff0000","scheme_type":"ansi"}"##,
            &mut renderer,
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
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
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
        let warning = apply_initial(&mut context, Some(&colors), &mut renderer).unwrap();
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
            apply_initial(&mut context, Some(&colors), &mut renderer)
                .unwrap()
                .contains("saved theme 'missing' is unavailable")
        );
        assert_eq!(context.current_theme_name, None);
        context.current_theme_name = Some("broken".into());
        context.themes.insert("broken".into(), "{}".into());
        assert!(apply_initial(&mut context, Some(&colors), &mut renderer).is_none());
        assert_eq!(context.current_theme_name.as_deref(), Some("broken"));
        assert_eq!(renderer.background_colors_for_test(), (None, None, None));
    }
}
