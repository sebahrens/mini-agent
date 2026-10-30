use crate::ui::slash::{SlashCtx, write_ok, write_result};
use crate::ui::terminal::TerminalGuard;

pub fn handle_welcome(renderer: &mut crate::ui::renderer::Renderer) {
    let _ = crate::ui::events::show_welcome(renderer);
}

pub fn handle_tutor(
    renderer: &mut crate::ui::renderer::Renderer,
    terminal_guard: &mut TerminalGuard,
) -> anyhow::Result<()> {
    match run_tutor(terminal_guard) {
        Ok(()) => Ok(()),
        Err(error)
            if error
                .downcast_ref::<crate::ui::terminal::TerminalLifecycleError>()
                .is_some() =>
        {
            Err(error)
        }
        Err(e) => {
            renderer.write_line(&format!("{}", e), crate::ui::slash::C_ERROR)?;
            Ok(())
        }
    }
}

/// Hands the tty to `less` (or prints the document when no pager exists).
/// The app stops the crossterm event thread before dispatching `/tutor` and
/// rebinds it afterwards (see `App::run_slash_command`), so the pager is the
/// only stdin reader while the terminal is suspended.
fn run_tutor(terminal_guard: &mut TerminalGuard) -> anyhow::Result<()> {
    terminal_guard.suspend()?;
    let result = crate::docs::show_get_started();
    terminal_guard.resume()?;
    result
}

pub fn handle(parts: &[&str], ctx: &mut SlashCtx<'_>) {
    if parts.get(1) == Some(&"keys") {
        keys_lines(&mut |heading, text| {
            if heading {
                write_ok(ctx.renderer, text);
            } else {
                write_result(ctx.renderer, text);
            }
        });
        return;
    }
    help_lines(&mut |heading, text| {
        if heading {
            write_ok(ctx.renderer, text);
        } else {
            write_result(ctx.renderer, text);
        }
    });
    // A live view never reloads templates while its runner owns context.
    if *ctx.is_running {
        return;
    }
    // The stamped cache dispatch already uses: a few stats when nothing changed.
    let commands = crate::context::commands::described_cached(ctx.custom_commands, ctx.workspace);
    if !commands.is_empty() {
        write_ok(ctx.renderer, "custom prompt commands:");
        for (name, description) in commands {
            write_result(
                ctx.renderer,
                format!("  /{name} [arguments]  {description}"),
            );
        }
    }
}

/// Every line `/help` prints, in order; `true` marks a section heading.
pub(crate) fn help_lines(emit: &mut dyn FnMut(bool, &str)) {
    emit(true, "commands:");
    emit(
        false,
        "  /config [--global|--project|--local] [key [TOML value]]  edit known settings",
    );
    emit(
        false,
        "  /keys                 list effective configured keyboard bindings",
    );
    emit(
        false,
        "  /top, /bottom         scroll the transcript to its start or end (no keys needed)",
    );
    emit(
        false,
        "  /vim [on|off]         toggle input Vim editing for this UI session",
    );
    emit(
        false,
        "  /cost                 session by model and UTC daily/all-time usage estimates",
    );
    #[cfg(feature = "agent-skills")]
    emit(
        false,
        "  /skills               list/pick authorized instruction skills for one turn",
    );
    emit(
        false,
        "  /add [path...]         add file(s) to context (Tab completes paths)",
    );
    #[cfg(feature = "subagents")]
    emit(
        false,
        "  /tasks                list subagent tasks; <id> tool log, result <id>, cancel <id>",
    );
    emit(
        false,
        "  /jobs                 list this session's background jobs",
    );
    emit(
        false,
        "  /jobs stop <job-id>    request an owned job's cancellation",
    );
    emit(
        false,
        "  /drop <path...>        remove file(s) from context",
    );
    emit(
        false,
        "  /drop-all              remove all added files from context",
    );
    emit(
        false,
        "  /init [force]          create AGENTS.md for this project",
    );
    emit(
        false,
        "  /memory [status|search|read|write|editor|clear]  manage memory",
    );
    emit(false, "  /clear [/new]          clear the current session");
    emit(
        false,
        "  /context               show context composition and pruning projection",
    );
    emit(false, "  /provider [name]       show or switch provider");
    emit(false, "  /login openai [new|number]  continue with ChatGPT");
    emit(
        false,
        "  /login copilot        authorize your GitHub OAuth app (device flow)",
    );
    emit(
        false,
        "  /logout copilot       clear the saved Copilot login for this host",
    );
    emit(
        false,
        "  /logout openai         sign out of the selected ChatGPT account",
    );
    emit(
        false,
        "  /doctor                show bounded configuration and runtime diagnostics",
    );
    emit(
        false,
        "  /model [alias|id]      show or switch model (quick alias, else raw id)",
    );
    emit(
        false,
        "  /agent [name]          show or switch the main-agent persona",
    );
    emit(
        false,
        "  /models [refresh]      list quick and provider models (or refresh)",
    );
    emit(false, "  /models <name>         same as /model <name>");
    emit(false, "  /models-add <n> <p> <m> save a quick model");
    emit(false, "  /sessions              list recent sessions");
    emit(
        false,
        "  /fork [n]              branch here or at user turn n",
    );
    emit(
        false,
        "  /sessions <id>         load a session (by ID prefix)",
    );
    emit(false, "  /sessions delete <id>  delete a session");
    #[cfg(feature = "export")]
    {
        emit(
            false,
            "  /export [file]         export session to HTML, .md or .jsonl",
        );
        emit(
            false,
            "  /import <file>         import a session from JSONL or JSON",
        );
        emit(
            false,
            "  /share                 share session as a secret GitHub gist (asks to confirm)",
        );
    }
    emit(
        false,
        "  /effort [level|default] session effort; low/medium/high/xhigh/max",
    );
    emit(
        false,
        "  /reasoning [on|off]    set or toggle supported model thinking",
    );
    emit(false, "  /thinking              alias for /reasoning");
    emit(
        false,
        "  /mode                  pick a security mode (described list)",
    );
    emit(
        false,
        "  /mode <mode>           set mode (standard|restrictive|readonly|planwrite|guarded|yolo)",
    );
    emit(false, "  /permissions          choose approval mode");
    emit(false, "  /permissions <mode>   set mode (yolo|auto|manual)");
    emit(
        false,
        "  /sandbox [profile]     inspect or select workspace|host-read|host-full",
    );
    emit(
        false,
        "  /expand [n|all]        unfold the latest (or nth latest, or every) tool result",
    );
    emit(
        false,
        "  /collapse [n|all]      fold tool results back to their one-line summary",
    );
    emit(false, "  /toggle                show toggleable features");
    emit(false, "  /toggle todo [on|off]  toggle todo-list tools");
    #[cfg(feature = "mcp")]
    {
        emit(
            false,
            "  /mcp                   show origins, retained peers and observed attempts",
        );
        emit(
            false,
            "  /mcp add <name> <command|url> [args...] [--project] [--allow-local]",
        );
        emit(
            false,
            "  /mcp remove <name> [--project]   remove the selected owning definition",
        );
        emit(
            false,
            "  /mcp <server>          list tools of an MCP server",
        );
        emit(
            false,
            "  /mcp login <server>    OAuth login to an MCP server",
        );
        emit(
            false,
            "  /mcp logout <server>   remove a server's stored OAuth token",
        );
    }
    emit(
        false,
        "  /undo [stash]          undo last exchange (stash only when asked; never prompts)",
    );
    emit(
        false,
        "  /redo                  restore conversation from /undo or rewind",
    );
    emit(
        false,
        "  /rewind                conversation/files/both; also double Esc",
    );
    emit(false, "  /retry                 retry last prompt");
    emit(
        false,
        "  /queue                 list input queued while agent is busy",
    );
    emit(
        false,
        "  /steer <message>       redirect the active turn at its next tool boundary",
    );
    emit(false, "  /queue clear           clear the queue");
    emit(
        false,
        "  /queue pop             remove the last queued input",
    );
    emit(
        false,
        "  /btw <message>         ask a side question in parallel (no session trace)",
    );
    emit(
        false,
        "  /review [msg]          review code (auto message if omitted)",
    );
    for (command, description) in [
        ("/diff [path|turn]", "show a contained Git diff"),
        (
            "/commit [message]",
            "preview latest-turn candidate files; confirm Y/N/B",
        ),
        ("/pr", "preview a tool-free PR proposal; confirm Y/N/B"),
    ] {
        let suffix = if cfg!(feature = "git-worktree") {
            ""
        } else {
            " (req. 'git-worktree' feature)"
        };
        emit(false, &format!("  {command:<22}{description}{suffix}"));
    }
    emit(
        false,
        "  /compress [/compact]   compress conversation history",
    );
    emit(
        false,
        "  /compress [instr]      compress with custom instructions",
    );
    emit(
        false,
        "  /editsys [mode]        edit system (similarity | hashedit)",
    );
    #[cfg(feature = "advisor")]
    {
        emit(false, "  /advisor               show advisor status");
        emit(false, "  /advisor on|off        enable or disable advisor");
        emit(
            false,
            "  /advisor handoff [on|off]  toggle human handoff mode",
        );
        emit(false, "  /advisor model <name>  set advisor model");
        emit(
            false,
            "  /advisor max-uses <n>  set max advisor calls per request",
        );
        emit(
            false,
            "  /advisor context-limit <n>  set max KB sent to advisor",
        );
    }
    emit(
        false,
        "  /cron <every> <prompt> repeat a prompt every 5m/2h while idle (min 1m, max 24h)",
    );
    emit(
        false,
        "  /cron <prompt>         self-paced: the model picks each next fire (1m..1h)",
    );
    emit(
        false,
        "  /cron [list]           list schedules: id, interval, next fire, fires",
    );
    emit(
        false,
        "  /cron stop <id>|all    remove one or every schedule",
    );
    emit(
        false,
        "  /cron run <id>         fire a schedule at the next idle point",
    );
    emit(
        false,
        "  /cron resume           re-arm the schedules saved with a resumed session",
    );
    #[cfg(feature = "loop")]
    {
        emit(
            false,
            "  /loop [prompt]         start iterative coding loop",
        );
        emit(false, "  /loop stop             stop the loop");
        emit(false, "  /loop status           show loop status");
    }
    #[cfg(not(feature = "loop"))]
    {
        emit(
            false,
            "  /loop [prompt]         start iterative coding loop (req. 'loop' feature)",
        );
    }
    #[cfg(feature = "goal")]
    {
        emit(
            false,
            "  /goal <objective>      set a goal the agent works toward across turns",
        );
        emit(
            false,
            "  /goal status           objective, round, verification and last check",
        );
        emit(
            false,
            "  /goal check <cmd>      require a command to pass before the goal is met",
        );
        emit(
            false,
            "  /goal bounds k=v       change max_rounds, max_tokens, continuation, …",
        );
        emit(false, "  /goal pause|resume|reopen|clear");
    }
    #[cfg(not(feature = "goal"))]
    {
        emit(
            false,
            "  /goal <objective>      persistent objective (req. 'goal' feature)",
        );
    }
    emit(false, "  /prompt                list available prompts");
    emit(false, "  /prompt <name>         activate a prompt");
    emit(false, "  /prompt default        clear active prompt");
    emit(false, "  /rename <name>         rename current session");
    emit(false, "  /theme                 list available themes");
    emit(false, "  /theme <name>          activate a theme");
    emit(false, "  /theme default         clear active theme");
    emit(
        false,
        "  /regen-prompts        restore built-in prompts to global dir",
    );
    emit(
        false,
        "  /regen-themes         restore built-in themes to config dir",
    );
    #[cfg(feature = "git-worktree")]
    {
        emit(
            false,
            "  /worktree <name>       create a git worktree on <name> branch and cd into it",
        );
        emit(
            false,
            "  /wt-merge [branch]     merge worktree branch into [branch] (default: main/master)",
        );
        emit(
            false,
            "  /wt-exit               exit worktree and return to main repo",
        );
    }
    #[cfg(feature = "hooks")]
    emit(
        false,
        "  /hooks                 show configured hook events and handlers",
    );
    emit(false, "  /history               show global chat history");
    emit(false, "  /quit [/exit]          exit mini-agent");
    emit(false, "  /welcome               show the quickstart guide");
    emit(
        false,
        "  /tutor                 open GET_STARTED.md in less",
    );
    emit(false, "  /tutorial              alias for /welcome");
    emit(false, "  /help                  show this message");
    emit(false, "");
    #[cfg(feature = "subagents")]
    {
        emit(
            false,
            "  /subagent-model [name] show or switch subagent model",
        );
        emit(
            false,
            "  /subagent-models       list quick models for subagent",
        );
        emit(
            false,
            "  /subagent-models <n>   switch subagent to a quick model",
        );
    }
    keys_lines(emit);
}

fn keys_lines(emit: &mut dyn FnMut(bool, &str)) {
    emit(true, "keys:");
    emit(false, "  ? (empty input)        show these shortcuts");
    emit(
        false,
        "  PgUp/PgDn, Alt+Up/Down scroll chat history by a page (Shift/Ctrl+wheel too)",
    );
    emit(
        false,
        "  Home/End               line start/end (empty input: top/bottom)",
    );
    emit(
        false,
        "  Ctrl+Home/Ctrl+End     jump to top/bottom of chat (also Alt+Shift+Up/Down, Alt+</Alt+>)",
    );
    emit(
        false,
        "  /<command>             command picker (Tab insert, Enter run, Esc cancel)",
    );
    emit(
        false,
        "  @<query>               file picker (Tab/Enter insert, Esc cancel)",
    );
    #[cfg(feature = "subagents")]
    emit(
        false,
        "  @agent:<name>          route the request to a specialist via task",
    );
    emit(
        false,
        "  mouse drag             select text (copies to clipboard on release)",
    );
    emit(
        false,
        "  mouse double-click     select and copy the word under the pointer",
    );
    emit(false, "  Esc (while selected)   clear selection (no copy)");
    #[cfg(windows)]
    emit(false, "  Ctrl+Shift+C           copy selected text");
    #[cfg(windows)]
    emit(
        false,
        "  Ctrl+V                 paste Unicode clipboard text",
    );
    emit(
        false,
        "  Ctrl+R                 cycle reasoning: collapsed/full/off",
    );
    emit(
        false,
        "  Ctrl+T                 fold/unfold the latest tool result (or click its summary)",
    );
    emit(false, "  Ctrl+G                 edit input in $EDITOR");
    emit(false, "  Ctrl+O                 launch lazygit");
    emit(
        false,
        "  Ctrl+W/U/K             delete word/before/after cursor",
    );
    emit(false, "  Ctrl+A/E/B/F           move line edge/character");
    emit(
        false,
        "  Ctrl+P/Ctrl+N          move up/down; history at input boundaries",
    );
    emit(false, "  Alt+B/F/D              move word/delete next word");
    emit(
        false,
        "  Alt/Ctrl+Left/Right    move word; Alt+Backspace deletes word",
    );
    emit(false, "  Ctrl+Y / Alt+Y         yank/rotate kill ring");
    emit(
        false,
        "  Ctrl+C                 interrupt/clear; press again consecutively to exit",
    );
    emit(
        false,
        "  Ctrl+D                 interrupt; idle: delete, or exit when empty",
    );
    emit(false, "  mouse scroll           scroll chat");
}

#[cfg(test)]
mod tests {
    use super::help_lines;
    use crate::ui::pickers::list::{COMMAND_ALIASES, available_commands, is_command_alias};

    /// Command names `/help` mentions, skipping lines that describe a feature
    /// this build does not include.
    fn commands_named_in_help() -> Vec<String> {
        let mut names = Vec::new();
        help_lines(&mut |_, line| {
            if line.contains("(req.") {
                return;
            }
            let chars: Vec<char> = line.chars().collect();
            for (index, &c) in chars.iter().enumerate() {
                let starts_word = index == 0 || matches!(chars[index - 1], ' ' | '[');
                let names_command = chars
                    .get(index + 1)
                    .is_some_and(|next| next.is_ascii_lowercase());
                if c == '/' && starts_word && names_command {
                    let name: String = std::iter::once('/')
                        .chain(chars[index + 1..].iter().copied().take_while(|next| {
                            next.is_ascii_lowercase() || next.is_ascii_digit() || *next == '-'
                        }))
                        .collect();
                    names.push(name);
                }
            }
        });
        names
    }

    #[test]
    fn mcp_live_admin_help_matches_compiled_feature() {
        let mut lines = Vec::new();
        help_lines(&mut |_, line| lines.push(line.to_owned()));
        let text = lines.join("\n");
        for command in ["/mcp add <name>", "/mcp remove <name>", "--allow-local"] {
            assert_eq!(text.contains(command), cfg!(feature = "mcp"), "{command}");
        }
    }
    #[test]
    fn every_command_in_help_is_offered_by_the_slash_picker() {
        let offered = available_commands();
        let named = commands_named_in_help();
        assert!(named.contains(&"/help".to_string()));
        let missing: Vec<&String> = named
            .iter()
            .filter(|name| !offered.contains(&name.as_str()) && !is_command_alias(name))
            .collect();
        assert!(
            missing.is_empty(),
            "slash picker is missing commands that /help lists: {missing:?}"
        );
    }

    /// mini-agent-5bvrb: the lazygit key stays discoverable from /help.
    #[test]
    fn help_names_the_lazygit_key() {
        let mut lines = Vec::new();
        help_lines(&mut |_, line| lines.push(line.to_string()));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Ctrl+O") && line.contains("lazygit")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("Ctrl+P")
                && line.contains("Ctrl+N")
                && line.contains("history")),
            "{lines:?}"
        );
    }

    /// mini-agent-ay2rm: tool-output folding stays discoverable from /help.
    #[test]
    fn help_names_the_tool_output_fold_key_and_commands() {
        let mut lines = Vec::new();
        help_lines(&mut |_, line| lines.push(line.to_string()));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Ctrl+T") && line.contains("tool result")),
            "{lines:?}"
        );
        for command in ["/expand", "/collapse"] {
            assert!(
                lines.iter().any(|line| line.contains(command)),
                "{command} missing: {lines:?}"
            );
        }
    }

    /// mini-agent-lqq0c: aliases are hidden from completion, but each one
    /// names an offered canonical command and `/help` still mentions it.
    #[test]
    fn aliases_are_hidden_from_completion_and_documented_in_help() {
        let offered = available_commands();
        let named = commands_named_in_help();
        for (alias, canonical) in COMMAND_ALIASES {
            assert!(!offered.contains(alias), "{alias} must not be offered");
            if canonical.starts_with("/subagent") && !cfg!(feature = "subagents") {
                continue;
            }
            assert!(offered.contains(canonical), "{canonical} must be offered");
            if !alias.contains("subagent") {
                assert!(
                    named.iter().any(|n| n == alias),
                    "/help must mention {alias}"
                );
            }
        }
    }
    #[test]
    fn git_commands_are_feature_gated_in_completion_but_always_reserved() {
        let offered = available_commands();
        let mut lines = Vec::new();
        help_lines(&mut |_, line| lines.push(line.to_owned()));
        for name in ["/commit", "/diff", "/pr"] {
            assert_eq!(offered.contains(&name), cfg!(feature = "git-worktree"));
            assert!(crate::ui::pickers::list::reserved_command(&name[1..]));
            let line = lines
                .iter()
                .find(|line| line.trim_start().starts_with(&format!("{name} ")))
                .unwrap();
            assert_eq!(
                line.contains("(req. 'git-worktree' feature)"),
                !cfg!(feature = "git-worktree")
            );
        }
    }
}
