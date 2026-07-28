use rquickjs::{Context, Ctx, IntoJs, Object, Value, prelude::Func};

use crate::agent::tools::{ToolError, check_perm, check_perm_path};
use crate::extras::js::types::SpawnResult;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::sandbox::Sandbox;

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

pub fn make_read_file(
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String) -> rquickjs::Result<String> {
    move |path: String| {
        runtime
            .block_on(check_perm_path(&permission, &ask_tx, "js/read_file", &path))
            .map_err(|error| permission_error("js/read_file", error))?;
        std::fs::read_to_string(&path).map_err(rquickjs::Error::Io)
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
        std::fs::write(&path, content).map_err(rquickjs::Error::Io)
    }
}

pub fn make_spawn(
    sandbox: Sandbox,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String, Vec<String>) -> rquickjs::Result<SpawnResult> {
    move |cmd: String, args: Vec<String>| {
        runtime
            .block_on(check_perm(&permission, &ask_tx, "js/spawn", &cmd))
            .map_err(|error| permission_error("js/spawn", error))?;
        let mut command = sandbox.wrap_command(r#"exec "$0" "$@""#).into_std();
        let output = command
            .arg(&cmd)
            .args(&args)
            .output()
            .map_err(rquickjs::Error::Io)?;
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
