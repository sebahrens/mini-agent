//! The top-level config key catalogue: every key any build of mini-agent
//! owns, the Cargo feature that owns it, and its accepted alternative
//! spellings.
//!
//! `Config` is `#[serde(default)]` and keeps keys it does not own in its
//! flattened `preserved` map, so a misspelled key used to vanish without a
//! word — including restricting keys such as `permission-deny`. This table
//! lets loading tell three cases apart:
//!
//! * a key owned by the running build, which never reaches `preserved`;
//! * a key owned only by a build with other Cargo features (for example
//!   `mcp_servers` in a `--no-default-features` build), which is preserved
//!   silently so builds do not warn about each other's keys;
//! * a key no build knows, which is reported with the nearest known key.
//!
//! The aliases listed here must match the `#[serde(alias)]` attributes on
//! `Config`; `config_key_catalogue_*` tests pin that correspondence. Adding a
//! top-level `Config` field means adding it here too.

/// One top-level config key.
pub(crate) struct ConfigKey {
    /// The spelling `Config` serializes.
    pub(crate) name: &'static str,
    /// The Cargo feature that must be enabled for the running build to own
    /// this key, or `None` when every build owns it.
    pub(crate) feature: Option<&'static str>,
    /// Alternative spellings accepted on input and rewritten to `name`.
    pub(crate) aliases: &'static [&'static str],
}

const fn key(name: &'static str) -> ConfigKey {
    ConfigKey {
        name,
        feature: None,
        aliases: &[],
    }
}

const fn aliased(name: &'static str, aliases: &'static [&'static str]) -> ConfigKey {
    ConfigKey {
        name,
        feature: None,
        aliases,
    }
}

const fn gated(feature: &'static str, name: &'static str) -> ConfigKey {
    ConfigKey {
        name,
        feature: Some(feature),
        aliases: &[],
    }
}

const fn gated_aliased(
    feature: &'static str,
    name: &'static str,
    aliases: &'static [&'static str],
) -> ConfigKey {
    ConfigKey {
        name,
        feature: Some(feature),
        aliases,
    }
}

/// Every top-level key of `Config` across all Cargo feature combinations.
pub(crate) const CONFIG_KEYS: &[ConfigKey] = &[
    key("model"),
    key("provider"),
    key("max_tokens"),
    key("turn_token_budget"),
    key("temperature"),
    key("extra_body"),
    key("reasoning"),
    key("retry"),
    key("no_tools"),
    key("no_context_files"),
    key("context_window"),
    key("reserve_tokens"),
    key("keep_recent_tokens"),
    key("keep_recent_tool_results"),
    key("max_agent_turns"),
    key("verify_command"),
    gated("goal", "goal"),
    gated("goal", "goal_checks"),
    gated("goal", "goal_judge_model"),
    key("session_title_model"),
    key("verify_timeout_secs"),
    key("verify_max_attempts"),
    key("max_text_file_size"),
    key("max_read_lines"),
    key("max_bash_output_lines"),
    key("max_grep_results"),
    key("max_find_results"),
    key("max_list_dir_entries"),
    gated("subagents", "subagent_max_read_lines"),
    gated("subagents", "subagent_max_grep_results"),
    gated("subagents", "subagent_max_find_results"),
    gated("subagents", "subagent_max_list_dir_entries"),
    key("compact_enabled"),
    key("mid_turn_compact_threshold"),
    key("always_show_welcome"),
    aliased("auto-update-prompts", &["auto_update_prompts"]),
    aliased("auto-update-themes", &["auto_update_themes"]),
    key("custom_providers"),
    key("embedding"),
    key("enable_skill_proposals"),
    key("permission"),
    aliased("permission-regex", &["permission_regex"]),
    aliased("permission-allow", &["permission_allow"]),
    aliased("permission-ask", &["permission_ask"]),
    aliased("permission-deny", &["permission_deny"]),
    key("restrictive"),
    key("accept_all"),
    key("yolo"),
    key("sandbox"),
    aliased("sandbox-backend", &["sandbox_backend"]),
    aliased(
        "windows-appcontainer-read-roots",
        &["windows_appcontainer_read_roots"],
    ),
    aliased(
        "windows-appcontainer-write-roots",
        &["windows_appcontainer_write_roots"],
    ),
    gated_aliased("js", "js-file-base-dir", &["js_file_base_dir"]),
    gated_aliased("js", "js-read-roots", &["js_read_roots"]),
    gated_aliased("js", "js-write-roots", &["js_write_roots"]),
    gated_aliased("js", "js-read-unrestricted", &["js_read_unrestricted"]),
    gated_aliased("js", "js-write-unrestricted", &["js_write_unrestricted"]),
    gated_aliased("js", "js-fetch-origins", &["js_fetch_origins"]),
    gated_aliased("js", "js-fetch-allow-http", &["js_fetch_allow_http"]),
    aliased("allow_all_mcp_calls", &["allow-all-mcp-calls"]),
    gated("mcp", "mcp_tool_timeout_secs"),
    gated_aliased("mcp", "enable-exa-mcp", &["enable_exa_mcp"]),
    gated_aliased("mcp", "enable-context7-mcp", &["enable_context7_mcp"]),
    gated_aliased("mcp", "enable-grepapp-mcp", &["enable_grepapp_mcp"]),
    aliased("default_permission_mode", &["default-permission-mode"]),
    aliased("permission-modes", &["permission_modes"]),
    key("show_tool_details"),
    key("show_reasoning"),
    key("statusline"),
    key("chat_left_margin"),
    key("mouse_capture"),
    key("terminal_title"),
    key("terminal_notify"),
    key("terminal_prompt_marks"),
    key("default_prompt"),
    gated_aliased("git-worktree", "wt-auto-merge", &["wt_auto_merge"]),
    gated_aliased("git-worktree", "wt-base-dir", &["wt_base_dir"]),
    key("shell"),
    key("editor"),
    key("api_keys"),
    key("quick_models"),
    key("prompt_to_model"),
    gated("mcp", "mcp_servers"),
    gated("acp", "acp_servers"),
    gated("acp", "acp_host"),
    gated("acp", "acp_port"),
    key("edit_system"),
    gated("subagents", "task_max_turns"),
    gated("subagents", "task_max_prompts"),
    gated("subagents", "task_max_concurrency"),
    gated("subagents", "task_max_output_bytes"),
    gated("subagents", "task_max_cost_units"),
    gated("subagents", "task_timeout_secs"),
    key("deny_repeated_reads"),
    key("show_cost_always"),
    gated("subagents", "task_enabled"),
    gated("subagents", "subagent_model"),
    gated("subagents", "subagent_provider"),
    key("colors"),
    key("chain"),
    gated("lsp", "lsp"),
    gated("advisor", "advisor"),
];

/// Whether the running build compiled in `feature`.
pub(crate) fn feature_enabled(feature: &str) -> bool {
    match feature {
        "goal" => cfg!(feature = "goal"),
        "subagents" => cfg!(feature = "subagents"),
        "js" => cfg!(feature = "js"),
        "mcp" => cfg!(feature = "mcp"),
        "acp" => cfg!(feature = "acp"),
        "git-worktree" => cfg!(feature = "git-worktree"),
        "lsp" => cfg!(feature = "lsp"),
        "advisor" => cfg!(feature = "advisor"),
        _ => false,
    }
}

/// The catalogue entry whose name or alias is exactly `spelling`.
pub(crate) fn lookup(spelling: &str) -> Option<&'static ConfigKey> {
    CONFIG_KEYS
        .iter()
        .find(|key| key.name == spelling || key.aliases.contains(&spelling))
}

/// Rewrite every aliased top-level key of `table` to its canonical spelling.
///
/// Configs are merged and re-serialized as `toml::Value`s, where an alias and
/// its canonical key would otherwise be two distinct entries (and a duplicate
/// field once deserialized). A table that sets both spellings of one key is
/// rejected: neither value may silently win.
pub(crate) fn canonicalize_top_level_keys(
    table: &mut toml::map::Map<String, toml::Value>,
) -> Result<(), String> {
    for key in CONFIG_KEYS {
        for alias in key.aliases {
            let Some(value) = table.remove(*alias) else {
                continue;
            };
            if table.contains_key(key.name) {
                return Err(format!(
                    "config sets both `{}` and `{alias}`; keep only one",
                    key.name
                ));
            }
            table.insert(key.name.to_string(), value);
        }
    }
    Ok(())
}

/// Warnings for top-level keys that no build of mini-agent owns.
///
/// `keys` are the keys the running build did not own (a parsed config's
/// preserved keys). Keys owned by a build with other Cargo features are
/// skipped. Each warning names the key, `source`, and the nearest known key
/// when one is close enough to be a plausible typo.
pub(crate) fn unknown_key_warnings<'a>(
    source: &str,
    keys: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    keys.into_iter()
        .filter(|spelling| match lookup(spelling) {
            None => true,
            Some(known) => {
                if let Some(feature) = known.feature.filter(|f| !feature_enabled(f)) {
                    tracing::debug!(
                        "{source}: config key `{spelling}` needs the `{feature}` feature, \
                         which this build lacks; keeping it unused"
                    );
                }
                false
            }
        })
        .map(|spelling| match nearest_known_key(spelling) {
            Some(suggestion) => format!(
                "{source}: unknown config key `{spelling}` is ignored (did you mean `{suggestion}`?)"
            ),
            None => format!("{source}: unknown config key `{spelling}` is ignored"),
        })
        .collect()
}

/// The canonical key nearest to `spelling`, compared case-insensitively with
/// `-` and `_` treated alike, or `None` when nothing is plausibly close.
pub(crate) fn nearest_known_key(spelling: &str) -> Option<&'static str> {
    let wanted = normalize(spelling);
    let wanted = wanted.as_str();
    let limit = (wanted.chars().count() / 4).max(2);
    CONFIG_KEYS
        .iter()
        .flat_map(|key| {
            std::iter::once(key.name)
                .chain(key.aliases.iter().copied())
                .map(move |candidate| (key.name, edit_distance(wanted, &normalize(candidate))))
        })
        .filter(|(_, distance)| *distance <= limit)
        .min_by_key(|(_, distance)| *distance)
        .map(|(name, _)| name)
}

fn normalize(spelling: &str) -> String {
    spelling.to_ascii_lowercase().replace('-', "_")
}

/// Levenshtein distance over chars.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != *cb);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// A TOML parse error reduced to its position and a message with quoted
/// values redacted: config files hold credentials, so no source excerpt or
/// echoed value may reach the terminal or logs.
pub(crate) fn describe_toml_error(content: &str, error: &toml::de::Error) -> String {
    let message = redact_quoted(error.message());
    match error.span() {
        Some(span) => {
            let (line, column) = line_column(content, span.start);
            format!("line {line}, column {column}: {message}")
        }
        None => message,
    }
}

/// One-based line and column (in chars) of byte `offset` within `content`.
fn line_column(content: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(content.len());
    while !content.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &content[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = before[line_start..].chars().count() + 1;
    (line, column)
}

/// Replace the contents of every `"..."` and `'...'` run in a parser message,
/// which is where serde and toml echo the offending value.
fn redact_quoted(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut open: Option<char> = None;
    for ch in message.chars() {
        match open {
            Some(quote) if ch == quote => {
                out.push('…');
                out.push(ch);
                open = None;
            }
            Some(_) => {}
            None if ch == '"' || ch == '\'' => {
                out.push(ch);
                open = Some(ch);
            }
            None => out.push(ch),
        }
    }
    if open.is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distance_counts_single_edits() {
        assert_eq!(edit_distance("sandbx", "sandbox"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn nearest_known_key_suggests_typos_and_separator_swaps() {
        assert_eq!(nearest_known_key("sandbx"), Some("sandbox"));
        assert_eq!(
            nearest_known_key("defualt_permission_mode"),
            Some("default_permission_mode")
        );
        assert_eq!(nearest_known_key("permision_deny"), Some("permission-deny"));
        assert_eq!(nearest_known_key("accept-all"), Some("accept_all"));
        assert_eq!(nearest_known_key("completely_unrelated_thing"), None);
    }

    #[test]
    fn redact_quoted_hides_echoed_values() {
        assert_eq!(
            redact_quoted("invalid type: string \"sk-secret\", expected u64"),
            "invalid type: string \"…\", expected u64"
        );
        assert_eq!(redact_quoted("unterminated 'abc"), "unterminated '…");
    }

    #[test]
    fn line_column_is_one_based_and_char_aware() {
        let content = "a = 1\nbé = x\n";
        assert_eq!(line_column(content, 0), (1, 1));
        let offset = content.find('x').unwrap();
        assert_eq!(line_column(content, offset), (2, 6));
    }

    #[test]
    fn canonicalize_rewrites_aliases_and_rejects_both_spellings() {
        let mut table: toml::map::Map<String, toml::Value> =
            toml::from_str("permission_deny = { bash = [\"rm **\"] }\nsandbox_backend = \"bwrap\"")
                .unwrap();
        canonicalize_top_level_keys(&mut table).unwrap();
        assert!(table.contains_key("permission-deny"));
        assert!(table.contains_key("sandbox-backend"));
        assert!(!table.contains_key("permission_deny"));

        let mut both: toml::map::Map<String, toml::Value> =
            toml::from_str("permission-modes = []\npermission_modes = [\"yolo\"]").unwrap();
        let error = canonicalize_top_level_keys(&mut both).unwrap_err();
        assert!(error.contains("permission-modes") && error.contains("permission_modes"));
    }

    #[test]
    fn config_key_catalogue_has_no_duplicate_spellings() {
        let mut seen = std::collections::BTreeSet::new();
        for key in CONFIG_KEYS {
            for spelling in std::iter::once(key.name).chain(key.aliases.iter().copied()) {
                assert!(seen.insert(spelling), "`{spelling}` is listed twice");
            }
        }
    }

    #[test]
    fn config_key_catalogue_gives_every_kebab_key_its_snake_spelling() {
        for key in CONFIG_KEYS.iter().filter(|key| key.name.contains('-')) {
            let snake = key.name.replace('-', "_");
            assert!(
                key.aliases.contains(&snake.as_str()),
                "`{}` must accept `{snake}`",
                key.name
            );
        }
    }

    /// Every catalogue spelling whose feature is compiled in is owned by
    /// `Config` (it never lands in `preserved`), and every spelling whose
    /// feature is compiled out is preserved. This pins the catalogue and the
    /// `#[serde(alias)]` attributes to each other in every feature row.
    #[test]
    fn config_key_catalogue_matches_config_ownership() {
        for key in CONFIG_KEYS {
            let owned = key.feature.is_none_or(feature_enabled);
            for spelling in std::iter::once(key.name).chain(key.aliases.iter().copied()) {
                // A value no field accepts: an owned key either rejects it or
                // consumes it, but never preserves it.
                let source = format!("{spelling:?} = {{ probe = [1, \"x\"] }}");
                let preserved = match toml::from_str::<crate::config::Config>(&source) {
                    Ok(cfg) => cfg.preserved.keys().any(|k| k == spelling),
                    Err(_) => false,
                };
                assert_eq!(
                    preserved, !owned,
                    "`{spelling}` ownership disagrees with the catalogue"
                );
            }
        }
    }
}
