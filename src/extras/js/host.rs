use rquickjs::{prelude::Func, Context, Ctx, IntoJs, Object, Value};

use crate::extras::js::types::SpawnResult;
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

pub fn make_read_file() -> impl Fn(String) -> rquickjs::Result<String> {
    move |path: String| std::fs::read_to_string(&path).map_err(rquickjs::Error::Io)
}

pub fn make_write_file() -> impl Fn(String, String) -> rquickjs::Result<()> {
    move |path: String, content: String| {
        std::fs::write(&path, content).map_err(rquickjs::Error::Io)
    }
}

pub fn make_spawn(
    sandbox: Sandbox,
) -> impl Fn(String, Vec<String>) -> rquickjs::Result<SpawnResult> {
    move |cmd: String, args: Vec<String>| {
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

pub fn register_host_globals(ctx: &Context, sandbox: Sandbox) {
    ctx.with(|ctx| {
        let globals = ctx.globals();

        globals
            .set("read_file", Func::from(make_read_file()))
            .expect("register read_file");
        globals
            .set("write_file", Func::from(make_write_file()))
            .expect("register write_file");
        globals
            .set("spawn", Func::from(make_spawn(sandbox)))
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
