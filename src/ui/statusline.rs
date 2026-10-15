//! Configurable status-bar statusline.
//!
//! The statusline is up to 3 lines, each an ordered list of segments parsed from
//! `[statusline]` in config (see `docs/agent/CONFIG.md`). When no `[statusline]` is set, a
//! built-in default layout is used. Items resolve to text + colors at render
//! time; `separator` is literal text and `flex_separator` expands to fill the
//! row, pushing later segments to the right.

use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use crossterm::style::Color;

use crate::config::Config;
use crate::config::types::{IconSpec, StatusLineConfig, StatusLineLine, StatusLineSegment};
use crate::session::{GitStatus, Session};
use crate::ui::utils::parse_color;

pub const MAX_STATUS_LINES: usize = 3;

/// A drawable statusline piece after items are resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusSpan {
    Text {
        text: String,
        fg: Option<Color>,
        bg: Option<Color>,
        layout: SpanLayout,
    },
    /// Expands to fill remaining width (splits evenly when several are present).
    Flex,
}

/// Layout metadata stays with the exact resolved item; values are never
/// classified by parsing their display text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpanLayout {
    Separator,
    Decoration,
    Item {
        priority: u8,
        plain: String,
        compact: Option<String>,
        dynamic_color: bool,
    },
}

/// Runtime values the statusline can show beyond the session itself.
pub struct StatusContext<'a> {
    pub workspace: &'a std::path::Path,
    pub loop_label: Option<&'a str>,
    /// Active goal, rendered as `round/max` plus a status word when the goal is
    /// not simply running.
    pub goal_label: Option<&'a str>,
    pub prompt_name: Option<&'a str>,
    /// Configured `[reasoning] effort` (wire spelling), if any.
    pub reasoning_effort: Option<&'a str>,
    pub perm_mode: Option<&'a str>,
    pub chain_label: Option<&'a str>,
    pub background_jobs: usize,
    pub todos: Option<(usize, usize)>,
    pub queued_inputs: usize,
    pub btw_cost: f64,
    pub btw_in: u64,
    pub btw_out: u64,
    /// `Some("sandbox:off")` while shell commands run unsandboxed because the
    /// implicitly selected default backend was unavailable. This security
    /// indicator is shown even when a custom `[statusline]` omits `sandbox`.
    pub sandbox_label: Option<&'a str>,
}

/// Status segment text for a degraded (unavailable-default) sandbox.
pub const SANDBOX_OFF_LABEL: &str = "sandbox:off";

/// The persistent sandbox indicator for `sandbox`, or `None` when the session
/// is contained or the operator disabled sandboxing deliberately.
pub(crate) fn sandbox_label(sandbox: &crate::sandbox::Sandbox) -> Option<&'static str> {
    if sandbox.policy() != crate::sandbox::SandboxPolicy::Disabled {
        match sandbox.profile() {
            crate::sandbox::SandboxProfile::HostFull => return Some("sandbox:host-full"),
            crate::sandbox::SandboxProfile::HostRead => return Some("sandbox:host-read"),
            crate::sandbox::SandboxProfile::Workspace => {}
        }
    }
    matches!(
        sandbox.explicit_shell_boundary(),
        crate::sandbox::ExplicitShellBoundary::UnavailableDefaultFallback { .. }
    )
    .then_some(SANDBOX_OFF_LABEL)
}

static SPEC: OnceLock<StatusLineConfig> = OnceLock::new();
static NEEDS_GIT_STATUS: OnceLock<bool> = OnceLock::new();
static TIME_ITEMS: OnceLock<TimeItems> = OnceLock::new();

/// Which time-dependent items the spec shows; computed once with the spec so
/// [`cache_key`] does not scan every segment on every frame.
#[derive(Clone, Copy, Debug, Default)]
struct TimeItems {
    clock: bool,
    session_age: bool,
    session_updated: bool,
}

impl TimeItems {
    fn of(spec: &StatusLineConfig) -> Self {
        let mut uses = Self::default();
        for segment in spec.lines.iter().flat_map(|line| &line.segments) {
            match segment.item.as_str() {
                "clock" => uses.clock = true,
                "session_age" => uses.session_age = true,
                "session_updated" => uses.session_updated = true,
                _ => {}
            }
        }
        uses
    }
}

fn time_items() -> TimeItems {
    *TIME_ITEMS.get_or_init(|| TimeItems::of(spec()))
}

/// Parse the statusline spec from config once at startup. Clamps to 3 lines.
pub fn init(cfg: &Config) {
    let mut spec = cfg.statusline.clone().unwrap_or_else(default_spec);
    if spec.lines.is_empty() {
        spec = default_spec();
    }
    spec.lines.truncate(MAX_STATUS_LINES);
    let needs_git = spec.needs_git_status();
    let _ = TIME_ITEMS.set(TimeItems::of(&spec));
    let _ = SPEC.set(spec);
    let _ = NEEDS_GIT_STATUS.set(needs_git);
}

fn spec() -> &'static StatusLineConfig {
    SPEC.get_or_init(default_spec)
}

/// Number of statusline lines (1-3).
pub fn line_count() -> usize {
    spec().lines.len().clamp(1, MAX_STATUS_LINES)
}

/// Whether the configured statusline needs `git status` (a subprocess). False lets
/// the caller skip computing it.
pub fn needs_git_status() -> bool {
    *NEEDS_GIT_STATUS.get_or_init(|| false)
}

/// Build the statusline's drawable lines for the current state.
pub fn build(session: &Session, ctx: &StatusContext) -> Vec<Vec<StatusSpan>> {
    build_lines(spec(), session, ctx)
}

/// Stable fingerprint of every live value that can affect [`build`]. The UI
/// uses this to retain built spans across refreshes where only the spinner or
/// chat viewport changed.
pub fn cache_key(session: &Session, ctx: &StatusContext) -> u64 {
    let mut state = std::collections::hash_map::DefaultHasher::new();
    session.id.hash(&mut state);
    // One lock for both spend values the statusline shows.
    session.spend_account.status_snapshot().hash(&mut state);
    session
        .known_input_output_prices()
        .is_some()
        .hash(&mut state);
    session.name.hash(&mut state);
    session.messages.len().hash(&mut state);
    session.compactions.len().hash(&mut state);
    session.created_at.hash(&mut state);
    session.updated_at.hash(&mut state);
    session.total_input_tokens.hash(&mut state);
    session.total_output_tokens.hash(&mut state);
    session.total_cached_input_tokens.hash(&mut state);
    session.total_cache_creation_input_tokens.hash(&mut state);
    session.total_real_input_tokens.hash(&mut state);
    session.recorded_total_cost().to_bits().hash(&mut state);
    // The inputs of `effective_context_tokens` rather than the sum itself,
    // which walks every message since the last calibration. Estimate changes
    // move `total_estimated_tokens`; the newest message covers in-place edits.
    session.calibrated_tokens.hash(&mut state);
    session.calibrated_msg_count.hash(&mut state);
    session.overhead_tokens.hash(&mut state);
    session.total_estimated_tokens.hash(&mut state);
    session
        .messages
        .last()
        .map(|message| message.estimated_tokens)
        .hash(&mut state);
    session.ctx_is_estimated().hash(&mut state);
    session.context_window.hash(&mut state);
    session.model.hash(&mut state);
    session.provider.hash(&mut state);
    session.show_cost_always.hash(&mut state);
    session.git_branch.hash(&mut state);
    session.git_status.is_some().hash(&mut state);
    if let Some(git) = &session.git_status {
        git.staged.hash(&mut state);
        git.modified.hash(&mut state);
        git.deleted.hash(&mut state);
        git.untracked.hash(&mut state);
        git.ahead.hash(&mut state);
        git.behind.hash(&mut state);
    }
    session.reasoning_enabled.hash(&mut state);
    ctx.workspace.hash(&mut state);
    ctx.loop_label.hash(&mut state);
    ctx.goal_label.hash(&mut state);
    ctx.prompt_name.hash(&mut state);
    ctx.reasoning_effort.hash(&mut state);
    ctx.perm_mode.hash(&mut state);
    ctx.chain_label.hash(&mut state);
    ctx.background_jobs.hash(&mut state);
    #[cfg(feature = "subagents")]
    session.background_task_counts().hash(&mut state);
    ctx.todos.hash(&mut state);
    ctx.queued_inputs.hash(&mut state);
    ctx.btw_cost.to_bits().hash(&mut state);
    ctx.btw_in.hash(&mut state);
    ctx.btw_out.hash(&mut state);
    ctx.sandbox_label.hash(&mut state);

    let uses = time_items();
    if uses.clock {
        // The clock shows minutes; offsets are whole minutes, so the UTC
        // minute changes exactly when the local display can.
        chrono::Utc::now()
            .timestamp()
            .div_euclid(60)
            .hash(&mut state);
    }
    if uses.session_age {
        elapsed_bucket(&session.created_at).hash(&mut state);
    }
    if uses.session_updated {
        elapsed_bucket(&session.updated_at).hash(&mut state);
    }
    state.finish()
}

/// Build drawable lines from an explicit spec (used by `build` and tests).
pub fn build_lines(
    spec: &StatusLineConfig,
    session: &Session,
    ctx: &StatusContext,
) -> Vec<Vec<StatusSpan>> {
    let mut lines: Vec<Vec<StatusSpan>> = spec
        .lines
        .iter()
        .map(|line| build_line(line, session, ctx))
        .collect();
    // A degraded sandbox is a security state, not a cosmetic item: when a
    // custom layout does not place `sandbox` itself, append it to the first
    // line so the indicator can never be configured away by omission.
    let places_sandbox = spec
        .lines
        .iter()
        .flat_map(|line| &line.segments)
        .any(|segment| segment.item == "sandbox");
    if !places_sandbox
        && let Some(label) = ctx.sandbox_label
        && let Some(first) = lines.first_mut()
    {
        if !first.is_empty() {
            first.push(StatusSpan::Text {
                text: " ".to_string(),
                fg: None,
                bg: None,
                layout: SpanLayout::Separator,
            });
        }
        first.push(StatusSpan::Text {
            text: single_line_safe(label),
            fg: Some(Color::Red),
            bg: None,
            layout: SpanLayout::Item {
                priority: 255,
                plain: single_line_safe(label),
                compact: None,
                dynamic_color: true,
            },
        });
    }
    lines
}

/// Item values can come from outside the process (a session name set by
/// `/import`, a git branch name): strip control sequences and keep them on
/// one row so they can neither drive the terminal nor break the layout.
fn single_line_safe(text: &str) -> String {
    // Most values (and every built-in glyph) need no change: skip the
    // sanitizer's two intermediate allocations for them. Sanitizing keeps
    // exactly these characters, and a newline is a control character.
    if text
        .chars()
        .all(|c| !c.is_control() && !crate::ui::events::is_deceptive_format_char(c))
    {
        return text.to_owned();
    }
    crate::ui::events::sanitize_output(text).replace('\n', " ")
}

fn color(c: &Option<compact_str::CompactString>) -> Option<Color> {
    c.as_ref().and_then(|s| parse_color(s))
}

fn build_line(line: &StatusLineLine, session: &Session, ctx: &StatusContext) -> Vec<StatusSpan> {
    // (span, is_separator) so we can trim separators around skipped items.
    let mut raw: Vec<(StatusSpan, bool)> = Vec::new();
    for seg in &line.segments {
        match seg.item.as_str() {
            "flex_separator" | "flex" => raw.push((StatusSpan::Flex, false)),
            "separator" | "sep" => {
                raw.push((
                    StatusSpan::Text {
                        text: single_line_safe(seg.text.as_deref().unwrap_or(" ")),
                        fg: color(&seg.color),
                        bg: color(&seg.bg),
                        layout: SpanLayout::Separator,
                    },
                    true,
                ));
            }
            item => {
                let always = seg.always.unwrap_or(false);
                if let Some(mut text) =
                    resolve_item(item, session, ctx, always).map(|text| single_line_safe(&text))
                {
                    let plain = text.clone();
                    let live_color = dynamic_item_color(item, session);
                    let layout = SpanLayout::Item {
                        dynamic_color: live_color.is_some(),
                        priority: item_priority(item, seg.priority),
                        compact: if matches!(item, "mode" | "sandbox" | "goal" | "loop") {
                            None
                        } else {
                            seg.min
                                .as_ref()
                                .map(|value| single_line_safe(value))
                                .or_else(|| compact_item(item, session, ctx))
                        },
                        plain,
                    };
                    if let Some(glyph) = resolve_icon(seg.icon.as_ref(), item) {
                        text = format!("{} {text}", single_line_safe(glyph));
                    }
                    // A live-state override (e.g. the context meter turning
                    // amber/red as it fills) wins over the configured color so
                    // a reading at or past 100% reads as a deliberate warning
                    // rather than a rendering glitch. The powerline cap below
                    // derives from this fg, so the segment edge tracks it too.
                    let fg = live_color.or_else(|| color(&seg.color));
                    let bg = color(&seg.bg);
                    // Powerline caps: the glyph is drawn in the segment's bg color
                    // (so it reads as the segment's rounded/triangle edge) over the
                    // status-bar background. Falls back to the fg when no bg is set.
                    let cap = bg.or(fg);
                    if let Some(l) = &seg.left {
                        raw.push((
                            StatusSpan::Text {
                                text: single_line_safe(powerline_str(l)),
                                fg: cap,
                                bg: None,
                                layout: SpanLayout::Decoration,
                            },
                            false,
                        ));
                    }
                    raw.push((
                        StatusSpan::Text {
                            text,
                            fg,
                            bg,
                            layout,
                        },
                        false,
                    ));
                    if let Some(r) = &seg.right {
                        raw.push((
                            StatusSpan::Text {
                                text: single_line_safe(powerline_str(r)),
                                fg: cap,
                                bg: None,
                                layout: SpanLayout::Decoration,
                            },
                            false,
                        ));
                    }
                }
            }
        }
    }

    // Drop leading/duplicate separators (a separator whose previous kept piece
    // is also a separator), then any trailing separators. This keeps layouts
    // clean when optional items (cost, mode, git) resolve to nothing.
    let mut cleaned: Vec<(StatusSpan, bool)> = Vec::with_capacity(raw.len());
    for (span, is_sep) in raw {
        if is_sep {
            let prev_is_sep = cleaned.last().is_none_or(|(_, s)| *s);
            if prev_is_sep {
                continue;
            }
        }
        cleaned.push((span, is_sep));
    }
    while matches!(cleaned.last(), Some((_, true))) {
        cleaned.pop();
    }
    cleaned.into_iter().map(|(s, _)| s).collect()
}

fn item_priority(item: &str, configured: Option<u8>) -> u8 {
    match item {
        "sandbox" => 255,
        "mode" => 254,
        "goal" | "loop" => 253,
        "prompt" => 240,
        _ => configured
            .unwrap_or(match item {
                "tokens_input" | "tokens_output" | "calls" | "cache_hit_ratio" | "btw" => 20,
                "cost" => 30,
                "git_branch" | "git_status" | "git_changes" | "cwd" | "cwd_full" => 40,
                "context" | "context_used" | "context_max" | "context_percentage" => 80,
                _ => 100,
            })
            .min(239),
    }
}

fn compact_item(item: &str, session: &Session, ctx: &StatusContext) -> Option<String> {
    match item {
        "context" => Some(format!("ctx {}%", context_percentage(session).unwrap_or(0))),
        "cwd_full" => Some(basename(&ctx.workspace.to_string_lossy())),
        "cwd" => Some("…".into()),
        _ => None,
    }
}

fn fixed_width(spans: &[StatusSpan]) -> usize {
    spans
        .iter()
        .map(|span| match span {
            StatusSpan::Text { text, .. } => crate::ui::utils::display_width(text),
            StatusSpan::Flex => 0,
        })
        .sum()
}

fn clean_layout(spans: &mut Vec<StatusSpan>) {
    let mut seen_item = false;
    let mut separator = false;
    spans.retain(|span| match span {
        StatusSpan::Text {
            layout: SpanLayout::Separator,
            ..
        } => {
            let keep = seen_item && !separator;
            separator = true;
            keep
        }
        StatusSpan::Flex => {
            separator = true;
            true
        }
        _ => {
            seen_item = true;
            separator = false;
            true
        }
    });
    while matches!(
        spans.last(),
        Some(StatusSpan::Text {
            layout: SpanLayout::Separator,
            ..
        }) | Some(StatusSpan::Flex)
    ) {
        spans.pop();
    }
}

/// Cell-bounded layout used by the actual renderer at each terminal width.
/// Wide layouts remain byte-for-byte identical. Narrow layouts shed decoration,
/// then use compact forms, then drop complete items in ascending priority.
pub(crate) fn fit_line(spans: &[StatusSpan], width: usize) -> Vec<StatusSpan> {
    let mut result = spans.to_vec();
    if fixed_width(&result) <= width {
        return result;
    }
    result.retain(|span| {
        !matches!(
            span,
            StatusSpan::Text {
                layout: SpanLayout::Decoration,
                ..
            }
        )
    });
    for span in &mut result {
        if let StatusSpan::Text { text, layout, .. } = span {
            match layout {
                SpanLayout::Item { plain, .. } => *text = plain.clone(),
                SpanLayout::Separator => *text = " ".into(),
                SpanLayout::Decoration => unreachable!(),
            }
        }
    }
    clean_layout(&mut result);
    // Context first, then paths; configured compact forms use their item priority.
    let mut compact: Vec<_> = result
        .iter()
        .enumerate()
        .filter_map(|(index, span)| {
            if let StatusSpan::Text {
                layout:
                    SpanLayout::Item {
                        priority,
                        compact: Some(_),
                        ..
                    },
                ..
            } = span
            {
                Some((*priority, index))
            } else {
                None
            }
        })
        .collect();
    compact.sort_by_key(|&(priority, index)| (std::cmp::Reverse(priority), index));
    for (_, index) in compact {
        if fixed_width(&result) <= width {
            return result;
        }
        if let StatusSpan::Text {
            text,
            layout:
                SpanLayout::Item {
                    compact: Some(short),
                    ..
                },
            ..
        } = &mut result[index]
            && crate::ui::utils::display_width(short) < crate::ui::utils::display_width(text)
        {
            *text = short.clone();
        }
    }
    while fixed_width(&result) > width {
        let candidate = result
            .iter()
            .enumerate()
            .filter_map(|(index, span)| {
                if let StatusSpan::Text {
                    layout: SpanLayout::Item { priority, .. },
                    ..
                } = span
                {
                    Some((*priority, index))
                } else {
                    None
                }
            })
            .min();
        let item_count = result
            .iter()
            .filter(|span| {
                matches!(
                    span,
                    StatusSpan::Text {
                        layout: SpanLayout::Item { .. },
                        ..
                    }
                )
            })
            .count();
        if item_count <= 1 {
            break;
        }
        if let Some((_, index)) = candidate {
            result.remove(index);
            clean_layout(&mut result);
        } else {
            break;
        }
    }
    // A sole oversized item is visibly clipped, never silently cut mid-word.
    if fixed_width(&result) > width {
        for span in &mut result {
            if let StatusSpan::Text { text, .. } = span {
                *text = ellipsize(text, width);
            }
        }
    }
    result
}

pub(crate) fn ellipsize(text: &str, width: usize) -> String {
    if crate::ui::utils::display_width(text) <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", crate::ui::utils::display_prefix(text, width - 1))
}

/// Context fill as an integer percentage of the window, or `None` when the
/// window is unknown (so the meter shows `0%` rather than dividing by zero).
/// Deliberately not clamped: a value past 100% is a real "over budget" state
/// (estimated new messages stacked on the calibrated anchor, or a window
/// configured smaller than the provider's), and hiding it would mask that
/// compaction is overdue or could not recover.
fn context_percentage(session: &Session) -> Option<u64> {
    session
        .effective_context_tokens()
        .saturating_mul(100)
        .checked_div(session.context_window)
}

/// Per-item color override driven by live session state, taking precedence over
/// the configured color. Currently only the context percentage: amber as it
/// approaches full, red once it crosses the window, so an over-budget reading is
/// visibly intentional. Returns `None` to keep the segment's configured color.
fn dynamic_item_color(item: &str, session: &Session) -> Option<Color> {
    if item == "sandbox" {
        return Some(Color::Red);
    }
    if !matches!(item, "context_percentage" | "context") {
        return None;
    }
    match context_percentage(session)? {
        pct if pct > 100 => Some(Color::Red),
        pct if pct >= 90 => Some(Color::Yellow),
        _ => None,
    }
}

/// Resolve a non-separator item to display text, or `None` to skip it.
/// `always` forces zero-valued numeric items (tokens, cost) to render.
fn resolve_item(
    item: &str,
    session: &Session,
    ctx: &StatusContext,
    always: bool,
) -> Option<String> {
    match item {
        "session_name" => {
            let n = session.name.as_str();
            (!n.is_empty()).then(|| n.to_string())
        }
        "session_id" => Some(session.id.chars().take(8).collect()),
        "git_branch" => session.git_branch.as_ref().map(|b| b.to_string()),
        "git_changes" => session.git_status.as_ref().and_then(format_changes),
        "git_status" => session.git_status.as_ref().map(format_status),
        "cwd" => Some(basename(&ctx.workspace.to_string_lossy())),
        "cwd_full" => Some(contract_home(&ctx.workspace.to_string_lossy())),
        "worktree" => git_worktree(&ctx.workspace.to_string_lossy()),
        "model" => Some(session.model.to_string()),
        "model_short" => Some(
            session
                .model
                .rsplit('/')
                .next()
                .unwrap_or(&session.model)
                .to_string(),
        ),
        "provider" => Some(session.provider.to_string()),
        "message_count" => Some(session.messages.len().to_string()),
        "session_age" => fmt_elapsed(&session.created_at),
        "session_updated" => fmt_elapsed(&session.updated_at),
        "clock" => Some(chrono::Local::now().format("%H:%M").to_string()),
        "host" => hostname(),
        "user" => std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .ok()
            .filter(|u| !u.is_empty()),
        "reasoning" => session.reasoning_enabled.then(|| "reasoning".to_string()),
        "reasoning_effort" => ctx
            .reasoning_effort
            .map(|effort| format!("effort:{effort}")),
        "tokens_input" => (session.total_input_tokens > 0 || always)
            .then(|| format!("in:{}", fmt_tokens(session.total_input_tokens))),
        "tokens_output" => (session.total_output_tokens > 0 || always)
            .then(|| format!("out:{}", fmt_tokens(session.total_output_tokens))),
        "calls" => Some(match session.spend_account.model_call_count() {
            Some((count, true)) => format!("calls:{count}"),
            Some((0, false)) => "calls:?".into(),
            Some((count, false)) => format!("calls:≥{count}"),
            None if !session.has_model_call_history() => "calls:0".into(),
            None => "calls:?".into(),
        }),
        "context" => Some(format!(
            "ctx {}{}/{} {}%",
            if session.ctx_is_estimated() { "~" } else { "" },
            fmt_tokens(session.effective_context_tokens()),
            fmt_tokens(session.context_window),
            context_percentage(session).unwrap_or(0)
        )),
        "cache_hit_ratio" => {
            let total = session.total_real_input_tokens;
            (session.total_cached_input_tokens > 0
                || session.total_cache_creation_input_tokens > 0
                || always)
                .then(|| {
                    let percent = session
                        .total_cached_input_tokens
                        .saturating_mul(100)
                        .checked_div(total)
                        .unwrap_or(0)
                        .min(100);
                    format!("cache:{percent}%")
                })
        }
        "context_used" => {
            // A `~` marks the figure as an estimate until the provider reports
            // real usage (it then snaps to the exact number).
            let mark = if session.ctx_is_estimated() { "~" } else { "" };
            Some(format!(
                "{mark}{}",
                fmt_tokens(session.effective_context_tokens())
            ))
        }
        "context_max" => Some(fmt_tokens(session.context_window)),
        "context_percentage" => Some(format!("{}%", context_percentage(session).unwrap_or(0))),
        "cost" => {
            let amount = session.recorded_total_cost();
            let unknown = session.known_input_output_prices().is_none()
                || session.spend_account.snapshot().tracking_error;
            if unknown {
                Some(if amount > 0.0 {
                    format!("${amount:.4}+?")
                } else {
                    "cost:?".into()
                })
            } else {
                (amount > 0.0 || session.show_cost_always || always)
                    .then(|| format!("${amount:.4}"))
            }
        }
        "prompt" => ctx.prompt_name.map(|s| format!("prompt:{s}")),
        "mode" => ctx.perm_mode.map(|m| format!("mode:{m}")),
        "sandbox" => ctx.sandbox_label.map(str::to_string),
        "loop" => ctx.loop_label.map(|s| format!("[{s}]")),
        "goal" => ctx.goal_label.map(|s| format!("[{s}]")),
        "chain" => ctx.chain_label.map(|s| s.to_string()),
        "todos" => match ctx.todos {
            Some((done, total)) => (total > 0 || always).then(|| format!("todo:{done}/{total}")),
            None => Some("todo:?".into()),
        },
        "queue" => {
            (ctx.queued_inputs > 0 || always).then(|| format!("queue:{}", ctx.queued_inputs))
        }
        #[cfg(feature = "subagents")]
        "background_tasks" => {
            let (active, ready) = session.background_task_counts();
            (active > 0 || ready > 0 || always).then(|| format!("tasks:{active} ready:{ready}"))
        }
        "background_jobs" => {
            (ctx.background_jobs > 0 || always).then(|| format!("jobs:{}", ctx.background_jobs))
        }
        "compaction" => {
            (!session.compactions.is_empty()).then(|| format!("cmp:{}", session.compactions.len()))
        }
        "btw" => {
            if ctx.btw_in == 0 && ctx.btw_out == 0 {
                None
            } else if ctx.btw_cost > 0.0 {
                Some(format!(
                    "btw:${:.4} ({}/{})",
                    ctx.btw_cost,
                    fmt_tokens(ctx.btw_in),
                    fmt_tokens(ctx.btw_out)
                ))
            } else {
                Some(format!(
                    "btw:{}/{}",
                    fmt_tokens(ctx.btw_in),
                    fmt_tokens(ctx.btw_out)
                ))
            }
        }
        _ => None,
    }
}

fn basename(dir: &str) -> String {
    std::path::Path::new(dir)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(dir)
        .to_string()
}

/// Full working-directory path with `$HOME` shortened to `~`.
fn contract_home(dir: &str) -> String {
    if let Some(home) = home_dir_text().as_deref() {
        if dir == home {
            return "~".to_string();
        }
        if let Some(rest) = dir
            .strip_prefix(home)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            return format!("~/{rest}");
        }
    }
    dir.to_string()
}

/// The process home directory as text, resolved once per process (tests
/// keep reading their thread-local override).
fn home_dir_text() -> Option<std::borrow::Cow<'static, str>> {
    fn resolve() -> Option<String> {
        crate::paths::process_home_dir().and_then(|h| h.to_str().map(str::to_owned))
    }
    #[cfg(test)]
    {
        resolve().map(std::borrow::Cow::Owned)
    }
    #[cfg(not(test))]
    {
        static HOME: OnceLock<Option<String>> = OnceLock::new();
        HOME.get_or_init(resolve)
            .as_deref()
            .map(std::borrow::Cow::Borrowed)
    }
}

/// Name of the linked git worktree for `dir` (from a `.git` file pointing into
/// `.../worktrees/<name>`), or `None` when this is not a linked worktree.
fn git_worktree(dir: &str) -> Option<String> {
    // Read once per workspace instead of on every statusline build: a
    // directory does not turn into (or stop being) a linked worktree while
    // the TUI shows it.
    static LAST: std::sync::Mutex<Option<(String, Option<String>)>> = std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|error| error.into_inner());
    if let Some((cached_dir, name)) = &*last
        && cached_dir == dir
    {
        return name.clone();
    }
    let name = read_git_worktree(dir);
    *last = Some((dir.to_owned(), name.clone()));
    name
}

fn read_git_worktree(dir: &str) -> Option<String> {
    let dot_git = std::path::Path::new(dir).join(".git");
    if !dot_git.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = content.strip_prefix("gitdir:")?.trim();
    let p = std::path::Path::new(gitdir);
    // .../.git/worktrees/<name>
    if p.components().any(|c| c.as_os_str() == "worktrees") {
        return p
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
    }
    None
}

fn elapsed_secs(rfc3339: &str) -> Option<i64> {
    let then = chrono::DateTime::parse_from_rfc3339(rfc3339).ok()?;
    Some(
        (chrono::Utc::now() - then.with_timezone(&chrono::Utc))
            .num_seconds()
            .max(0),
    )
}

/// A value that changes whenever [`fmt_elapsed`]'s text can: every second
/// below a minute, then every minute (coarser displays merely rebuild more
/// often than needed). Hashing it avoids formatting a string per frame.
fn elapsed_bucket(rfc3339: &str) -> Option<i64> {
    let secs = elapsed_secs(rfc3339)?;
    Some(if secs < 60 { secs } else { 60 + secs / 60 })
}

/// Short human elapsed time since an RFC3339 timestamp (e.g. `5m`, `2h10m`).
fn fmt_elapsed(rfc3339: &str) -> Option<String> {
    let secs = elapsed_secs(rfc3339)?;
    Some(if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    })
}

/// Machine hostname, resolved once per process.
fn hostname() -> Option<String> {
    static HOST: OnceLock<Option<String>> = OnceLock::new();
    HOST.get_or_init(read_hostname).clone()
}

/// Machine hostname from env, falling back to `/etc/hostname` on unix.
fn read_hostname() -> Option<String> {
    if let Ok(h) = std::env::var("HOSTNAME").or_else(|_| std::env::var("HOST"))
        && !h.is_empty()
    {
        return Some(h);
    }
    #[cfg(unix)]
    if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
        let h = h.trim();
        if !h.is_empty() {
            return Some(h.to_string());
        }
    }
    None
}

/// `+staged ~modified -deleted ?untracked`, only the non-zero parts; `None`
/// when the tree is clean.
pub fn format_changes(g: &GitStatus) -> Option<String> {
    if !g.is_dirty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if g.staged > 0 {
        parts.push(format!("+{}", g.staged));
    }
    if g.modified > 0 {
        parts.push(format!("~{}", g.modified));
    }
    if g.deleted > 0 {
        parts.push(format!("-{}", g.deleted));
    }
    if g.untracked > 0 {
        parts.push(format!("?{}", g.untracked));
    }
    Some(parts.join(" "))
}

/// Upstream sync plus a clean/dirty marker: `↑1 ↓2 *`, or `✓` when clean and
/// in sync.
pub fn format_status(g: &GitStatus) -> String {
    let mut parts: Vec<String> = Vec::new();
    if g.ahead > 0 {
        parts.push(format!("\u{2191}{}", g.ahead));
    }
    if g.behind > 0 {
        parts.push(format!("\u{2193}{}", g.behind));
    }
    if g.is_dirty() {
        parts.push("*".to_string());
    }
    if parts.is_empty() {
        "\u{2713}".to_string()
    } else {
        parts.join(" ")
    }
}

/// Resolve a segment's `icon` setting to a glyph: `Auto(true)` uses the item's
/// built-in icon, a custom string is looked up by name (or used literally), and
/// anything else yields no icon. Needs a Nerd Font to render.
fn resolve_icon<'a>(icon: Option<&'a IconSpec>, item: &str) -> Option<&'a str> {
    match icon {
        Some(IconSpec::Auto(true)) => item_icon(item),
        Some(IconSpec::Custom(s)) => Some(icon_str(s)),
        _ => None,
    }
}

/// Built-in Nerd Font icon for an item, when one fits.
pub fn item_icon(item: &str) -> Option<&'static str> {
    let g = match item {
        "git_branch" => "\u{e0a0}",                                          //
        "git_changes" => "\u{f044}",                                         //
        "git_status" => "\u{f021}",                                          //
        "cwd" | "cwd_full" => "\u{f07b}",                                    //
        "worktree" => "\u{f126}",                                            //
        "model" | "model_short" => "\u{f2db}",                               //
        "provider" => "\u{f0c2}",                                            //
        "cost" => "\u{f155}",                                                //
        "context_used" | "context_max" | "context_percentage" => "\u{f1c0}", //
        "session_name" | "session_id" => "\u{f292}",                         //
        "message_count" => "\u{f086}",                                       //
        "session_age" | "session_updated" | "clock" => "\u{f017}",           //
        "host" => "\u{f233}",                                                //
        "user" => "\u{f007}",                                                //
        "reasoning" => "\u{f0eb}",                                           //
        "prompt" => "\u{f120}",                                              //
        "mode" => "\u{f023}",                                                //
        "loop" => "\u{f01e}",                                                //
        "goal" => "\u{f140}",                                                //
        "btw" => "\u{f075}",                                                 //
        "compaction" => "\u{f066}",                                          //
        _ => return None,
    };
    Some(g)
}

/// Named icon lookup, with passthrough so any literal glyph also works.
fn icon_str(name: &str) -> &str {
    match name {
        "branch" | "git" => "\u{e0a0}",
        "folder" | "dir" => "\u{f07b}",
        "chip" | "model" => "\u{f2db}",
        "dollar" | "money" => "\u{f155}",
        "database" | "context" => "\u{f1c0}",
        "hash" => "\u{f292}",
        "terminal" => "\u{f120}",
        "lock" => "\u{f023}",
        "pencil" | "edit" => "\u{f044}",
        "sync" | "refresh" => "\u{f021}",
        other => other,
    }
}

/// Resolve a powerline cap name to its glyph, or return the input unchanged so
/// any literal string (including a raw Nerd Font codepoint) also works. These
/// glyphs need a Nerd Font / Powerline-patched font to render.
pub fn powerline_glyph(name: &str) -> String {
    powerline_str(name).to_string()
}

fn powerline_str(name: &str) -> &str {
    match name {
        "pl_right" | "powerline_right" => "\u{e0b0}",   //
        "pl_left" | "powerline_left" => "\u{e0b2}",     //
        "pl_right_thin" => "\u{e0b1}",                  //
        "pl_left_thin" => "\u{e0b3}",                   //
        "pl_round_right" | "round_right" => "\u{e0b4}", //
        "pl_round_left" | "round_left" => "\u{e0b6}",   //
        "pl_flame_right" => "\u{e0c0}",
        "pl_flame_left" => "\u{e0c2}",
        other => other,
    }
}

pub fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

/// Built-in single-line layout used when `[statusline]` is not configured.
pub fn default_spec() -> StatusLineConfig {
    fn seg(item: &str, c: Option<&str>) -> StatusLineSegment {
        StatusLineSegment {
            item: item.into(),
            color: c.map(|s| s.into()),
            ..Default::default()
        }
    }
    fn sep(text: &str) -> StatusLineSegment {
        StatusLineSegment {
            item: "separator".into(),
            text: Some(text.into()),
            ..Default::default()
        }
    }
    let segments = vec![
        seg("cwd", None),
        sep("  "),
        StatusLineSegment {
            item: "git_branch".into(),
            color: Some("dark_grey".into()),
            left: Some("(".into()),
            right: Some(")".into()),
            ..Default::default()
        },
        sep(" | "),
        seg("model", None),
        sep(" "),
        seg("reasoning_effort", None),
        sep("  |  "),
        seg("context", None),
        sep("  "),
        seg("calls", None),
        sep(" "),
        seg("tokens_input", None),
        sep(" "),
        seg("tokens_output", None),
        sep(" "),
        seg("cache_hit_ratio", None),
        StatusLineSegment {
            item: "flex_separator".into(),
            ..Default::default()
        },
        seg("sandbox", Some("red")),
        sep(" "),
        seg("loop", None),
        sep(" "),
        seg("goal", None),
        sep(" "),
        seg("mode", None),
        sep(" "),
        seg("todos", None),
        sep(" "),
        seg("queue", None),
        sep(" "),
        seg("background_jobs", None),
        sep(" "),
        #[cfg(feature = "subagents")]
        seg("background_tasks", None),
        #[cfg(feature = "subagents")]
        sep(" "),
        seg("cost", None),
        sep(" "),
        seg("btw", None),
        sep(" "),
        seg("prompt", None),
    ];
    StatusLineConfig {
        lines: vec![StatusLineLine { segments }],
    }
}

#[cfg(test)]
mod context_percentage_tests {
    use super::context_percentage;
    use crate::session::Session;

    #[test]
    fn context_percentage_saturates_instead_of_overflowing() {
        let mut session = Session::new("provider", "model", 1, "/workspace");
        session.overhead_tokens = u64::MAX;
        assert_eq!(context_percentage(&session), Some(u64::MAX));
    }
}
