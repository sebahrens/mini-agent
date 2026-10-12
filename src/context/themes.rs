use std::collections::HashMap;
use std::path::PathBuf;

use include_dir::{Dir, include_dir};

use crate::config::{ColorsConfig, SchemeType};
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

pub fn regen() -> anyhow::Result<()> {
    let dir = global_dir();
    crate::context::copy_embedded_to(&EMBEDDED, &dir)
}

pub fn apply(content: &str, renderer: &mut Renderer) -> Result<(), serde_json::Error> {
    let colors = serde_json::from_str::<ColorsConfig>(content)?;
    apply_colors(&colors, renderer);
    Ok(())
}

fn apply_colors(colors: &ColorsConfig, renderer: &mut Renderer) {
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
    }
    warning
}

#[cfg(test)]
mod themes_tests {
    use super::*;
    use crossterm::style::Color;

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
