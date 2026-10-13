use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::ExecutableCommand;
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};

const UTF8_CODE_PAGE: u32 = 65_001;
const DROP_RESTORE_ATTEMPTS: usize = 3;
const DROP_RESTORE_BACKOFF: Duration = Duration::from_millis(10);
static SYSTEM_TERMINAL_ATTACHED: AtomicBool = AtomicBool::new(false);

pub(crate) fn is_attached() -> bool {
    SYSTEM_TERMINAL_ATTACHED.load(Ordering::Acquire)
}

/// COLORFGBG's final ANSI color is the terminal background. Invalid or absent
/// hints remain unknown; this never reads or discards terminal input.
pub(crate) fn background_hint_is_light(hint: &str) -> Option<bool> {
    if hint.len() > 128 {
        return None;
    }
    let index = hint.rsplit(';').next()?.trim().parse::<u8>().ok()?;
    Some(background_color_is_light(
        crossterm::style::Color::AnsiValue(index),
    ))
}

#[derive(Default)]
pub(crate) struct BackgroundQuery {
    pub(crate) color: Option<crossterm::style::Color>,
    pub(crate) pending: std::collections::VecDeque<crossterm::event::Event>,
}

/// Query only before the App starts its input thread. Crossterm remains the
/// sole decoder: unrelated keys, paste, mouse and incomplete replies are
/// retained as their original events for that thread, never read as raw bytes.
pub(crate) fn query_background(enabled: bool) -> BackgroundQuery {
    #[cfg(unix)]
    {
        use std::io::IsTerminal;
        if enabled
            && is_attached()
            && io::stdin().is_terminal()
            && io::stdout().is_terminal()
            && std::env::var("TERM").is_ok_and(|term| term != "dumb")
        {
            let mut out = io::stdout();
            if out
                .write_all(b"\x1b]11;?\x07")
                .and_then(|()| out.flush())
                .is_ok()
            {
                return collect_background_events(Duration::from_millis(150), |remaining| {
                    if crossterm::event::poll(remaining)? {
                        crossterm::event::read().map(Some)
                    } else {
                        Ok(None)
                    }
                });
            }
        }
    }
    #[cfg(not(unix))]
    let _ = enabled;
    BackgroundQuery::default()
}

#[cfg(any(unix, test))]
fn osc_rgb(value: &str) -> Option<crossterm::style::Color> {
    let mut parts = value.split('/');
    let mut component = || {
        let value = parts.next()?;
        if value.is_empty() || value.len() > 4 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let number = u32::from_str_radix(value, 16).ok()?;
        let maximum = (1u32 << (4 * value.len())) - 1;
        u8::try_from((number * 255 + maximum / 2) / maximum).ok()
    };
    let (r, g, b) = (component()?, component()?, component()?);
    if parts.next().is_some() {
        return None;
    }
    Some(crossterm::style::Color::Rgb { r, g, b })
}

#[cfg(any(unix, test))]
fn osc_rgb_prefix(value: &str) -> bool {
    let parts: Vec<_> = value.split('/').collect();
    parts.len() <= 3
        && parts.iter().enumerate().all(|(index, part)| {
            part.len() <= 4
                && (index + 1 == parts.len() || !part.is_empty())
                && part.bytes().all(|b| b.is_ascii_hexdigit())
        })
}

#[derive(Default)]
#[cfg(any(unix, test))]
struct BackgroundReply {
    wire: String,
    events: std::collections::VecDeque<crossterm::event::Event>,
}

#[cfg(any(unix, test))]
impl BackgroundReply {
    fn consume(
        &mut self,
        event: crossterm::event::Event,
        pending: &mut std::collections::VecDeque<crossterm::event::Event>,
    ) -> Option<crossterm::style::Color> {
        use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
        // The Unix decoder presents OSC's ESC] as Alt+] and its BEL as Ctrl+G.
        // Split ESC/ST fragments also work. Paste events remain wholly opaque.
        let piece = match &event {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc if key.modifiers.is_empty() => Some("\x1b".to_owned()),
                KeyCode::Char(']') if key.modifiers == KeyModifiers::ALT => {
                    Some("\x1b]".to_owned())
                }
                KeyCode::Char('\\') if key.modifiers == KeyModifiers::ALT => {
                    Some("\x1b\\".to_owned())
                }
                KeyCode::Char('g') if key.modifiers == KeyModifiers::CONTROL => {
                    Some("\x07".to_owned())
                }
                KeyCode::Char(c)
                    if c.is_ascii()
                        && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
                {
                    Some(c.to_string())
                }
                _ => None,
            },
            _ => None,
        };
        let Some(piece) = piece else {
            self.flush(pending);
            pending.push_back(event);
            return None;
        };
        self.wire.push_str(&piece);
        self.events.push_back(event);
        const PREFIX: &str = "\x1b]11;rgb:";
        if let Some(body) = self.wire.strip_prefix(PREFIX) {
            let completed = body
                .strip_suffix('\x07')
                .or_else(|| body.strip_suffix("\x1b\\"));
            if let Some(body) = completed {
                if let Some(color) = osc_rgb(body) {
                    self.wire.clear();
                    self.events.clear();
                    return Some(color);
                }
            } else if self.wire.len() <= 32
                && osc_rgb_prefix(body.strip_suffix('\x1b').unwrap_or(body))
            {
                return None;
            }
        } else if PREFIX.starts_with(&self.wire) {
            return None;
        }
        self.flush(pending);
        None
    }

    fn flush(&mut self, pending: &mut std::collections::VecDeque<crossterm::event::Event>) {
        pending.append(&mut self.events);
        self.wire.clear();
    }
}

#[cfg(any(unix, test))]
fn collect_background_events(
    timeout: Duration,
    mut next: impl FnMut(Duration) -> io::Result<Option<crossterm::event::Event>>,
) -> BackgroundQuery {
    let deadline = std::time::Instant::now() + timeout;
    let mut result = BackgroundQuery::default();
    let mut reply = BackgroundReply::default();
    // Bound both the absolute wait and consumed events. The 128th event is
    // preserved too; everything not consumed remains in crossterm's queue.
    for _ in 0..128 {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(Some(event)) = next(remaining) else {
            break;
        };
        if let Some(color) = reply.consume(event, &mut result.pending) {
            result.color = Some(color);
            break;
        }
    }
    reply.flush(&mut result.pending);
    result
}

pub(crate) fn background_color_is_light(color: crossterm::style::Color) -> bool {
    use crossterm::style::Color;
    let ansi = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let (r, g, b) = match color {
        Color::Rgb { r, g, b } => (r, g, b),
        Color::AnsiValue(index @ 0..=15) => ansi[index as usize],
        Color::AnsiValue(index @ 16..=231) => {
            let cube = [0, 95, 135, 175, 215, 255];
            let index = usize::from(index - 16);
            (cube[index / 36], cube[(index / 6) % 6], cube[index % 6])
        }
        Color::AnsiValue(index) => {
            let gray = 8 + (index - 232) * 10;
            (gray, gray, gray)
        }
        Color::Black => ansi[0],
        Color::DarkRed => ansi[1],
        Color::DarkGreen => ansi[2],
        Color::DarkYellow => ansi[3],
        Color::DarkBlue => ansi[4],
        Color::DarkMagenta => ansi[5],
        Color::DarkCyan => ansi[6],
        Color::Grey => ansi[7],
        Color::DarkGrey => ansi[8],
        Color::Red => ansi[9],
        Color::Green => ansi[10],
        Color::Yellow => ansi[11],
        Color::Blue => ansi[12],
        Color::Magenta => ansi[13],
        Color::Cyan => ansi[14],
        Color::White => ansi[15],
        Color::Reset => return false,
    };
    299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b) >= 128_000
}

#[cfg(test)]
mod background_hint_tests {
    use super::*;

    #[test]
    fn background_hints_distinguish_light_dark_and_unknown_without_input_io() {
        for value in ["15;0", "0", "15;16", "15;232"] {
            assert_eq!(background_hint_is_light(value), Some(false), "{value}");
        }
        for value in ["0;15", "15", "0;7", "0;231", "0;255"] {
            assert_eq!(background_hint_is_light(value), Some(true), "{value}");
        }
        for value in ["", "0;", "0;256", "0;-1", "rgb:ffff/ffff/ffff", "default"] {
            assert_eq!(background_hint_is_light(value), None, "{value}");
        }
        assert_eq!(background_hint_is_light(&"0".repeat(129)), None);
        assert!(background_color_is_light(crossterm::style::Color::White));
        assert!(!background_color_is_light(crossterm::style::Color::Black));
    }

    fn key(
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(code, modifiers))
    }

    fn reply(body: &str, split_escape: bool, st: bool) -> Vec<crossterm::event::Event> {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut events = if split_escape {
            vec![
                key(KeyCode::Esc, KeyModifiers::NONE),
                key(KeyCode::Char(']'), KeyModifiers::NONE),
            ]
        } else {
            vec![key(KeyCode::Char(']'), KeyModifiers::ALT)]
        };
        events.extend(format!("11;rgb:{body}").chars().map(|c| {
            key(
                KeyCode::Char(c),
                if c.is_uppercase() {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                },
            )
        }));
        if st && split_escape {
            events.extend([
                key(KeyCode::Esc, KeyModifiers::NONE),
                key(KeyCode::Char('\\'), KeyModifiers::NONE),
            ]);
        } else if st {
            events.push(key(KeyCode::Char('\\'), KeyModifiers::ALT));
        } else {
            events.push(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        }
        events
    }

    fn collect(events: Vec<crossterm::event::Event>) -> BackgroundQuery {
        let mut events = events.into_iter();
        collect_background_events(Duration::from_secs(1), |_| Ok(events.next()))
    }

    #[test]
    fn osc_background_accepts_bel_st_and_split_escape_with_hex_scaling() {
        use crossterm::style::Color;
        for (body, expected) in [
            (
                "ffff/ffff/ffff",
                Color::Rgb {
                    r: 255,
                    g: 255,
                    b: 255,
                },
            ),
            ("0/0/0", Color::Rgb { r: 0, g: 0, b: 0 }),
            (
                "ff/80/00",
                Color::Rgb {
                    r: 255,
                    g: 128,
                    b: 0,
                },
            ),
            (
                "FFF/800/000",
                Color::Rgb {
                    r: 255,
                    g: 128,
                    b: 0,
                },
            ),
        ] {
            for split in [false, true] {
                for st in [false, true] {
                    let result = collect(reply(body, split, st));
                    assert_eq!(result.color, Some(expected), "{body}/{split}/{st}");
                    assert!(result.pending.is_empty());
                }
            }
        }
    }

    #[test]
    fn osc_background_preserves_opaque_input_and_exact_key_state_in_order() {
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
        };
        let unrelated = vec![
            key(KeyCode::Char('雪'), KeyModifiers::NONE),
            Event::Paste("\x1b]11;rgb:ffff/ffff/ffff\x07\n雪".into()),
            Event::Resize(91, 37),
            Event::FocusLost,
            Event::Key(KeyEvent {
                code: KeyCode::Left,
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Release,
                state: KeyEventState::CAPS_LOCK,
            }),
            Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollUp,
                column: 2,
                row: 4,
                modifiers: KeyModifiers::ALT,
            }),
        ];
        let mut input = unrelated.clone();
        input.extend(reply("ffff/ffff/ffff", false, false));
        let result = collect(input);
        assert!(result.color.is_some());
        assert_eq!(result.pending.into_iter().collect::<Vec<_>>(), unrelated);
        // Pasted response-looking text alone is task input, not a color reply.
        assert!(collect(vec![unrelated[1].clone()]).color.is_none());
    }

    #[test]
    fn osc_background_invalid_incomplete_and_interleaved_replies_replay_exactly() {
        use crossterm::event::{Event, KeyCode, KeyModifiers};
        for body in [
            "fffff/0/0",
            "ff/ff",
            "ff//ff",
            "ff/ff/ff/ff",
            "gg/0/0",
            "-1/0/0",
        ] {
            let input = reply(body, false, false);
            let result = collect(input.clone());
            assert!(result.color.is_none(), "{body}");
            assert_eq!(result.pending.into_iter().collect::<Vec<_>>(), input);
        }
        let mut incomplete = reply("ff/ff/ff", true, true);
        incomplete.pop();
        let mut interleaved = reply("ff/ff/ff", false, false);
        interleaved.insert(6, Event::Resize(20, 10));
        let invalid_key = vec![
            key(KeyCode::Char(']'), KeyModifiers::ALT),
            key(KeyCode::Char('x'), KeyModifiers::CONTROL),
        ];
        for input in [incomplete, interleaved, invalid_key] {
            let result = collect(input.clone());
            assert!(result.color.is_none());
            assert_eq!(result.pending.into_iter().collect::<Vec<_>>(), input);
        }
    }

    #[test]
    fn osc_background_deadline_event_bound_and_read_errors_retain_consumed_input() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let normal = key(KeyCode::Char('x'), KeyModifiers::NONE);
        let mut calls = 0;
        let result = collect_background_events(Duration::from_secs(1), |_| {
            calls += 1;
            Ok(Some(normal.clone()))
        });
        assert_eq!(calls, 128);
        assert_eq!(result.pending.len(), 128);
        assert!(result.color.is_none());
        let first = key(KeyCode::Char(']'), KeyModifiers::ALT);
        let mut calls = 0;
        let result = collect_background_events(Duration::from_secs(1), |_| {
            calls += 1;
            if calls == 1 {
                Ok(Some(first.clone()))
            } else {
                Err(io::Error::other("fixture read failure"))
            }
        });
        assert_eq!(result.pending.into_iter().collect::<Vec<_>>(), vec![first]);
        let mut calls = 0;
        let result = collect_background_events(Duration::from_millis(5), |_| {
            calls += 1;
            std::thread::sleep(Duration::from_millis(20));
            Ok(Some(normal.clone()))
        });
        assert_eq!(calls, 1);
        assert_eq!(result.pending.into_iter().collect::<Vec<_>>(), vec![normal]);
    }
}

#[derive(Debug)]
pub struct TerminalLifecycleError {
    operation: &'static str,
    source: io::Error,
}

impl TerminalLifecycleError {
    fn new(operation: &'static str, source: io::Error) -> Self {
        Self { operation, source }
    }
}

impl std::fmt::Display for TerminalLifecycleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "terminal {} failed: {}",
            self.operation, self.source
        )
    }
}

impl std::error::Error for TerminalLifecycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConsoleCodePages {
    input: u32,
    output: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalAction {
    EnterAlternate,
    Clear,
    EnableMouse,
    EnablePaste,
    PushKeyboard,
    EnableRaw,
    PushTitle,
    PopKeyboard,
    DisablePaste,
    DisableMouse,
    /// Discard terminal input that is still queued (mouse reports, CSI-u key
    /// sequences, mashed keys) so the shell never echoes it as text.
    FlushInput,
    DisableRaw,
    LeaveAlternate,
    ShowCursor,
    PopTitle,
}

/// Restoration order. Input reporting (keyboard protocol, bracketed paste,
/// mouse) is switched off while the terminal is still raw, then the queued
/// input is discarded, and only then does the terminal return to cooked mode.
/// Leaving raw mode first lets the line discipline echo in-flight reports such
/// as `^[[<35;10;5M` onto the user's shell prompt.
const TEARDOWN_ORDER: [TerminalAction; 8] = [
    TerminalAction::PopKeyboard,
    TerminalAction::DisablePaste,
    TerminalAction::DisableMouse,
    TerminalAction::FlushInput,
    TerminalAction::DisableRaw,
    TerminalAction::LeaveAlternate,
    TerminalAction::ShowCursor,
    TerminalAction::PopTitle,
];

/// How long teardown waits for reports the terminal already sent to arrive
/// before discarding queued input.
const INPUT_DRAIN_GRACE: Duration = Duration::from_millis(15);
static TITLE_PUSHED: AtomicBool = AtomicBool::new(false);

/// Options fixed for the lifetime of the terminal attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalOptions {
    /// Enable mouse reporting (config `mouse_capture`). Without it the
    /// terminal keeps its native selection.
    pub mouse_capture: bool,
    /// Save the terminal title on attach and restore it on detach, so status
    /// titles set while running (config `terminal_title`) do not outlive us.
    pub title_status: bool,
}

impl Default for TerminalOptions {
    fn default() -> Self {
        Self {
            mouse_capture: true,
            title_status: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Undo {
    Action(TerminalAction),
    InputCodePage(u32),
    OutputCodePage(u32),
}

trait TerminalOperations {
    fn console_code_pages(&mut self) -> io::Result<Option<ConsoleCodePages>>;
    fn set_console_input_code_page(&mut self, code_page: u32) -> io::Result<()>;
    fn set_console_output_code_page(&mut self, code_page: u32) -> io::Result<()>;
    fn apply(&mut self, action: TerminalAction) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

struct SystemTerminal;

impl TerminalOperations for SystemTerminal {
    fn console_code_pages(&mut self) -> io::Result<Option<ConsoleCodePages>> {
        system_console_code_pages()
    }

    fn set_console_input_code_page(&mut self, code_page: u32) -> io::Result<()> {
        set_system_console_input_code_page(code_page)
    }

    fn set_console_output_code_page(&mut self, code_page: u32) -> io::Result<()> {
        set_system_console_output_code_page(code_page)
    }

    fn apply(&mut self, action: TerminalAction) -> io::Result<()> {
        let mut stdout = std::io::stdout();
        match action {
            TerminalAction::EnterAlternate => stdout.execute(EnterAlternateScreen).map(drop),
            TerminalAction::Clear => stdout.execute(Clear(ClearType::All)).map(drop),
            TerminalAction::EnableMouse => stdout.execute(EnableMouseCapture).map(drop),
            TerminalAction::EnablePaste => stdout.execute(EnableBracketedPaste).map(drop),
            TerminalAction::PushKeyboard => stdout
                .execute(PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
                ))
                .map(drop),
            TerminalAction::EnableRaw => terminal::enable_raw_mode(),
            // XTWINOPS title stack: save icon+window title. Terminals without
            // the stack ignore it; the reset below then falls back to default.
            TerminalAction::PushTitle => {
                TITLE_PUSHED.store(true, Ordering::Release);
                stdout
                    .write_all(b"\x1b[22;0t")
                    .and_then(|()| stdout.flush())
            }
            TerminalAction::DisableRaw => terminal::disable_raw_mode(),
            TerminalAction::PopKeyboard => stdout.execute(PopKeyboardEnhancementFlags).map(drop),
            TerminalAction::DisablePaste => stdout.execute(DisableBracketedPaste).map(drop),
            TerminalAction::DisableMouse => stdout.execute(DisableMouseCapture).map(drop),
            TerminalAction::FlushInput => {
                stdout.flush()?;
                std::thread::sleep(INPUT_DRAIN_GRACE);
                discard_pending_input();
                Ok(())
            }
            TerminalAction::LeaveAlternate => stdout.execute(LeaveAlternateScreen).map(drop),
            TerminalAction::ShowCursor => stdout.execute(Show).map(drop),
            TerminalAction::PopTitle => {
                TITLE_PUSHED.store(false, Ordering::Release);
                stdout
                    .write_all(b"\x1b]0;\x07\x1b[23;0t")
                    .and_then(|()| stdout.flush())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        std::io::stdout().flush()
    }
}

/// Agent activity reported through the terminal title (config
/// `terminal_title`), for multiplexers and tab bars that surface titles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentActivity {
    Idle,
    Working,
    WaitingForApproval,
}

impl AgentActivity {
    pub(crate) fn label(self) -> &'static str {
        match self {
            AgentActivity::Idle => "idle",
            AgentActivity::Working => "working",
            AgentActivity::WaitingForApproval => "waiting for approval",
        }
    }
}

/// OSC 0 (icon name and window title) announcing `activity`. The text is a
/// fixed product name and label, never model or tool output.
pub(crate) fn title_sequence(activity: AgentActivity) -> String {
    format!(
        "\x1b]0;{}: {}\x07",
        crate::product::PUBLIC_NAME,
        activity.label()
    )
}

/// Opt-in escape sequences emitted when the agent activity changes (config
/// `terminal_notify`, `terminal_prompt_marks`), besides the title.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ActivitySignals {
    pub notify: crate::config::TerminalNotify,
    pub prompt_marks: bool,
}

impl ActivitySignals {
    pub(crate) fn any(self) -> bool {
        self.notify != crate::config::TerminalNotify::Off || self.prompt_marks
    }
}

/// Make `text` safe as an OSC string payload: control characters (which
/// could end the sequence and start another) and `;` (the field separator
/// of OSC 777) are dropped. Payloads are fixed product text today; this
/// keeps them safe if anything variable is ever added.
fn osc_payload(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() && *c != ';')
        .collect()
}

/// Desktop notification announcing `body` in the configured style.
fn notification_sequence(style: crate::config::TerminalNotify, body: &str) -> String {
    use crate::config::TerminalNotify;
    let title = osc_payload(crate::product::PUBLIC_NAME);
    let body = osc_payload(body);
    match style {
        TerminalNotify::Off => String::new(),
        TerminalNotify::Osc9 => format!("\x1b]9;{title}: {body}\x07"),
        TerminalNotify::Osc777 => format!("\x1b]777;notify;{title};{body}\x07"),
    }
}

const PROMPT_START: &str = "\x1b]133;A\x07";
const INPUT_START: &str = "\x1b]133;B\x07";
const COMMAND_START: &str = "\x1b]133;C\x07";
const COMMAND_END: &str = "\x1b]133;D\x07";

/// The opt-in sequences for a change of activity from `previous` to
/// `next` (empty when nothing changed or nothing is enabled).
///
/// Notifications: a run that ends (to idle) announces "turn finished", and
/// a permission prompt announces "waiting for approval"; the first frame
/// announces nothing. Prompt marks (OSC 133): waiting for input (idle or a
/// permission prompt) is a prompt (`A`, `B`), a run is a command (`C`), and
/// a run ending closes it (`D`) before the next prompt. The text is fixed,
/// never model or tool output.
pub(crate) fn activity_sequences(
    previous: Option<AgentActivity>,
    next: AgentActivity,
    signals: ActivitySignals,
) -> String {
    if previous == Some(next) {
        return String::new();
    }
    let mut out = String::new();
    if signals.prompt_marks {
        match next {
            AgentActivity::Working => out.push_str(COMMAND_START),
            AgentActivity::Idle | AgentActivity::WaitingForApproval => {
                if next == AgentActivity::Idle && previous.is_some() {
                    out.push_str(COMMAND_END);
                }
                out.push_str(PROMPT_START);
                out.push_str(INPUT_START);
            }
        }
    }
    let body = match (previous, next) {
        (Some(_), AgentActivity::Idle) => Some("turn finished"),
        (Some(_), AgentActivity::WaitingForApproval) => Some("waiting for approval"),
        _ => None,
    };
    if let Some(body) = body {
        out.push_str(&notification_sequence(signals.notify, body));
    }
    out
}

/// Best effort: discard bytes the terminal already queued on stdin. Failure
/// (stdin redirected, not a tty) leaves nothing to discard.
#[cfg(unix)]
fn discard_pending_input() {
    use std::os::fd::AsFd;
    let _ = discard_queued_input(std::io::stdin().as_fd());
}

#[cfg(unix)]
fn discard_queued_input(fd: std::os::fd::BorrowedFd<'_>) -> nix::Result<()> {
    nix::sys::termios::tcflush(fd, nix::sys::termios::FlushArg::TCIFLUSH)
}

/// Windows console input records are not echoed by the shell after exit.
#[cfg(not(unix))]
fn discard_pending_input() {}

#[cfg(windows)]
#[allow(unsafe_code)]
fn system_console_code_pages() -> io::Result<Option<ConsoleCodePages>> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleCP, GetConsoleMode, GetConsoleOutputCP, GetStdHandle, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    fn attached(handle_kind: u32) -> bool {
        let handle = unsafe { GetStdHandle(handle_kind) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut mode = 0;
        unsafe { GetConsoleMode(handle, &mut mode) != 0 }
    }

    // Code pages belong to the process console rather than an individual stream. Requiring both
    // handles to support console modes prevents redirected/headless execution from mutating them.
    if !attached(STD_INPUT_HANDLE) || !attached(STD_OUTPUT_HANDLE) {
        return Ok(None);
    }

    let input = unsafe { GetConsoleCP() };
    if input == 0 {
        return Err(io::Error::last_os_error());
    }
    let output = unsafe { GetConsoleOutputCP() };
    if output == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(ConsoleCodePages { input, output }))
}

#[cfg(not(windows))]
fn system_console_code_pages() -> io::Result<Option<ConsoleCodePages>> {
    Ok(None)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn set_system_console_input_code_page(code_page: u32) -> io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleCP;

    if unsafe { SetConsoleCP(code_page) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn set_system_console_input_code_page(_code_page: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn set_system_console_output_code_page(code_page: u32) -> io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleOutputCP;

    if unsafe { SetConsoleOutputCP(code_page) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn set_system_console_output_code_page(_code_page: u32) -> io::Result<()> {
    Ok(())
}

struct TerminalSession<T: TerminalOperations> {
    terminal: T,
    undo: Vec<Undo>,
    attached: bool,
    options: TerminalOptions,
}

impl<T: TerminalOperations> TerminalSession<T> {
    #[cfg(test)]
    fn new(terminal: T) -> io::Result<Self> {
        Self::with_options(terminal, TerminalOptions::default())
    }

    fn with_options(terminal: T, options: TerminalOptions) -> io::Result<Self> {
        let mut session = Self {
            terminal,
            undo: Vec::with_capacity(TEARDOWN_ORDER.len() + 2),
            attached: false,
            options,
        };
        session.resume()?;
        Ok(session)
    }

    fn resume(&mut self) -> io::Result<()> {
        if self.attached {
            return Ok(());
        }
        if !self.undo.is_empty() {
            self.restore()?;
        }
        if let Err(attach_error) = self.attach() {
            return match self.restore() {
                Ok(()) => Err(attach_error),
                Err(restore_error) => Err(io::Error::other(format!(
                    "terminal attachment failed and restoration was incomplete: {attach_error}; {restore_error}"
                ))),
            };
        }
        self.attached = true;
        Ok(())
    }

    fn attach(&mut self) -> io::Result<()> {
        if let Some(code_pages) = self.terminal.console_code_pages()? {
            self.terminal.set_console_input_code_page(UTF8_CODE_PAGE)?;
            self.undo.push(Undo::InputCodePage(code_pages.input));
            self.terminal.set_console_output_code_page(UTF8_CODE_PAGE)?;
            self.undo.push(Undo::OutputCodePage(code_pages.output));
        }

        // The undo stack runs in reverse, so this attach order yields
        // `TEARDOWN_ORDER`: input reporting stops while the terminal is still
        // raw, queued input is discarded, and only then is cooked mode back.
        if self.options.title_status && self.terminal.apply(TerminalAction::PushTitle).is_ok() {
            self.undo.push(Undo::Action(TerminalAction::PopTitle));
        }
        // Rendering hides the caret; whatever hid it, detaching shows it.
        self.undo.push(Undo::Action(TerminalAction::ShowCursor));
        self.apply_with_undo(
            TerminalAction::EnterAlternate,
            TerminalAction::LeaveAlternate,
        )?;
        self.terminal.apply(TerminalAction::Clear)?;
        self.apply_with_undo(TerminalAction::EnableRaw, TerminalAction::DisableRaw)?;
        self.undo.push(Undo::Action(TerminalAction::FlushInput));
        if self.options.mouse_capture {
            self.apply_with_undo(TerminalAction::EnableMouse, TerminalAction::DisableMouse)?;
        }
        self.apply_with_undo(TerminalAction::EnablePaste, TerminalAction::DisablePaste)?;
        // Enhancement negotiation is unsupported by some otherwise valid terminals. Preserve the
        // existing best-effort behavior while remembering a successful push for symmetric cleanup.
        if self.terminal.apply(TerminalAction::PushKeyboard).is_ok() {
            self.undo.push(Undo::Action(TerminalAction::PopKeyboard));
        }
        Ok(())
    }

    fn apply_with_undo(&mut self, action: TerminalAction, undo: TerminalAction) -> io::Result<()> {
        self.terminal.apply(action)?;
        self.undo.push(Undo::Action(undo));
        Ok(())
    }

    fn suspend(&mut self) -> io::Result<()> {
        if !self.attached && self.undo.is_empty() {
            return Ok(());
        }
        self.attached = false;
        self.restore()
    }

    fn restore(&mut self) -> io::Result<()> {
        let mut failed = Vec::new();
        let mut first_error = None;
        while let Some(undo) = self.undo.pop() {
            let result = match undo {
                Undo::Action(action) => self.terminal.apply(action),
                Undo::InputCodePage(code_page) => {
                    self.terminal.set_console_input_code_page(code_page)
                }
                Undo::OutputCodePage(code_page) => {
                    self.terminal.set_console_output_code_page(code_page)
                }
            };
            if let Err(error) = result {
                failed.push(undo);
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        // Preserve the original reverse-cleanup priority for a later retry by Drop/resume.
        failed.reverse();
        self.undo.extend(failed);
        if let Err(error) = self.terminal.flush()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl<T: TerminalOperations> Drop for TerminalSession<T> {
    fn drop(&mut self) {
        self.attached = false;
        for attempt in 0..DROP_RESTORE_ATTEMPTS {
            if self.restore().is_ok() {
                return;
            }
            if self.undo.is_empty() {
                break;
            }
            if attempt + 1 < DROP_RESTORE_ATTEMPTS {
                std::thread::sleep(DROP_RESTORE_BACKOFF);
            }
        }
        tracing::error!(
            remaining_actions = self.undo.len(),
            "terminal restoration remained incomplete after bounded retries"
        );
    }
}

pub struct TerminalGuard {
    session: TerminalSession<SystemTerminal>,
}

impl TerminalGuard {
    /// What was successfully emitted during this attachment; this does not
    /// query the terminal or claim the terminal implemented each protocol.
    pub(crate) fn diagnostic_protocols(&self) -> (bool, bool, bool) {
        self.session.diagnostic_protocols()
    }
    #[cfg(test)]
    pub(crate) fn detached_for_test() -> Self {
        Self {
            session: TerminalSession {
                terminal: SystemTerminal,
                undo: Vec::new(),
                attached: false,
                options: TerminalOptions::default(),
            },
        }
    }
}

impl<T: TerminalOperations> TerminalSession<T> {
    fn diagnostic_protocols(&self) -> (bool, bool, bool) {
        let requested = |action| {
            self.undo
                .iter()
                .any(|undo| matches!(undo, Undo::Action(candidate) if *candidate == action))
        };
        (
            self.attached,
            requested(TerminalAction::DisablePaste),
            requested(TerminalAction::PopKeyboard),
        )
    }
}

impl TerminalGuard {
    /// Attaches the terminal. `mouse_capture = false` skips enabling mouse
    /// reporting so the terminal's own text selection keeps working.
    pub fn new(options: TerminalOptions) -> Result<Self, TerminalLifecycleError> {
        let guard = Self {
            session: TerminalSession::with_options(SystemTerminal, options)
                .map_err(|error| TerminalLifecycleError::new("attachment", error))?,
        };
        SYSTEM_TERMINAL_ATTACHED.store(true, Ordering::Release);
        Ok(guard)
    }

    pub fn suspend(&mut self) -> Result<(), TerminalLifecycleError> {
        let result = self
            .session
            .suspend()
            .map_err(|error| TerminalLifecycleError::new("suspension", error));
        if result.is_ok() {
            SYSTEM_TERMINAL_ATTACHED.store(false, Ordering::Release);
        }
        result
    }

    pub fn resume(&mut self) -> Result<(), TerminalLifecycleError> {
        let result = self
            .session
            .resume()
            .map_err(|error| TerminalLifecycleError::new("resumption", error));
        if result.is_ok() {
            SYSTEM_TERMINAL_ATTACHED.store(true, Ordering::Release);
        }
        result
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        SYSTEM_TERMINAL_ATTACHED.store(false, Ordering::Release);
    }
}

/// Best-effort emergency restoration used by the panic hook before it prints.
/// Normal teardown still runs during unwinding and retries any failed actions.
pub(crate) fn restore_for_panic() {
    if !SYSTEM_TERMINAL_ATTACHED.swap(false, Ordering::AcqRel) {
        return;
    }
    let mut terminal = SystemTerminal;
    let title_pushed = TITLE_PUSHED.load(Ordering::Acquire);
    for action in TEARDOWN_ORDER {
        if action == TerminalAction::PopTitle && !title_pushed {
            continue;
        }
        let _ = terminal.apply(action);
    }
    let _ = terminal.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Operation {
        Snapshot,
        SetInput(u32),
        SetOutput(u32),
        Action(TerminalAction),
        Flush,
    }

    struct MockState {
        calls: Vec<Operation>,
        fail_at: Option<usize>,
        console_attached: bool,
        input_code_page: u32,
        output_code_page: u32,
        alternate_screen: bool,
        mouse_capture: bool,
        bracketed_paste: bool,
        keyboard_enhancement: bool,
        raw_mode: bool,
        title_pushed: bool,
        cursor_shown: bool,
    }

    impl Default for MockState {
        fn default() -> Self {
            Self {
                calls: Vec::new(),
                fail_at: None,
                console_attached: true,
                input_code_page: 437,
                output_code_page: 850,
                alternate_screen: false,
                mouse_capture: false,
                bracketed_paste: false,
                keyboard_enhancement: false,
                raw_mode: false,
                title_pushed: false,
                cursor_shown: false,
            }
        }
    }

    struct MockTerminal(Arc<Mutex<MockState>>);

    impl MockTerminal {
        fn operation(&self, operation: Operation) -> io::Result<()> {
            let mut state = self.0.lock().unwrap();
            state.calls.push(operation);
            if state.fail_at == Some(state.calls.len()) {
                Err(io::Error::other("injected terminal failure"))
            } else {
                Ok(())
            }
        }

        fn update(
            &self,
            operation: Operation,
            apply: impl FnOnce(&mut MockState),
        ) -> io::Result<()> {
            self.operation(operation)?;
            apply(&mut self.0.lock().unwrap());
            Ok(())
        }
    }

    impl TerminalOperations for MockTerminal {
        fn console_code_pages(&mut self) -> io::Result<Option<ConsoleCodePages>> {
            self.operation(Operation::Snapshot)?;
            let state = self.0.lock().unwrap();
            Ok(state.console_attached.then_some(ConsoleCodePages {
                input: state.input_code_page,
                output: state.output_code_page,
            }))
        }

        fn set_console_input_code_page(&mut self, code_page: u32) -> io::Result<()> {
            self.update(Operation::SetInput(code_page), |state| {
                state.input_code_page = code_page;
            })
        }

        fn set_console_output_code_page(&mut self, code_page: u32) -> io::Result<()> {
            self.update(Operation::SetOutput(code_page), |state| {
                state.output_code_page = code_page;
            })
        }

        fn apply(&mut self, action: TerminalAction) -> io::Result<()> {
            self.update(Operation::Action(action), |state| match action {
                TerminalAction::EnterAlternate => state.alternate_screen = true,
                TerminalAction::EnableMouse => state.mouse_capture = true,
                TerminalAction::EnablePaste => state.bracketed_paste = true,
                TerminalAction::PushKeyboard => state.keyboard_enhancement = true,
                TerminalAction::EnableRaw => state.raw_mode = true,
                TerminalAction::DisableRaw => state.raw_mode = false,
                TerminalAction::PopKeyboard => state.keyboard_enhancement = false,
                TerminalAction::DisablePaste => state.bracketed_paste = false,
                TerminalAction::DisableMouse => state.mouse_capture = false,
                TerminalAction::LeaveAlternate => state.alternate_screen = false,
                TerminalAction::PushTitle => state.title_pushed = true,
                TerminalAction::PopTitle => state.title_pushed = false,
                TerminalAction::ShowCursor => state.cursor_shown = true,
                TerminalAction::Clear | TerminalAction::FlushInput => {}
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            self.operation(Operation::Flush)
        }
    }

    fn mock(fail_at: Option<usize>) -> (MockTerminal, Arc<Mutex<MockState>>) {
        let state = Arc::new(Mutex::new(MockState {
            fail_at,
            ..MockState::default()
        }));
        (MockTerminal(state.clone()), state)
    }

    fn assert_restored(state: &Arc<Mutex<MockState>>) {
        let state = state.lock().unwrap();
        assert_eq!(state.input_code_page, 437);
        assert_eq!(state.output_code_page, 850);
        assert!(!state.alternate_screen);
        assert!(!state.mouse_capture);
        assert!(!state.bracketed_paste);
        assert!(!state.keyboard_enhancement);
        assert!(!state.raw_mode);
        assert!(!state.title_pushed);
    }

    fn assert_cursor_shown(state: &Arc<Mutex<MockState>>) {
        assert!(
            state.lock().unwrap().cursor_shown,
            "detaching must show the caret again"
        );
    }

    #[test]
    fn every_required_attachment_failure_restores_prior_state() {
        // Keyboard enhancement is intentionally best-effort, so its failure is covered separately.
        for fail_at in [1, 2, 3, 4, 5, 6, 7, 8] {
            let (terminal, state) = mock(Some(fail_at));
            assert!(TerminalSession::new(terminal).is_err(), "step {fail_at}");
            assert_restored(&state);
        }
    }

    #[test]
    fn disabled_mouse_capture_never_enables_mouse_reporting() {
        let (terminal, state) = mock(None);
        let session = TerminalSession::with_options(
            terminal,
            TerminalOptions {
                mouse_capture: false,
                title_status: false,
            },
        )
        .unwrap();
        {
            let state = state.lock().unwrap();
            assert!(state.raw_mode);
            assert!(!state.mouse_capture);
            assert!(
                !state
                    .calls
                    .contains(&Operation::Action(TerminalAction::EnableMouse))
            );
        }
        drop(session);
        assert_restored(&state);
    }

    #[test]
    fn optional_keyboard_enhancement_failure_still_attaches_and_restores() {
        let (terminal, state) = mock(Some(9));
        let session = TerminalSession::new(terminal).unwrap();
        assert!(state.lock().unwrap().raw_mode);
        assert!(!state.lock().unwrap().keyboard_enhancement);
        drop(session);
        assert_restored(&state);
        assert!(
            !state
                .lock()
                .unwrap()
                .calls
                .contains(&Operation::Action(TerminalAction::PopKeyboard))
        );
    }

    #[test]
    fn doctor_protocol_metadata_tracks_successful_emission_without_terminal_operations() {
        for (failure, expected) in [(None, (true, true, true)), (Some(9), (true, true, false))] {
            let (terminal, state) = mock(failure);
            let mut session = TerminalSession::new(terminal).unwrap();
            let calls = state.lock().unwrap().calls.clone();
            assert_eq!(session.diagnostic_protocols(), expected);
            assert_eq!(state.lock().unwrap().calls, calls);
            session.suspend().unwrap();
            let calls = state.lock().unwrap().calls.clone();
            assert_eq!(session.diagnostic_protocols(), (false, false, false));
            assert_eq!(state.lock().unwrap().calls, calls);
            assert_restored(&state);
        }
        assert_eq!(
            TerminalGuard::detached_for_test().diagnostic_protocols(),
            (false, false, false)
        );
    }

    #[test]
    fn normal_error_and_panic_paths_restore_every_attached_state() {
        let (terminal, normal) = mock(None);
        drop(TerminalSession::new(terminal).unwrap());
        assert_restored(&normal);
        assert_cursor_shown(&normal);

        let (terminal, panicking) = mock(None);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _session = TerminalSession::new(terminal).unwrap();
            panic!("exercise terminal guard unwinding");
        }));
        assert!(result.is_err());
        assert_restored(&panicking);
        assert_cursor_shown(&panicking);
    }

    #[test]
    fn restoration_continues_after_an_error_and_drop_retries_the_failed_step() {
        // Calls 14 and 17 are respectively raw-mode and output-code-page restoration after the
        // nine successful attachment operations. Every later restoration must still run.
        for fail_at in [14, 17] {
            let (terminal, state) = mock(Some(fail_at));
            let mut session = TerminalSession::new(terminal).unwrap();
            assert!(session.suspend().is_err(), "restore step {fail_at}");
            drop(session);
            assert_restored(&state);
        }
    }

    #[test]
    fn drop_retries_a_transient_restoration_failure() {
        let (terminal, state) = mock(Some(14));
        drop(TerminalSession::new(terminal).unwrap());
        assert_restored(&state);
        assert_eq!(
            state
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|call| **call == Operation::Action(TerminalAction::DisableRaw))
                .count(),
            2
        );
    }

    fn restore_actions(state: &Arc<Mutex<MockState>>, from_call: usize) -> Vec<TerminalAction> {
        state.lock().unwrap().calls[from_call..]
            .iter()
            .filter_map(|call| match call {
                Operation::Action(action) => Some(*action),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn teardown_stops_input_reporting_and_flushes_before_leaving_raw_mode() {
        let (terminal, state) = mock(None);
        let session = TerminalSession::with_options(
            terminal,
            TerminalOptions {
                mouse_capture: true,
                title_status: true,
            },
        )
        .unwrap();
        let attached_calls = state.lock().unwrap().calls.len();
        assert!(state.lock().unwrap().title_pushed);
        drop(session);
        assert_eq!(restore_actions(&state, attached_calls), TEARDOWN_ORDER);
        assert_restored(&state);
    }

    #[test]
    fn teardown_order_disables_every_input_mode_while_still_raw() {
        let position = |action| {
            TEARDOWN_ORDER
                .iter()
                .position(|candidate| *candidate == action)
                .unwrap()
        };
        let raw = position(TerminalAction::DisableRaw);
        for earlier in [
            TerminalAction::PopKeyboard,
            TerminalAction::DisablePaste,
            TerminalAction::DisableMouse,
            TerminalAction::FlushInput,
        ] {
            assert!(position(earlier) < raw, "{earlier:?} must precede raw exit");
        }
        assert!(position(TerminalAction::ShowCursor) > position(TerminalAction::LeaveAlternate));
    }

    /// PTY regression for the "escape sequences printed after exit" report:
    /// reports the terminal queued before teardown must not survive into the
    /// shell's input.
    #[cfg(unix)]
    #[test]
    fn queued_terminal_reports_are_discarded_from_a_pty() {
        use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};
        use std::io::{Read, Write};
        use std::os::fd::AsFd;

        let pty = nix::pty::openpty(None, None).expect("openpty");
        let mut termios = tcgetattr(&pty.slave).unwrap();
        cfmakeraw(&mut termios);
        tcsetattr(&pty.slave, SetArg::TCSANOW, &termios).unwrap();
        let mut master = std::fs::File::from(pty.master);
        let mut slave = std::fs::File::from(pty.slave);

        master.write_all(b"\x1b[<35;10;5M\x1b[99;5u").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        discard_queued_input(slave.as_fd()).unwrap();
        master.write_all(b"Z").unwrap();

        let mut buffer = [0u8; 64];
        let read = slave.read(&mut buffer).unwrap();
        assert_eq!(&buffer[..read], b"Z");
    }

    fn notify(style: crate::config::TerminalNotify) -> ActivitySignals {
        ActivitySignals {
            notify: style,
            prompt_marks: false,
        }
    }

    /// mini-agent-qfrps: a finished turn and a waiting prompt notify in the
    /// configured style; the first frame and unchanged frames do not.
    #[test]
    fn notifications_follow_the_configured_style() {
        use crate::config::TerminalNotify;
        use AgentActivity::*;
        let name = crate::product::PUBLIC_NAME;
        assert_eq!(
            activity_sequences(Some(Working), Idle, notify(TerminalNotify::Osc9)),
            format!("\x1b]9;{name}: turn finished\x07")
        );
        assert_eq!(
            activity_sequences(
                Some(Working),
                WaitingForApproval,
                notify(TerminalNotify::Osc777)
            ),
            format!("\x1b]777;notify;{name};waiting for approval\x07")
        );
        assert_eq!(
            activity_sequences(None, Idle, notify(TerminalNotify::Osc9)),
            ""
        );
        assert_eq!(
            activity_sequences(Some(Idle), Working, notify(TerminalNotify::Osc9)),
            ""
        );
        assert_eq!(
            activity_sequences(Some(Idle), Idle, notify(TerminalNotify::Osc9)),
            ""
        );
        for (previous, next) in [(Some(Working), Idle), (Some(Idle), WaitingForApproval)] {
            assert_eq!(
                activity_sequences(previous, next, ActivitySignals::default()),
                "",
                "everything is off by default"
            );
        }
        assert!(!ActivitySignals::default().any());
    }

    #[test]
    fn prompt_marks_bracket_each_run() {
        use AgentActivity::*;
        let marks = ActivitySignals {
            notify: crate::config::TerminalNotify::Off,
            prompt_marks: true,
        };
        assert_eq!(
            activity_sequences(None, Idle, marks),
            "\x1b]133;A\x07\x1b]133;B\x07"
        );
        assert_eq!(
            activity_sequences(Some(Idle), Working, marks),
            "\x1b]133;C\x07"
        );
        assert_eq!(
            activity_sequences(Some(Working), WaitingForApproval, marks),
            "\x1b]133;A\x07\x1b]133;B\x07"
        );
        assert_eq!(
            activity_sequences(Some(Working), Idle, marks),
            "\x1b]133;D\x07\x1b]133;A\x07\x1b]133;B\x07"
        );
    }

    #[test]
    fn osc_payloads_cannot_break_out_of_their_sequence() {
        assert_eq!(osc_payload("a\x07b\x1b]0;x\x1b\\c;d\ne"), "ab]0x\\cde");
        let sequence = notification_sequence(crate::config::TerminalNotify::Osc777, "x\x07;y");
        assert_eq!(sequence.matches('\x07').count(), 1);
        assert_eq!(sequence.matches('\x1b').count(), 1);
    }

    #[test]
    fn activity_titles_are_fixed_osc_sequences() {
        assert_eq!(
            title_sequence(AgentActivity::Working),
            "\x1b]0;mini-agent: working\x07"
        );
        assert_eq!(
            title_sequence(AgentActivity::WaitingForApproval),
            "\x1b]0;mini-agent: waiting for approval\x07"
        );
        assert_eq!(
            title_sequence(AgentActivity::Idle),
            "\x1b]0;mini-agent: idle\x07"
        );
    }

    #[test]
    fn title_stack_is_left_alone_unless_title_status_is_enabled() {
        let (terminal, state) = mock(None);
        drop(TerminalSession::new(terminal).unwrap());
        let calls = &state.lock().unwrap().calls;
        assert!(!calls.contains(&Operation::Action(TerminalAction::PushTitle)));
        assert!(!calls.contains(&Operation::Action(TerminalAction::PopTitle)));
    }

    #[test]
    fn suspend_and_resume_are_symmetric_and_idempotent() {
        let (terminal, state) = mock(None);
        let mut session = TerminalSession::new(terminal).unwrap();
        session.suspend().unwrap();
        session.suspend().unwrap();
        assert_restored(&state);

        session.resume().unwrap();
        session.resume().unwrap();
        let attached = state.lock().unwrap();
        assert_eq!(attached.input_code_page, UTF8_CODE_PAGE);
        assert_eq!(attached.output_code_page, UTF8_CODE_PAGE);
        assert!(attached.raw_mode);
        assert_eq!(
            attached
                .calls
                .iter()
                .filter(|call| { **call == Operation::Action(TerminalAction::EnterAlternate) })
                .count(),
            2
        );
        drop(attached);
        drop(session);
        assert_restored(&state);
    }

    #[test]
    fn redirected_streams_skip_code_page_mutation() {
        let (terminal, state) = mock(None);
        state.lock().unwrap().console_attached = false;
        drop(TerminalSession::new(terminal).unwrap());
        let state = state.lock().unwrap();
        assert!(state.calls.contains(&Operation::Snapshot));
        assert!(
            !state
                .calls
                .iter()
                .any(|call| matches!(call, Operation::SetInput(_) | Operation::SetOutput(_)))
        );
    }

    #[test]
    fn windows_console_attachment_uses_native_code_page_apis() {
        let source = include_str!("terminal.rs");
        for required in [
            "GetConsoleMode",
            "GetConsoleCP",
            "GetConsoleOutputCP",
            "SetConsoleCP",
            "SetConsoleOutputCP",
        ] {
            assert!(source.contains(required), "missing {required}");
        }
        assert!(source.contains("STD_INPUT_HANDLE"));
        assert!(source.contains("STD_OUTPUT_HANDLE"));
    }

    #[test]
    fn terminal_modes_have_one_production_owner() {
        fn rust_files(path: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rust_files(&path, files);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    files.push(path);
                }
            }
        }

        let ui_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
        let terminal_path = ui_root.join("terminal.rs");
        let mut files = Vec::new();
        rust_files(&ui_root, &mut files);

        let forbidden = [
            "enable_raw_mode",
            "disable_raw_mode",
            "EnterAlternateScreen",
            "LeaveAlternateScreen",
            "EnableMouseCapture",
            "DisableMouseCapture",
            "EnableBracketedPaste",
            "DisableBracketedPaste",
            "PushKeyboardEnhancementFlags",
            "PopKeyboardEnhancementFlags",
        ];
        for path in files {
            if path == terminal_path {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            for symbol in forbidden {
                assert!(
                    !source.contains(symbol),
                    "{} bypasses TerminalGuard with {symbol}",
                    path.display()
                );
            }
        }
    }

    #[cfg(windows)]
    #[test]
    #[allow(unsafe_code)]
    fn attached_native_windows_console_restores_original_code_pages() {
        use windows_sys::Win32::System::Console::{GetConsoleCP, GetConsoleOutputCP};

        let Some(original) = system_console_code_pages().unwrap() else {
            // Redirected `cargo test` is deliberately a no-op. Run this focused test from Windows
            // Terminal or ConHost to exercise the native console assertion.
            return;
        };
        let mut session = TerminalSession::new(SystemTerminal).unwrap();
        assert_eq!(unsafe { GetConsoleCP() }, UTF8_CODE_PAGE);
        assert_eq!(unsafe { GetConsoleOutputCP() }, UTF8_CODE_PAGE);
        session.suspend().unwrap();
        assert_eq!(unsafe { GetConsoleCP() }, original.input);
        assert_eq!(unsafe { GetConsoleOutputCP() }, original.output);
        session.resume().unwrap();
        drop(session);
        assert_eq!(unsafe { GetConsoleCP() }, original.input);
        assert_eq!(unsafe { GetConsoleOutputCP() }, original.output);
    }
}
