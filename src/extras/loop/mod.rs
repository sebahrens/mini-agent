#[cfg(unix)]
use crate::process_creation::StdCommandCreationExt;

pub(crate) mod headless;
pub mod plan;
pub(crate) use crate::extras::validation;

pub const DEFAULT_PLAN_FILENAME: &str = "LOOP_PLAN.md";
/// Interactive loops must terminate even when the user does not specify a bound.
pub const DEFAULT_TUI_MAX_ITERATIONS: u32 = 100;
/// How much of an iteration's response a restart round carries forward.
pub const SUMMARY_TRUNCATION_CHARS: usize = 1024;

#[cfg(unix)]
pub(crate) fn verify_workflow_only_headless_relevance() -> anyhow::Result<()> {
    use anyhow::Context;

    let policy = include_str!("../../../scripts/loop-verification-policy.sh");
    let harness = format!(
        "{policy}\n\
         path_is_relevant_for_profile .github/workflows/ci.yml headless rust\n\
         path_is_relevant_for_profile .github/workflows/ci.yaml headless rust\n\
         ! path_is_relevant_for_profile docs/architecture.md headless rust\n"
    );
    let status = std::process::Command::new("bash")
        .arg("-c")
        .arg(harness)
        .status_guarded()
        .context("failed to execute the embedded loop verification policy")?;

    anyhow::ensure!(
        status.success(),
        "embedded loop verification policy rejected workflow-only headless changes"
    );
    Ok(())
}
