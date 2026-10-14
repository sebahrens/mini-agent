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

/// Structural editor contract. Dynamic object contents retain their runtime
/// validators; this schema never grants permission or certifies a command.
#[derive(Clone, Copy)]
pub(crate) enum ConfigShape {
    Any,
    Bool,
    Integer,
    Number,
    String,
    Strings,
    Object,
    StringMap,
    StringArrayMap,
    Enum(&'static [&'static str]),
    BoolOrInteger,
}

/// One top-level config key.
pub(crate) struct ConfigKey {
    /// The spelling `Config` serializes.
    pub(crate) name: &'static str,
    /// The Cargo feature that must be enabled for the running build to own
    /// this key, or `None` when every build owns it.
    pub(crate) feature: Option<&'static str>,
    /// Alternative spellings accepted on input and rewritten to `name`.
    pub(crate) aliases: &'static [&'static str],
    pub(crate) shape: ConfigShape,
    pub(crate) description: &'static str,
    pub(crate) restart_required: bool,
}

const fn key(name: &'static str) -> ConfigKey {
    ConfigKey {
        name,
        feature: None,
        aliases: &[],
        shape: ConfigShape::Any,
        description: "Saved configuration for this setting; applies after restart",
        restart_required: true,
    }
}

const fn aliased(name: &'static str, aliases: &'static [&'static str]) -> ConfigKey {
    ConfigKey {
        name,
        feature: None,
        aliases,
        shape: ConfigShape::Any,
        description: "Saved configuration for this setting; applies after restart",
        restart_required: true,
    }
}

const fn gated(feature: &'static str, name: &'static str) -> ConfigKey {
    ConfigKey {
        name,
        feature: Some(feature),
        aliases: &[],
        shape: ConfigShape::Any,
        description: "Saved configuration for this setting; applies after restart",
        restart_required: true,
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
        shape: ConfigShape::Any,
        description: "Saved configuration for this setting; applies after restart",
        restart_required: true,
    }
}

impl ConfigKey {
    const fn described(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }
    const fn live(mut self) -> Self {
        self.restart_required = false;
        self
    }
    const fn shaped(mut self, shape: ConfigShape) -> Self {
        self.shape = shape;
        self
    }
}

/// Every top-level key of `Config` across all Cargo feature combinations.
pub(crate) const CONFIG_KEYS: &[ConfigKey] = &[
    key("model")
        .described("Default model identifier")
        .shaped(ConfigShape::String),
    key("provider")
        .described("Default provider or custom gateway")
        .shaped(ConfigShape::String),
    key("max_tokens")
        .described("Maximum response tokens")
        .shaped(ConfigShape::Integer),
    key("turn_token_budget").described("Cumulative fail-closed token budget for one agentic turn: the sum of input+output tokens across every completion call the turn makes.").shaped(ConfigShape::Integer),
    key("max_budget_usd")
        .described("Global spend ceiling in US dollars")
        .shaped(ConfigShape::Number),
    key("cache_ttl").described("Owner-selected native Anthropic cache TTL. Project layers cannot alter this billing policy. Omission preserves manual five-minute caching.").shaped(ConfigShape::Enum(&["5m", "1h"])),
    key("temperature")
        .described("Sampling temperature")
        .shaped(ConfigShape::Number),
    key("extra_body").described("Provider-specific JSON shallow-merged into every completion request body as a global default.").shaped(ConfigShape::Any),
    key("reasoning").described("First-class reasoning controls for the OpenAI family: `effort` and `summary` map to the Responses API's `reasoning` object (and `effort` alone to Chat Completions' `reasoning_effort`),…").shaped(ConfigShape::Object),
    key("retry").shaped(ConfigShape::Object),
    key("no_tools").shaped(ConfigShape::Bool),
    key("headless_questions").shaped(ConfigShape::Enum(&["error", "fail"])),
    key("no_context_files")
        .described("Disable automatic instruction-file discovery")
        .shaped(ConfigShape::Bool),
    key("context_window")
        .described("Context token capacity")
        .shaped(ConfigShape::Integer),
    key("reserve_tokens").shaped(ConfigShape::Integer),
    key("keep_recent_tokens").shaped(ConfigShape::Integer),
    key("keep_recent_tool_results").described("Number of newest historical tool results kept verbatim in the live model context.").shaped(ConfigShape::Integer),
    key("max_agent_turns").shaped(ConfigShape::Integer),
    key("verify_command").described("Trusted operator command run before a tool-using turn may complete. Project-local values require the existing sensitive-config approval.").shaped(ConfigShape::String),
    key("format")
        .described("Formatter command templates by extension")
        .shaped(ConfigShape::StringMap),
    key("keys")
        .described("Keyboard actions and chord lists")
        .shaped(ConfigShape::StringArrayMap),
    key("vim_mode")
        .described("Enable modal input editing")
        .shaped(ConfigShape::Bool),
    gated("goal", "goal").described("Goal defaults. Benign: these only bound how long the harness keeps working, never what it is allowed to run.").shaped(ConfigShape::Object),
    gated("goal", "goal_checks").described("Commands that must exit zero before a goal may be reported complete.").shaped(ConfigShape::Strings),
    gated("goal", "goal_judge_model").described("A `quick_models` entry name to judge goal completion claims.").shaped(ConfigShape::String),
    key("session_title_model").described("A `quick_models` entry name that titles sessions for `--resume` and `/sessions` after their first exchange.").shaped(ConfigShape::String),
    key("verify_timeout_secs").described("Wall-clock bound for one verification attempt. Default: 300 seconds.").shaped(ConfigShape::Integer),
    key("verify_max_attempts").described("Total verification attempts within one agent turn. Default: 3; capped at 8 so a persistently failing command cannot loop indefinitely.").shaped(ConfigShape::Integer),
    key("max_text_file_size").shaped(ConfigShape::Integer),
    key("max_read_lines").shaped(ConfigShape::Integer),
    key("max_bash_output_lines").shaped(ConfigShape::Integer),
    key("shell_timeout_secs").described("Foreground shell tool deadline. Other sandbox command classes retain their own limits; this does not alter brokered JavaScript spawn limits.").shaped(ConfigShape::Integer),
    key("update_check").described("Opt-in anonymous release check, only during interactive startup.").shaped(ConfigShape::Bool),
    key("max_grep_results").shaped(ConfigShape::Integer),
    key("max_find_results").shaped(ConfigShape::Integer),
    key("max_list_dir_entries").shaped(ConfigShape::Integer),
    gated("subagents", "subagent_max_read_lines").shaped(ConfigShape::Integer),
    gated("subagents", "subagent_max_grep_results").shaped(ConfigShape::Integer),
    gated("subagents", "subagent_max_find_results").shaped(ConfigShape::Integer),
    gated("subagents", "subagent_max_list_dir_entries").shaped(ConfigShape::Integer),
    key("compact_enabled").shaped(ConfigShape::Bool),
    key("mid_turn_compact_threshold").described("Opt-in mid-turn compaction threshold, as a fraction of the context window (0.0–1.0) of real provider prompt pressure.").shaped(ConfigShape::Number),
    key("always_show_welcome").shaped(ConfigShape::Bool),
    aliased("auto-update-prompts", &["auto_update_prompts"]).shaped(ConfigShape::Bool),
    aliased("auto-update-themes", &["auto_update_themes"]).shaped(ConfigShape::Bool),
    key("custom_providers").shaped(ConfigShape::Object),
    key("embedding").described("Embedding backend for the skill library. Absent means the built-in offline deterministic backend.").shaped(ConfigShape::Object),
    key("enable_skill_proposals").described("Allow model-authored JavaScript steps to submit bounded learned-skill proposals.").shaped(ConfigShape::Bool),
    key("permission")
        .described("Default tool permission policy")
        .shaped(ConfigShape::Any),
    aliased("permission-regex", &["permission_regex"]).shaped(ConfigShape::Any),
    aliased("permission-allow", &["permission_allow"]).shaped(ConfigShape::StringArrayMap),
    aliased("permission-allow-prefix", &["permission_allow_prefix"])
        .shaped(ConfigShape::StringArrayMap),
    aliased("permission-ask", &["permission_ask"]).shaped(ConfigShape::StringArrayMap),
    aliased("permission-deny", &["permission_deny"]).shaped(ConfigShape::StringArrayMap),
    key("restrictive").shaped(ConfigShape::Bool),
    key("accept_all").shaped(ConfigShape::Bool),
    key("yolo").shaped(ConfigShape::Bool),
    key("sandbox")
        .described("Enable process containment")
        .shaped(ConfigShape::Bool),
    aliased("sandbox-backend", &["sandbox_backend"]).shaped(ConfigShape::String),
    aliased(
        "windows-appcontainer-read-roots",
        &["windows_appcontainer_read_roots"],
    )
    .shaped(ConfigShape::Strings),
    aliased(
        "windows-appcontainer-write-roots",
        &["windows_appcontainer_write_roots"],
    )
    .shaped(ConfigShape::Strings),
    gated_aliased("js", "js-file-base-dir", &["js_file_base_dir"]).shaped(ConfigShape::String),
    gated_aliased("js", "js-read-roots", &["js_read_roots"]).shaped(ConfigShape::Strings),
    gated_aliased("js", "js-write-roots", &["js_write_roots"]).shaped(ConfigShape::Strings),
    gated_aliased("js", "js-read-unrestricted", &["js_read_unrestricted"])
        .shaped(ConfigShape::Bool),
    gated_aliased("js", "js-write-unrestricted", &["js_write_unrestricted"])
        .shaped(ConfigShape::Bool),
    gated_aliased("js", "js-fetch-origins", &["js_fetch_origins"]).shaped(ConfigShape::Strings),
    gated_aliased("js", "js-fetch-allow-http", &["js_fetch_allow_http"]).shaped(ConfigShape::Bool),
    aliased("allow_all_mcp_calls", &["allow-all-mcp-calls"]).shaped(ConfigShape::Bool),
    gated("mcp", "mcp_tool_timeout_secs").described("Bound on one MCP `tools/call` round trip, in seconds. Default: 120.").shaped(ConfigShape::Integer),
    gated_aliased("mcp", "enable-exa-mcp", &["enable_exa_mcp"]).shaped(ConfigShape::Bool),
    gated_aliased("mcp", "enable-context7-mcp", &["enable_context7_mcp"]).shaped(ConfigShape::Bool),
    gated_aliased("mcp", "enable-grepapp-mcp", &["enable_grepapp_mcp"]).shaped(ConfigShape::Bool),
    aliased("default_permission_mode", &["default-permission-mode"]).shaped(ConfigShape::String),
    aliased("permission-modes", &["permission_modes"]).shaped(ConfigShape::Strings),
    key("permission_mode_cycle").described("Explicit-user Shift+Tab cycle, independent of the rule-application modes.").shaped(ConfigShape::Strings),
    key("show_tool_details").shaped(ConfigShape::BoolOrInteger),
    key("diff_context_lines").described("Context rows in successful edit/write previews (hard maximum 20).").shaped(ConfigShape::Integer),
    key("max_diff_lines").described("Preview row cap (hard maximum 1000); zero disables mutation previews.").shaped(ConfigShape::Integer),
    key("show_reasoning").shaped(ConfigShape::Bool),
    key("reasoning_display")
        .live()
        .described("Presentation of reasoning; does not enable model thinking")
        .shaped(ConfigShape::Enum(&["off", "collapsed", "full"])),
    key("statusline").described("Configurable status-bar (up to 3 lines). When absent, a built-in default layout is used.").shaped(ConfigShape::Object),
    key("chat_left_margin").described("Left padding (columns) for the chat area. Default: 0.").shaped(ConfigShape::Integer),
    key("mouse_capture").described("Capture mouse events in the TUI (wheel scrolling, click-to-place-cursor, link opening, drag-to-copy). `false` leaves the mouse to the terminal so its native selection works. Default: true.").shaped(ConfigShape::Bool),
    key("terminal_title").described("Report agent activity in the terminal/window title (`mini-agent: working`, `waiting for approval`, `idle`) so terminal multiplexers and tab bars can show it; the previous title is…").shaped(ConfigShape::Bool),
    key("terminal_notify").described("Desktop notification escape (`osc9`, `osc777`) emitted when a turn finishes or a permission prompt waits. Default: off.").shaped(ConfigShape::Enum(&["off", "osc9", "osc777"])),
    key("terminal_prompt_marks").described("Emit OSC 133 semantic prompt marks: a prompt while the TUI waits for input, command start while a run is active, command end when it finishes. Default: false.").shaped(ConfigShape::Bool),
    key("default_prompt").shaped(ConfigShape::String),
    gated_aliased("git-worktree", "wt-auto-merge", &["wt_auto_merge"]).shaped(ConfigShape::Bool),
    gated_aliased("git-worktree", "wt-base-dir", &["wt_base_dir"]).shaped(ConfigShape::String),
    key("shell").shaped(ConfigShape::String),
    key("editor").shaped(ConfigShape::String),
    key("api_keys")
        .described("Provider credentials (values hidden)")
        .shaped(ConfigShape::StringMap),
    key("quick_models").shaped(ConfigShape::Object),
    key("prompt_to_model").described("Map prompt names to quick-model names. When switching to a prompt, zerostack looks up the quick model and switches provider+model. Empty-string values are treated as \"no change\".").shaped(ConfigShape::StringMap),
    gated("mcp", "mcp_servers").shaped(ConfigShape::Object),
    gated("acp", "acp_servers").shaped(ConfigShape::Object),
    gated("acp", "acp_host").shaped(ConfigShape::String),
    gated("acp", "acp_port").shaped(ConfigShape::Integer),
    key("edit_system").shaped(ConfigShape::Enum(&["similarity", "hashedit"])),
    gated("subagents", "task_max_turns").shaped(ConfigShape::Integer),
    gated("subagents", "task_max_prompts").shaped(ConfigShape::Integer),
    gated("subagents", "task_max_concurrency").shaped(ConfigShape::Integer),
    gated("subagents", "task_max_output_bytes").shaped(ConfigShape::Integer),
    gated("subagents", "task_max_cost_units").shaped(ConfigShape::Integer),
    gated("subagents", "task_timeout_secs").shaped(ConfigShape::Integer),
    key("deny_repeated_reads").shaped(ConfigShape::Bool),
    key("show_cost_always").described("Show the session cost in the status bar even when it is $0.0000 (e.g. when the model has no per-token pricing configured). Default: false.").shaped(ConfigShape::Bool),
    gated("subagents", "task_enabled").shaped(ConfigShape::Bool),
    gated("subagents", "subagents").shaped(ConfigShape::Object),
    gated("subagents", "subagent_model").shaped(ConfigShape::String),
    gated("subagents", "subagent_provider").shaped(ConfigShape::String),
    key("colors").shaped(ConfigShape::Object),
    key("chain").shaped(ConfigShape::Object),
    gated("lsp", "lsp").shaped(ConfigShape::Object),
    gated("advisor", "advisor").shaped(ConfigShape::Object),
];

/// Whether the running build compiled in `feature`.
pub(crate) fn feature_enabled(feature: &str) -> bool {
    // A table rather than a `match`: with every feature on (`--all-features`)
    // each arm is `true` and Clippy's `match_like_matches_macro` fires, while
    // any other build needs the per-feature values.
    const COMPILED: [(&str, bool); 8] = [
        ("goal", cfg!(feature = "goal")),
        ("subagents", cfg!(feature = "subagents")),
        ("js", cfg!(feature = "js")),
        ("mcp", cfg!(feature = "mcp")),
        ("acp", cfg!(feature = "acp")),
        ("git-worktree", cfg!(feature = "git-worktree")),
        ("lsp", cfg!(feature = "lsp")),
        ("advisor", cfg!(feature = "advisor")),
    ];
    COMPILED
        .iter()
        .any(|(name, enabled)| *enabled && *name == feature)
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
pub(super) fn line_column(content: &str, offset: usize) -> (usize, usize) {
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
    fn feature_enabled_reports_exactly_this_builds_features() {
        assert_eq!(feature_enabled("goal"), cfg!(feature = "goal"));
        assert_eq!(feature_enabled("js"), cfg!(feature = "js"));
        assert_eq!(feature_enabled("lsp"), cfg!(feature = "lsp"));
        assert_eq!(
            feature_enabled("git-worktree"),
            cfg!(feature = "git-worktree")
        );
        assert!(!feature_enabled("no-such-feature"));
        assert!(!feature_enabled(""));
    }

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
