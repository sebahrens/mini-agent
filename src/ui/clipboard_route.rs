//! Where a copy goes. The local helpers (`wl-copy`, `xclip`, `pbcopy`) write
//! the clipboard of the machine mini-agent runs on, which over SSH is not the
//! machine the user sits at, and inside tmux a bare OSC 52 never reaches the
//! outer terminal. The route is chosen from the environment before any helper
//! is spawned. Also holds the Unix text read behind Ctrl+V.

use std::sync::atomic::{AtomicU8, Ordering};

/// Base64 length above which an OSC 52 request is refused: many terminals and
/// tmux drop longer ones silently, so the user is told instead of being
/// shown a success that did not happen (about 74 KB, 55 KB of text).
pub(crate) const OSC52_MAX_BASE64: usize = 74_000;

/// One way of putting text on a clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClipboardMethod {
    /// `wl-copy`, `xclip` or `pbcopy` on this machine.
    Helper,
    /// `tmux load-buffer -w -`: the tmux buffer, and the outer clipboard when
    /// tmux's `set-clipboard` allows it.
    Tmux,
    /// An OSC 52 escape written to the terminal; `passthrough` wraps it in a
    /// tmux DCS passthrough envelope.
    Osc52 { passthrough: bool },
}

/// The `clipboard` override: `MINI_AGENT_CLIPBOARD=auto|osc52|helper|tmux`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ClipboardMode {
    #[default]
    Auto,
    Osc52,
    Helper,
    Tmux,
}

impl ClipboardMode {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "osc52" => Self::Osc52,
            "helper" => Self::Helper,
            "tmux" => Self::Tmux,
            _ => Self::Auto,
        }
    }
}

/// What the route depends on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClipboardEnv {
    pub(crate) ssh: bool,
    pub(crate) tmux: bool,
    pub(crate) mode: ClipboardMode,
}

impl ClipboardEnv {
    pub(crate) fn current() -> Self {
        let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        Self {
            ssh: set("SSH_TTY") || set("SSH_CONNECTION"),
            tmux: set("TMUX"),
            mode: std::env::var("MINI_AGENT_CLIPBOARD")
                .map(|value| ClipboardMode::parse(&value))
                .unwrap_or_default(),
        }
    }
}

/// The methods to try, in order, for `env`.
pub(crate) fn clipboard_route(env: ClipboardEnv) -> Vec<ClipboardMethod> {
    use ClipboardMethod::{Helper, Osc52, Tmux};
    let osc52 = Osc52 {
        passthrough: env.tmux,
    };
    match env.mode {
        ClipboardMode::Osc52 => vec![osc52],
        ClipboardMode::Helper => vec![Helper],
        ClipboardMode::Tmux => vec![Tmux],
        ClipboardMode::Auto => match (env.ssh, env.tmux) {
            // Remote: the helpers would fill the remote machine's clipboard.
            (true, false) => vec![osc52],
            (true, true) => vec![Tmux, osc52],
            (false, true) => vec![Helper, Tmux, osc52],
            (false, false) => vec![Helper, osc52],
        },
    }
}

/// The OSC 52 request for `text`, wrapped for tmux when `passthrough`.
pub(crate) fn osc52_payload(text: &str, passthrough: bool) -> String {
    let request = super::renderer::osc52_request(text);
    if passthrough {
        // tmux forwards the DCS body with each ESC doubled (it needs
        // `allow-passthrough on`).
        format!("\x1bPtmux;{}\x1b\\", request.replace('\x1b', "\x1b\x1b"))
    } else {
        request
    }
}

/// Whether `text` fits one OSC 52 request.
pub(crate) fn osc52_fits(text: &str) -> bool {
    text.len().div_ceil(3) * 4 <= OSC52_MAX_BASE64
}

static LAST_ROUTE: AtomicU8 = AtomicU8::new(0);

fn remember(method: ClipboardMethod) {
    LAST_ROUTE.store(
        match method {
            ClipboardMethod::Helper => 1,
            ClipboardMethod::Tmux => 2,
            ClipboardMethod::Osc52 { .. } => 3,
        },
        Ordering::Relaxed,
    );
}

/// How the last successful copy travelled, for the status line.
pub(crate) fn last_route_label() -> &'static str {
    match LAST_ROUTE.load(Ordering::Relaxed) {
        1 => "clipboard",
        2 => "tmux",
        3 => "your terminal (OSC 52)",
        _ => "clipboard",
    }
}

#[cfg(not(windows))]
pub(crate) async fn copy_unix(text: &str) -> anyhow::Result<super::renderer::ClipboardCopyOutcome> {
    use super::renderer::{ClipboardCopyOutcome, run_clipboard_command};
    let methods = clipboard_route(ClipboardEnv::current());
    let mut last_error = None;
    for method in methods {
        match method {
            ClipboardMethod::Helper => {
                let helpers: &[(&str, &[&str])] = &[
                    ("wl-copy", &[]),
                    ("xclip", &["-selection", "clipboard"]),
                    ("pbcopy", &[]),
                ];
                for &(program, args) in helpers {
                    let mut command = tokio::process::Command::new(program);
                    command.args(args);
                    if run_clipboard_command(command, text, std::time::Duration::from_secs(2))
                        .await
                    {
                        remember(method);
                        return Ok(ClipboardCopyOutcome::Confirmed);
                    }
                }
                last_error = Some(anyhow::anyhow!(
                    "no clipboard helper (wl-copy, xclip, pbcopy) succeeded"
                ));
            }
            ClipboardMethod::Tmux => {
                let mut command = tokio::process::Command::new("tmux");
                command.args(["load-buffer", "-w", "-"]);
                if run_clipboard_command(command, text, std::time::Duration::from_secs(2)).await {
                    remember(method);
                    return Ok(ClipboardCopyOutcome::Confirmed);
                }
                last_error = Some(anyhow::anyhow!("tmux load-buffer failed"));
            }
            ClipboardMethod::Osc52 { passthrough } => {
                if !osc52_fits(text) {
                    last_error = Some(anyhow::anyhow!(
                        "{} KB is too large for one OSC 52 request (about {} KB); copy less, or \
                         set MINI_AGENT_CLIPBOARD=helper",
                        text.len().div_ceil(1024),
                        OSC52_MAX_BASE64 / 4 * 3 / 1024
                    ));
                    continue;
                }
                use std::io::Write;
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(osc52_payload(text, passthrough).as_bytes())?;
                stdout.flush()?;
                remember(method);
                return Ok(ClipboardCopyOutcome::FallbackRequested);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no clipboard route is available")))
}

/// The clipboard's text through `pbpaste`, `wl-paste` or `xclip`, bounded in
/// size and time. An empty clipboard yields an empty string.
#[cfg(not(windows))]
pub(crate) async fn read_unix_text() -> anyhow::Result<String> {
    use crate::process_creation::TokioCommandCreationExt;
    use tokio::io::AsyncReadExt;

    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let readers: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbpaste", &[])]
    } else if wayland {
        &[
            ("wl-paste", &["--no-newline"]),
            ("xclip", &["-selection", "clipboard", "-o"]),
        ]
    } else {
        &[
            ("xclip", &["-selection", "clipboard", "-o"]),
            ("wl-paste", &["--no-newline"]),
        ]
    };
    for &(program, args) in readers {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        crate::sandbox::configure_child_lifetime(&mut command);
        let Ok(mut child) = command.spawn_guarded() else {
            continue;
        };
        let Some(stdout) = child.stdout.take() else {
            continue;
        };
        let limit = super::renderer::MAX_CLIPBOARD_BYTES as u64;
        let read = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut bytes = Vec::new();
            stdout.take(limit + 1).read_to_end(&mut bytes).await?;
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((bytes, status))
        })
        .await;
        match read {
            Ok(Ok((bytes, status))) if status.success() => {
                if bytes.len() as u64 > limit {
                    anyhow::bail!("clipboard text is too large");
                }
                let text = String::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!("clipboard text is not valid UTF-8"))?;
                super::renderer::validate_clipboard_text(&text)?;
                return Ok(text);
            }
            // An empty or non-text clipboard makes the reader exit non-zero.
            Ok(Ok(_)) => return Ok(String::new()),
            _ => {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
    }
    anyhow::bail!("no clipboard reader available (install pbpaste, wl-paste or xclip)")
}

#[cfg(test)]
mod tests {
    use super::ClipboardMethod::{Helper, Osc52, Tmux};
    use super::*;

    fn env(ssh: bool, tmux: bool, mode: ClipboardMode) -> ClipboardEnv {
        ClipboardEnv { ssh, tmux, mode }
    }

    #[test]
    fn routes_follow_ssh_and_tmux() {
        let auto = ClipboardMode::Auto;
        assert_eq!(
            clipboard_route(env(false, false, auto)),
            [
                Helper,
                Osc52 {
                    passthrough: false
                }
            ]
        );
        assert_eq!(
            clipboard_route(env(true, false, auto)),
            [Osc52 {
                passthrough: false
            }]
        );
        assert_eq!(
            clipboard_route(env(false, true, auto)),
            [Helper, Tmux, Osc52 { passthrough: true }]
        );
        assert_eq!(
            clipboard_route(env(true, true, auto)),
            [Tmux, Osc52 { passthrough: true }]
        );
    }

    #[test]
    fn the_override_replaces_the_automatic_route() {
        assert_eq!(
            clipboard_route(env(true, true, ClipboardMode::Helper)),
            [Helper]
        );
        assert_eq!(
            clipboard_route(env(false, false, ClipboardMode::Osc52)),
            [Osc52 {
                passthrough: false
            }]
        );
        assert_eq!(
            clipboard_route(env(false, false, ClipboardMode::Tmux)),
            [Tmux]
        );
        assert_eq!(ClipboardMode::parse(" OSC52 "), ClipboardMode::Osc52);
        assert_eq!(ClipboardMode::parse("bogus"), ClipboardMode::Auto);
    }

    #[test]
    fn tmux_passthrough_doubles_escapes_and_oversized_payloads_are_refused() {
        let plain = osc52_payload("hi", false);
        assert!(plain.starts_with("\x1b]52;c;") && plain.ends_with('\x07'));
        let wrapped = osc52_payload("hi", true);
        assert!(wrapped.starts_with("\x1bPtmux;\x1b\x1b]52;c;"));
        assert!(wrapped.ends_with("\x07\x1b\\"));
        assert!(osc52_fits(&"x".repeat(50_000)));
        assert!(!osc52_fits(&"x".repeat(60_000)));
    }
}
