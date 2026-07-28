//! Minimal code-as-action agent harness on QuickJS (via rquickjs).
//!
//! The LLM writes a JavaScript program per step instead of emitting tool
//! calls. The script runs in a fresh, deny-by-default QuickJS context that
//! only sees five host functions plus `console.log`:
//!
//!   read_file(path) -> string
//!   write_file(path, content)
//!   shell(cmd) -> { code, stdout, stderr }
//!   fetch(url) -> string (response body)
//!   final_answer(answer)  // ends the agent loop
//!
//! Per-step resource limits:
//!   * memory  -- Runtime::set_memory_limit (allocation fails beyond cap)
//!   * time    -- interrupt handler checks a wall-clock deadline
//!   * stack   -- Runtime::set_max_stack_size
//!
//! Everything the script "observes" (console.log output, errors) is fed back
//! to the LLM as the next user message — that's the whole ReAct loop.

use rquickjs::{Context, Ctx, Exception, Function, Object, Runtime};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Per-step limits — tune to taste.
// ---------------------------------------------------------------------------
const STEP_MEMORY_LIMIT: usize = 64 * 1024 * 1024; // 64 MiB JS heap
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const STEP_MAX_STACK: usize = 512 * 1024; // 512 KiB interpreter stack
const MAX_STEPS: usize = 8;

/// What one executed script produced.
#[derive(Debug, Default)]
struct StepOutcome {
    logs: Vec<String>,
    final_answer: Option<String>,
    error: Option<String>,
}

/// Shared state the host functions write into while a script runs.
#[derive(Default)]
struct StepState {
    logs: Vec<String>,
    final_answer: Option<String>,
}

fn main() {
    // Demo "conversation": in a real harness, `next_script` comes from your
    // LLM provider (send system prompt + observation history, get JS back).
    let demo_scripts = [
        // Step 1: happy path — uses files, shell and log.
        r#"
            write_file("/tmp/notes.txt", "QuickJS inside a Rust agent harness");
            const contents = read_file("/tmp/notes.txt");
            console.log("file says:", contents);

            const res = shell("echo hello from the host shell");
            console.log("shell exit:", res.code, "stdout:", res.stdout.trim());
        "#,
        // Step 2: runaway loop — must be killed by the interrupt handler.
        r#"
            console.log("entering infinite loop...");
            while (true) {}
        "#,
        // Step 3: memory bomb — must be stopped by the memory limit.
        r#"
            console.log("allocating way past the cap...");
            const chunks = [];
            while (true) chunks.push(new Uint8Array(8 * 1024 * 1024));
        "#,
        // Step 4: the model decides it is done.
        r#"
            final_answer("All host functions and both resource limits verified.");
        "#,
    ];

    for (i, script) in demo_scripts.iter().enumerate().take(MAX_STEPS) {
        println!("=== step {} ===", i + 1);
        let outcome = run_step(script);

        // This is the "observation" you would feed back to the LLM.
        for line in &outcome.logs {
            println!("  [log] {line}");
        }
        if let Some(err) = &outcome.error {
            println!("  [error] {err}");
        }
        if let Some(answer) = &outcome.final_answer {
            println!("  [final answer] {answer}");
            break;
        }
    }
}

/// Execute one model-written script in a fresh, limited QuickJS context.
fn run_step(source: &str) -> StepOutcome {
    let state = Rc::new(RefCell::new(StepState::default()));

    // Fresh runtime per step: no state leaks between steps, and a poisoned
    // heap (e.g. after OOM) is simply thrown away.
    let rt = Runtime::new().expect("failed to create QuickJS runtime");
    rt.set_memory_limit(STEP_MEMORY_LIMIT);
    rt.set_max_stack_size(STEP_MAX_STACK);

    // Wall-clock deadline enforced via the interrupt handler, which QuickJS
    // polls regularly during execution. Returning `true` aborts the script.
    let deadline = Instant::now() + STEP_TIMEOUT;
    rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));

    let ctx = Context::full(&rt).expect("failed to create context");

    let mut outcome = StepOutcome::default();
    let eval_result: Result<(), String> = ctx.with(|ctx| {
        bind_host_functions(&ctx, state.clone()).map_err(|e| format!("bind error: {e}"))?;

        match ctx.eval::<(), _>(source) {
            Ok(()) => Ok(()),
            Err(rquickjs::Error::Exception) => {
                // Pull the actual JS exception object for a useful message.
                let exc = ctx.catch();
                let msg = exc
                    .as_exception()
                    .and_then(|e| e.message())
                    .unwrap_or_else(|| format!("{exc:?}"));
                Err(msg)
            }
            Err(e) => Err(e.to_string()),
        }
    });

    let state = state.borrow();
    outcome.logs = state.logs.clone();
    outcome.final_answer = state.final_answer.clone();
    outcome.error = eval_result.err();
    outcome
}

/// shell(cmd) — a plain `fn` (not a closure) because returning a JS value
/// (`Object<'js>`) requires the higher-ranked lifetime bound that only fn
/// items get for free.
fn js_shell<'js>(ctx: Ctx<'js>, cmd: String) -> rquickjs::Result<Object<'js>> {
    #[cfg(windows)]
    let output = std::process::Command::new("cmd")
        .args(["/C", &cmd])
        .output();
    #[cfg(not(windows))]
    let output = std::process::Command::new("sh").args(["-c", &cmd]).output();

    let output =
        output.map_err(|e| Exception::throw_message(&ctx, &format!("shell({cmd}): {e}")))?;

    let obj = Object::new(ctx.clone())?;
    obj.set("code", output.status.code().unwrap_or(-1))?;
    obj.set(
        "stdout",
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )?;
    obj.set(
        "stderr",
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )?;
    Ok(obj)
}

/// Expose exactly the capabilities the agent is allowed to have.
/// Everything else (filesystem, network, process) is unreachable from JS.
fn bind_host_functions(ctx: &Ctx<'_>, state: Rc<RefCell<StepState>>) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    // -- read_file(path) -> string ------------------------------------------
    globals.set(
        "read_file",
        Function::new(ctx.clone(), {
            move |ctx: Ctx<'_>, path: String| -> rquickjs::Result<String> {
                std::fs::read_to_string(&path)
                    .map_err(|e| Exception::throw_message(&ctx, &format!("read_file({path}): {e}")))
            }
        })?,
    )?;

    // -- write_file(path, content) ------------------------------------------
    globals.set(
        "write_file",
        Function::new(ctx.clone(), {
            move |ctx: Ctx<'_>, path: String, content: String| -> rquickjs::Result<()> {
                std::fs::write(&path, content).map_err(|e| {
                    Exception::throw_message(&ctx, &format!("write_file({path}): {e}"))
                })
            }
        })?,
    )?;

    // -- shell(cmd) -> { code, stdout, stderr } ------------------------------
    // NOTE: this is the one capability that escapes the interpreter sandbox.
    // Gate it behind your permission system, and on Windows wrap the spawned
    // process in an AppContainer / Job Object (see harness docs).
    globals.set("shell", Function::new(ctx.clone(), js_shell)?)?;

    // -- fetch(url) -> string -------------------------------------------------
    globals.set(
        "fetch",
        Function::new(ctx.clone(), {
            move |ctx: Ctx<'_>, url: String| -> rquickjs::Result<String> {
                ureq::get(&url)
                    .call()
                    .map_err(|e| Exception::throw_message(&ctx, &format!("fetch({url}): {e}")))?
                    .body_mut()
                    .read_to_string()
                    .map_err(|e| Exception::throw_message(&ctx, &format!("fetch({url}): {e}")))
            }
        })?,
    )?;

    // -- final_answer(answer) --------------------------------------------------
    globals.set(
        "final_answer",
        Function::new(ctx.clone(), {
            let state = state.clone();
            move |answer: String| {
                state.borrow_mut().final_answer = Some(answer);
            }
        })?,
    )?;

    // -- console.log(...) — the observation channel back to the LLM -----------
    let console = Object::new(ctx.clone())?;
    console.set(
        "log",
        Function::new(ctx.clone(), {
            let state = state.clone();
            move |args: rquickjs::function::Rest<rquickjs::convert::Coerced<String>>| {
                let line = args
                    .iter()
                    .map(|c| c.0.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                state.borrow_mut().logs.push(line);
            }
        })?,
    )?;
    globals.set("console", console)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_returns_body() {
        let o = run_step(r#"console.log(fetch("https://example.com").length);"#);
        assert!(o.error.is_none(), "fetch failed: {:?}", o.error);
        assert!(o.logs[0].parse::<usize>().unwrap() > 100);
    }

    #[test]
    fn syntax_error_is_reported_not_fatal() {
        let o = run_step("this is not javascript");
        assert!(o.error.is_some());
    }

    #[test]
    fn state_does_not_leak_between_steps() {
        run_step("globalThis.leak = 42;");
        let o = run_step(r#"console.log(typeof globalThis.leak);"#);
        assert_eq!(o.logs[0], "undefined");
    }
}
