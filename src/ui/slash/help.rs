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

pub fn handle(_parts: &[&str], ctx: &mut SlashCtx<'_>) {
    help_lines(&mut |heading, text| {
        if heading {
            write_ok(ctx.renderer, text);
        } else {
            write_result(ctx.renderer, text);
        }
    });
}

/// Every line `/help` prints, in order; `true` marks a section heading.
pub(crate) fn help_lines(emit: &mut dyn FnMut(bool, &str)) {
    emit(true, "commands:");
    emit(false, "  /add [path]            add file(s) to context");
    emit(false, "  /drop <path>           remove file from context");
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
    emit(false, "  /provider [name]       show or switch provider");
    emit(false, "  /model [name]          show or switch model");
    emit(
        false,
        "  /agent [name]          show or switch the main-agent persona",
    );
    emit(false, "  /models                list quick models");
    emit(false, "  /models <name>         switch to a quick model");
    emit(false, "  /models-add <n> <p> <m> save a quick model");
    emit(false, "  /sessions              list recent sessions");
    emit(
        false,
        "  /sessions <id>         load a session (by ID prefix)",
    );
    emit(false, "  /sessions delete <id>  delete a session");
    #[cfg(feature = "export")]
    {
        emit(
            false,
            "  /export [file]         export session to HTML (or .jsonl)",
        );
        emit(
            false,
            "  /import <file>         import a session from JSONL or JSON",
        );
        emit(
            false,
            "  /share                 share session as a secret GitHub gist",
        );
    }
    emit(
        false,
        "  /reasoning             toggle LLM reasoning ability",
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
    emit(false, "  /toggle                show toggleable features");
    emit(false, "  /toggle todo [on|off]  toggle todo-list tools");
    #[cfg(feature = "mcp")]
    {
        emit(false, "  /mcp                   list MCP servers and tools");
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
        "  /redo                  restore the last /undo or rewind",
    );
    emit(
        false,
        "  /rewind                rewind to an earlier turn (picker)",
    );
    emit(false, "  /retry                 retry last prompt");
    emit(
        false,
        "  /queue                 list input queued while agent is busy",
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
            "  /model-subagent [name] show or switch subagent model",
        );
        emit(
            false,
            "  /models-subagent       list quick models for subagent",
        );
        emit(
            false,
            "  /models-subagent <n>   switch subagent to a quick model",
        );
    }
    emit(true, "keys:");
    emit(false, "  PgUp/PgDn             scroll chat history");
    emit(false, "  Home/End               jump to top/bottom");
    emit(
        false,
        "  /<command>             command picker (Tab insert, Enter run, Esc cancel)",
    );
    emit(
        false,
        "  @<query>               file picker (Tab/Enter insert, Esc cancel)",
    );
    emit(
        false,
        "  mouse drag             select text (copies to clipboard on release)",
    );
    emit(false, "  Esc (while selected)   clear selection (no copy)");
    #[cfg(windows)]
    emit(false, "  Ctrl+Shift+C           copy selected text");
    #[cfg(windows)]
    emit(
        false,
        "  Ctrl+V                 paste Unicode clipboard text",
    );
    emit(false, "  Ctrl+R                 toggle reasoning");
    emit(false, "  Ctrl+G                 edit input in $EDITOR");
    emit(false, "  Ctrl+H                 launch lazygit");
    emit(
        false,
        "  Ctrl+W/U/K             delete word/before/after cursor",
    );
    emit(false, "  Ctrl+A/E/B/F           move line edge/character");
    emit(false, "  Alt+B/F/D              move word/delete next word");
    emit(false, "  Ctrl+Y / Alt+Y         yank/rotate kill ring");
    emit(false, "  Ctrl+C / Ctrl+D        interrupt/quit");
    emit(false, "  mouse scroll           scroll chat");
}

#[cfg(test)]
mod tests {
    use super::help_lines;
    use crate::ui::pickers::list::available_commands;

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
    fn every_command_in_help_is_offered_by_the_slash_picker() {
        let offered = available_commands();
        let named = commands_named_in_help();
        assert!(named.contains(&"/help".to_string()));
        let missing: Vec<&String> = named
            .iter()
            .filter(|name| !offered.contains(&name.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "slash picker is missing commands that /help lists: {missing:?}"
        );
    }
}
