use std::future::Future;
use std::process::Stdio;
use std::time::Duration;

use rquickjs::{Context, Ctx, IntoJs, Object, Value, prelude::Func};
use tokio::time::timeout;

use crate::agent::tools::{ToolError, check_perm, check_perm_path};
use crate::extras::js::types::{STEP_TIMEOUT, SpawnResult};
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::sandbox::{Sandbox, kill_process_group};

impl<'js> IntoJs<'js> for SpawnResult {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("stdout", self.stdout)?;
        obj.set("stderr", self.stderr)?;
        obj.set("code", self.code)?;
        Ok(obj.into())
    }
}

fn permission_error(tool: &'static str, error: ToolError) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("permission check", tool, error.to_string())
}

fn timeout_error(tool: &'static str) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("host call", tool, "execution timed out")
}

async fn timeout_host_call<T>(
    tool: &'static str,
    duration: Duration,
    call: impl Future<Output = rquickjs::Result<T>>,
) -> rquickjs::Result<T> {
    timeout(duration, call)
        .await
        .map_err(|_| timeout_error(tool))?
}

fn block_on_host_call<T>(
    runtime: &tokio::runtime::Handle,
    tool: &'static str,
    duration: Duration,
    call: impl Future<Output = rquickjs::Result<T>>,
) -> rquickjs::Result<T> {
    runtime.block_on(timeout_host_call(tool, duration, call))
}

pub fn make_read_file(
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String) -> rquickjs::Result<String> {
    move |path: String| {
        runtime
            .block_on(check_perm_path(&permission, &ask_tx, "js/read_file", &path))
            .map_err(|error| permission_error("js/read_file", error))?;
        block_on_host_call(&runtime, "js/read_file", STEP_TIMEOUT, async move {
            tokio::fs::read_to_string(path)
                .await
                .map_err(rquickjs::Error::Io)
        })
    }
}

pub fn make_write_file(
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String, String) -> rquickjs::Result<()> {
    move |path: String, content: String| {
        runtime
            .block_on(check_perm_path(
                &permission,
                &ask_tx,
                "js/write_file",
                &path,
            ))
            .map_err(|error| permission_error("js/write_file", error))?;
        block_on_host_call(&runtime, "js/write_file", STEP_TIMEOUT, async move {
            tokio::fs::write(path, content)
                .await
                .map_err(rquickjs::Error::Io)
        })
    }
}

pub fn make_spawn(
    sandbox: Sandbox,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String, Vec<String>) -> rquickjs::Result<SpawnResult> {
    make_spawn_with_timeout(sandbox, permission, ask_tx, runtime, STEP_TIMEOUT)
}

fn make_spawn_with_timeout(
    sandbox: Sandbox,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    runtime: tokio::runtime::Handle,
    duration: Duration,
) -> impl Fn(String, Vec<String>) -> rquickjs::Result<SpawnResult> {
    move |cmd: String, args: Vec<String>| {
        runtime
            .block_on(check_perm(&permission, &ask_tx, "js/spawn", &cmd))
            .map_err(|error| permission_error("js/spawn", error))?;
        let mut command = sandbox.wrap_command(r#"exec "$0" "$@""#);
        command
            .arg(&cmd)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = runtime.block_on(async {
            let child = command.spawn().map_err(rquickjs::Error::Io)?;
            let pid = child.id();
            match timeout(duration, child.wait_with_output()).await {
                Ok(output) => output.map_err(rquickjs::Error::Io),
                Err(_) => {
                    if let Some(pid) = pid {
                        kill_process_group(pid);
                    }
                    Err(timeout_error("js/spawn"))
                }
            }
        })?;
        Ok(SpawnResult {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            code: output.status.code().unwrap_or(-1),
        })
    }
}

pub fn register_host_globals(
    ctx: &Context,
    sandbox: Sandbox,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    runtime: tokio::runtime::Handle,
) {
    ctx.with(|ctx| {
        let globals = ctx.globals();

        globals
            .set(
                "read_file",
                Func::from(make_read_file(
                    permission.clone(),
                    ask_tx.clone(),
                    runtime.clone(),
                )),
            )
            .expect("register read_file");
        globals
            .set(
                "write_file",
                Func::from(make_write_file(
                    permission.clone(),
                    ask_tx.clone(),
                    runtime.clone(),
                )),
            )
            .expect("register write_file");
        globals
            .set(
                "spawn",
                Func::from(make_spawn(sandbox, permission, ask_tx, runtime)),
            )
            .expect("register spawn");

        let console = Object::new(ctx.clone()).expect("console object");
        console
            .set(
                "log",
                Func::from(|msg: Value| {
                    eprintln!("[js] {:?}", msg);
                }),
            )
            .expect("register console.log");
        globals.set("console", console).expect("register console");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_host_call_timeout_reports_execution_timed_out() {
        for tool in ["js/read_file", "js/write_file"] {
            let error = timeout_host_call(
                tool,
                Duration::from_millis(1),
                std::future::pending::<rquickjs::Result<()>>(),
            )
            .await
            .expect_err("pending host call should time out");

            assert!(
                error.to_string().contains("execution timed out"),
                "unexpected {tool} timeout error: {error}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_timeout_reports_execution_timed_out() {
        let runtime = tokio::runtime::Handle::current();
        let error = tokio::task::spawn_blocking(move || {
            let spawn = make_spawn_with_timeout(
                Sandbox::new(false, "bwrap"),
                None,
                None,
                runtime,
                Duration::from_millis(25),
            );
            spawn("sleep".to_string(), vec!["5".to_string()])
                .expect_err("sleep should time out")
                .to_string()
        })
        .await
        .expect("spawn timeout test task panicked");

        assert!(
            error.contains("execution timed out"),
            "unexpected spawn timeout error: {error}"
        );
    }
}
