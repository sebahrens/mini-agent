pub(crate) mod audit;
pub(crate) mod broker;
#[cfg(test)]
pub mod engine;
pub mod host;
pub(crate) mod protocol;
#[cfg(feature = "skills")]
pub(crate) mod realm;
#[cfg(feature = "skills")]
pub mod skills;
pub(crate) mod supervisor;
pub mod tool;
pub mod types;
pub(crate) mod worker;

#[cfg(test)]
mod tests;

pub(crate) async fn verify_runtime(
    workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
) -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use rig::tool::Tool;

    let allow = host::AllowConfig::from_settings(workspace.root(), None, None, None, false, false)
        .with_workspace_binding(workspace);
    let tool = tool::JsTool::new(
        crate::sandbox::Sandbox::new(false, "release-runtime-check"),
        None,
        None,
        allow,
    );
    let result = tool
        .call(tool::JsArgs {
            code: "1 + 1".to_string(),
        })
        .await
        .context("JavaScript runtime self-check could not execute")?;
    ensure!(
        result == "2",
        "JavaScript runtime self-check returned {result:?}"
    );
    Ok(())
}
