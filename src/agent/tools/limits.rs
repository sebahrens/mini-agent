//! Process-wide output and traversal bounds for the built-in tools.
//!
//! The `[tool_limits]` config table resolves into [`ToolLimits`]. Agent
//! construction installs the resolved value with [`install`]; tools, the
//! pre-model spill hook and transcript replay read it with [`current`]. Unit
//! tests never install a value, so they observe the documented defaults and
//! exercise non-default bounds through the explicit-parameter helpers.

use std::sync::RwLock;

use crate::config::Config;

/// Default final backstop for one self-bounded tool result, in characters
/// (roughly 25k tokens).
pub const DEFAULT_MAX_RESULT_CHARS: usize = 100_000;
/// Smallest accepted backstop; smaller values would leave no useful head/tail.
pub const MIN_MAX_RESULT_CHARS: usize = 4_000;
/// Default display width for one `read` line before it is clipped.
pub const DEFAULT_MAX_READ_LINE_CHARS: usize = 2_000;
/// Smallest accepted read line width.
pub const MIN_MAX_READ_LINE_CHARS: usize = 80;
/// Default raw-size limit for notebook views and edits, independent of
/// `max_text_file_size` (image outputs make executed notebooks large).
pub const DEFAULT_NOTEBOOK_MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolLimits {
    /// Final character backstop for a self-bounded tool result.
    pub max_result_chars: usize,
    /// Characters of one `read` line shown before a clip marker.
    pub max_read_line_chars: usize,
    /// Raw notebook size accepted by the cell view and `notebook_edit`
    /// (never above the 64 MiB hard ceiling).
    pub notebook_max_bytes: u64,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            max_result_chars: DEFAULT_MAX_RESULT_CHARS,
            max_read_line_chars: DEFAULT_MAX_READ_LINE_CHARS,
            notebook_max_bytes: DEFAULT_NOTEBOOK_MAX_BYTES,
        }
    }
}

fn to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

impl ToolLimits {
    pub fn from_config(cfg: &Config) -> Self {
        let defaults = Self::default();
        let Some(table) = cfg.tool_limits.as_ref() else {
            return defaults;
        };
        Self {
            max_result_chars: table
                .max_result_chars
                .map_or(defaults.max_result_chars, to_usize)
                .max(MIN_MAX_RESULT_CHARS),
            max_read_line_chars: table
                .max_read_line_chars
                .map_or(defaults.max_read_line_chars, to_usize)
                .max(MIN_MAX_READ_LINE_CHARS),
            notebook_max_bytes: table
                .notebook_max_bytes
                .unwrap_or(defaults.notebook_max_bytes)
                .min(super::notebook::MAX_NOTEBOOK_BYTES),
        }
    }

    /// Characters kept from the start of an over-backstop result.
    pub fn backstop_head_chars(&self) -> usize {
        self.max_result_chars / 4
    }

    /// Characters kept from the end of an over-backstop result. Head, tail and
    /// the truncation notice together stay below `max_result_chars`.
    pub fn backstop_tail_chars(&self) -> usize {
        self.max_result_chars / 2
    }
}

static CURRENT: RwLock<ToolLimits> = RwLock::new(ToolLimits {
    max_result_chars: DEFAULT_MAX_RESULT_CHARS,
    max_read_line_chars: DEFAULT_MAX_READ_LINE_CHARS,
    notebook_max_bytes: DEFAULT_NOTEBOOK_MAX_BYTES,
});

/// Install the limits resolved from the active configuration.
pub fn install(limits: ToolLimits) {
    *CURRENT.write().unwrap_or_else(|error| error.into_inner()) = limits;
}

/// The limits currently in force.
pub fn current() -> ToolLimits {
    *CURRENT.read().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_limits_default_and_resolve_from_config() {
        assert_eq!(
            ToolLimits::from_config(&Config::default()),
            ToolLimits::default()
        );
        let cfg: Config = toml::from_str("[tool_limits]\nmax_result_chars = 30000\n").unwrap();
        assert_eq!(ToolLimits::from_config(&cfg).max_result_chars, 30_000);
        let tiny: Config = toml::from_str("[tool_limits]\nmax_result_chars = 1\n").unwrap();
        assert_eq!(
            ToolLimits::from_config(&tiny).max_result_chars,
            MIN_MAX_RESULT_CHARS
        );
        assert!(toml::from_str::<Config>("[tool_limits]\nunknown = 1\n").is_err());
        let width: Config = toml::from_str("[tool_limits]\nmax_read_line_chars = 500\n").unwrap();
        assert_eq!(ToolLimits::from_config(&width).max_read_line_chars, 500);
        let notebook: Config =
            toml::from_str("[tool_limits]\nnotebook_max_bytes = 999999999999\n").unwrap();
        assert_eq!(
            ToolLimits::from_config(&notebook).notebook_max_bytes,
            super::super::notebook::MAX_NOTEBOOK_BYTES
        );
        let limits = ToolLimits::default();
        assert!(
            limits.backstop_head_chars() + limits.backstop_tail_chars() + 1_000
                < limits.max_result_chars
        );
    }
}
