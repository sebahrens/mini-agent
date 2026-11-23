pub(crate) mod audit;
pub(crate) mod broker;
pub mod host;
pub(crate) mod memory;
mod pack;
mod pipe;
pub(crate) mod protocol;
#[cfg(feature = "skills")]
pub(crate) mod realm;
pub(crate) mod session;
#[cfg(feature = "skills")]
pub mod skills;
pub(crate) mod supervisor;
pub mod tool;
pub(crate) mod tool_bridge;
pub mod types;
pub(crate) mod worker;

#[cfg(test)]
mod tests;

/// Self-check failure for unavailable containment. The chain, outermost first,
/// is the "could not execute" summary, the closed worker witness line (CI and
/// the release smoke match it on its own stderr line), then the actionable
/// `{reason} ({backend})` detail.
fn containment_unavailable_error(backend: &str, reason: &str) -> anyhow::Error {
    anyhow::Error::msg(format!("{reason} ({backend})"))
        .context(supervisor::WorkerError::ContainmentUnavailable.to_string())
        .context("JavaScript runtime self-check could not execute")
}

pub(crate) async fn verify_runtime(
    workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
) -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use rig::tool::Tool;

    // The worker error is deliberately closed ("containment is unavailable"); the
    // cached preflight status carries the actionable reason (for example an
    // unsupported Windows install location), so report it here.
    if let Ok(crate::sandbox::worker::WorkerContainmentStatus::Unavailable {
        backend,
        reason,
        ..
    }) = tokio::task::spawn_blocking(crate::sandbox::worker::containment_status).await
    {
        return Err(containment_unavailable_error(&backend.to_string(), &reason));
    }
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

#[cfg(test)]
mod containment_witness_tests {
    #[test]
    fn unavailable_error_chain_carries_both_witnesses_in_order() {
        let error = super::containment_unavailable_error("appcontainer", "bad install location");
        let chain: Vec<String> = error.chain().map(|e| e.to_string()).collect();
        assert_eq!(
            chain,
            [
                "JavaScript runtime self-check could not execute",
                "JavaScript worker containment is unavailable",
                "bad install location (appcontainer)",
            ]
        );
    }
}
