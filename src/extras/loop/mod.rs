use std::path::PathBuf;

#[cfg(unix)]
use crate::process_creation::StdCommandCreationExt;

pub(crate) mod headless;
pub mod plan;
pub mod transcript;
pub(crate) mod validation;

pub const DEFAULT_PLAN_FILENAME: &str = "LOOP_PLAN.md";
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

pub struct LoopState {
    pub active: bool,
    pub prompt: String,
    pub plan_file: PathBuf,
    pub iteration: u32,
    pub max_iterations: Option<u32>,
    pub last_summary: Option<String>,
    pub run_cmd: Option<String>,
    pub last_run_output: Option<String>,
}

impl LoopState {
    pub fn new(
        prompt: String,
        plan_file: PathBuf,
        max_iterations: Option<u32>,
        run_cmd: Option<String>,
    ) -> Self {
        LoopState {
            active: true,
            prompt,
            plan_file,
            iteration: 0,
            max_iterations,
            last_summary: None,
            run_cmd,
            last_run_output: None,
        }
    }

    pub fn build_prompt(&self) -> String {
        let plan_contents = plan::read_plan(&self.plan_file).unwrap_or_default();

        let max_label = match self.max_iterations {
            Some(max) => max.to_string(),
            None => "∞".to_string(),
        };

        let summary = self.last_summary.as_deref().unwrap_or("starting fresh");
        let run_output = self.last_run_output.as_deref().unwrap_or("(none)");

        format!(
            "{}\n\n--- Loop Context (Iteration {}/{}) ---\n\nCurrent plan ({}):\n{}\n\nPrevious iteration summary:\n{}\n\nPrevious validation output:\n{}\n\n--- Instructions ---\n- Choose ONE task from the plan. Do not implement multiple things.\n- Before writing code, search the codebase with grep/find_files first.\n- After implementing: run the tests for the changed code.\n- Keep {} up to date: mark completed items, add new findings.\n- If you discover bugs unrelated to your task, document them in {}.\n- Commit working changes with descriptive messages.",
            self.prompt,
            self.iteration,
            max_label,
            self.plan_file.display(),
            plan_contents,
            summary,
            run_output,
            DEFAULT_PLAN_FILENAME,
            DEFAULT_PLAN_FILENAME,
        )
    }

    pub fn iteration_label(&self) -> String {
        let max_label = match self.max_iterations {
            Some(max) => max.to_string(),
            None => "∞".to_string(),
        };
        format!("LOOP {}/{}", self.iteration, max_label)
    }

    pub fn should_stop(&self) -> bool {
        match self.max_iterations {
            Some(max) => self.iteration > max,
            None => false,
        }
    }
}
