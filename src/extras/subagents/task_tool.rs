use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::stream::{FuturesUnordered, StreamExt};
use rig::completion::Usage;
use rig::tool::Tool;
use serde::Deserialize;
use tokio::time::Instant;

#[cfg(feature = "hooks")]
use crate::agent::runner::{SubagentRunOutput, usage_saturating_add};
use crate::agent::tools::{ToolError, check_perm};
use crate::extras::subagents::builder::{self, SubagentAuthorization};
use crate::extras::subagents::{clone_subagent_event_tx, with_config};
use crate::extras::truncate::truncate_cjk;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

/// Per-subagent wall-clock timeout, retained as a defense-in-depth bound in
/// addition to the configurable whole-call deadline.
const SUBAGENT_TIMEOUT: Duration = Duration::from_secs(300);

/// Hard cap on one subagent response. The aggregate output cap is the primary
/// control; this prevents a single completed child from monopolizing it.
const MAX_SUBAGENT_RESPONSE_BYTES: usize = 128 * 1024;

const DEFAULT_MAX_PROMPTS: usize = 8;
const DEFAULT_MAX_CONCURRENCY: usize = 4;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;
const DEFAULT_MAX_COST_UNITS: u64 = 500_000;
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(300);
const MIN_OUTPUT_BYTES: usize = 256;
const MAX_CALL_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Deserialize)]
pub struct TaskArgs {
    /// One or more exploration prompts. Concurrency and aggregate resources
    /// are bounded by the task-tool configuration.
    pub prompts: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
struct TaskLimits {
    max_prompts: usize,
    max_concurrency: usize,
    max_output_bytes: usize,
    max_cost_units: u64,
    timeout: Duration,
}

impl Default for TaskLimits {
    fn default() -> Self {
        Self {
            max_prompts: DEFAULT_MAX_PROMPTS,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_cost_units: DEFAULT_MAX_COST_UNITS,
            timeout: DEFAULT_CALL_TIMEOUT,
        }
    }
}

impl TaskLimits {
    fn from_config(config: &crate::config::Config) -> Self {
        Self {
            max_prompts: config.resolve_task_max_prompts(),
            max_concurrency: config.resolve_task_max_concurrency(),
            max_output_bytes: config.resolve_task_max_output_bytes(),
            max_cost_units: config.resolve_task_max_cost_units(),
            timeout: Duration::from_secs(config.resolve_task_timeout_secs()),
        }
    }

    fn validate(self) -> Result<Self, ToolError> {
        if self.max_prompts == 0 {
            return Err(ToolError::Msg(
                "task: task_max_prompts must be greater than zero".into(),
            ));
        }
        if self.max_concurrency == 0 {
            return Err(ToolError::Msg(
                "task: task_max_concurrency must be greater than zero".into(),
            ));
        }
        if self.max_output_bytes < MIN_OUTPUT_BYTES {
            return Err(ToolError::Msg(format!(
                "task: task_max_output_bytes must be at least {MIN_OUTPUT_BYTES}"
            )));
        }
        if self.max_cost_units == 0 {
            return Err(ToolError::Msg(
                "task: task_max_cost_units must be greater than zero".into(),
            ));
        }
        if self.timeout.is_zero() {
            return Err(ToolError::Msg(
                "task: task_timeout_secs must be greater than zero".into(),
            ));
        }
        if self.timeout > MAX_CALL_TIMEOUT {
            return Err(ToolError::Msg(format!(
                "task: task_timeout_secs must not exceed {}",
                MAX_CALL_TIMEOUT.as_secs()
            )));
        }
        Ok(self)
    }
}

fn validate_prompts(prompts: &[String], limits: TaskLimits) -> Result<(), ToolError> {
    if prompts.is_empty() {
        return Err(ToolError::Msg("task: prompts must not be empty".into()));
    }
    if prompts.len() > limits.max_prompts {
        return Err(ToolError::Msg(format!(
            "task: received {} prompts, maximum is {}",
            prompts.len(),
            limits.max_prompts
        )));
    }
    if let Some((index, _)) = prompts
        .iter()
        .enumerate()
        .find(|(_, prompt)| prompt.trim().is_empty())
    {
        return Err(ToolError::Msg(format!(
            "task: prompt {} must not be empty",
            index + 1
        )));
    }
    Ok(())
}

pub struct TaskTool {
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
}

impl TaskTool {
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        Self { permission, ask_tx }
    }
}

impl Tool for TaskTool {
    const NAME: &'static str = "task";
    type Error = ToolError;
    type Args = TaskArgs;
    type Output = String;

    fn description(&self) -> String {
        "Search and investigate the codebase via a fresh-context subagent. \
Use for any cross-file question: where is X used, how does Y work, \
find/list/count all X across the codebase, what calls Z, audit Q. \
The subagent reads, greps, finds files, lists directories, accesses memory, \
and returns a verified summary. \
Multiple prompts use bounded parallelism and return in prompt order. \
If a child fails or an aggregate resource limit is reached, remaining work \
is cancelled and explicit partial statuses are returned. \
Skip only for known-location work: reading one identified file, \
editing in a known location, grepping for a literal you will act on immediately."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        let max_prompts =
            with_config(|cfg| cfg.config.resolve_task_max_prompts()).unwrap_or(DEFAULT_MAX_PROMPTS);
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompts": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": max_prompts,
                    "items": { "type": "string", "minLength": 1 },
                    "description": "Investigation prompt for the subagent. Use one for a focused question, or multiple to run independent investigations with bounded parallelism. Examples: 'List all tests in this project', 'Where is config loaded?', 'How does the agent loop work?'"
                }
            },
            "required": ["prompts"]
        })
    }

    async fn call(&self, args: TaskArgs) -> Result<String, ToolError> {
        let (client, model_name, max_turns, config, limits) = with_config(|cfg| {
            (
                cfg.client.clone(),
                cfg.model_name.clone(),
                cfg.max_turns,
                cfg.config.clone(),
                TaskLimits::from_config(&cfg.config),
            )
        })
        .map_err(|err| ToolError::Msg(err.to_string()))?;
        let limits = limits.validate()?;
        validate_prompts(&args.prompts, limits)?;

        check_perm(
            &self.permission,
            &self.ask_tx,
            Self::NAME,
            &args.prompts.join(" | "),
        )
        .await?;

        let subagent_event_tx = clone_subagent_event_tx();

        #[cfg(feature = "archmd")]
        let architecture = with_config(|cfg| cfg.architecture.clone())
            .map_err(|err| ToolError::Msg(err.to_string()))?;

        let authorization =
            SubagentAuthorization::new(self.permission.clone(), self.ask_tx.clone());
        let executor: TaskExecutor = Arc::new(move |_index, prompt_text| {
            let client = client.clone();
            let model_name = model_name.clone();
            let event_tx = subagent_event_tx.clone();
            #[cfg(feature = "archmd")]
            let architecture = architecture.clone();
            let config = config.clone();
            let authorization = authorization.clone();
            Box::pin(async move {
                let display_prompt = prompt_text.clone();
                #[cfg(feature = "hooks")]
                let execution_prompt =
                    match crate::extras::hooks::dispatch_subagent_start("explore").await {
                        Some(extra) => format!("{extra}\n\n{prompt_text}"),
                        None => prompt_text,
                    };
                #[cfg(not(feature = "hooks"))]
                let execution_prompt = prompt_text;

                let model = client.completion_model(model_name);
                let agent = builder::build_explore_agent(
                    model,
                    max_turns,
                    &config,
                    authorization,
                    #[cfg(feature = "archmd")]
                    architecture,
                )
                .await;
                let result = tokio::time::timeout(
                    SUBAGENT_TIMEOUT,
                    agent.run_subagent(
                        &execution_prompt,
                        max_turns,
                        event_tx.as_ref(),
                        &config.retry,
                    ),
                )
                .await;
                let run = match result {
                    Ok(run) => run,
                    Err(_) => {
                        let output = Err("timeout: subagent exceeded 300s".to_string());
                        return ChildExecution {
                            cost_units: usage_cost_units(&Usage::new(), &display_prompt, &output),
                            output,
                        };
                    }
                };
                #[cfg(feature = "hooks")]
                let mut run = run;

                #[cfg(feature = "hooks")]
                if let Ok(response) = run.response.as_ref()
                    && let crate::extras::hooks::SubagentStopGate::Continue { reason } =
                        crate::extras::hooks::dispatch_subagent_stop("explore", false).await
                {
                    tracing::info!("hooks: SubagentStop forced continuation: {reason}");
                    let continuation = format!("{response}\n\n{reason}");
                    if let Ok(retried) = tokio::time::timeout(
                        SUBAGENT_TIMEOUT,
                        agent.run_subagent(
                            &continuation,
                            max_turns,
                            event_tx.as_ref(),
                            &config.retry,
                        ),
                    )
                    .await
                    {
                        run = merge_forced_continuation_run(run, retried);
                    }
                }

                let cost_units = usage_cost_units(&run.usage, &display_prompt, &run.response);
                ChildExecution {
                    output: run.response,
                    cost_units,
                }
            })
        });

        let report = execute_tasks(args.prompts, limits, executor).await;
        Ok(report.render())
    }
}

#[cfg(feature = "hooks")]
fn merge_forced_continuation_run(
    mut original: SubagentRunOutput,
    mut retried: SubagentRunOutput,
) -> SubagentRunOutput {
    let combined_usage = usage_saturating_add(original.usage, retried.usage);
    if retried.response.is_ok() {
        retried.usage = combined_usage;
        retried
    } else {
        original.usage = combined_usage;
        original
    }
}

type ChildFuture = Pin<Box<dyn Future<Output = ChildExecution> + Send>>;
type IndexedChildFuture = Pin<Box<dyn Future<Output = (usize, ChildExecution)> + Send>>;
type TaskExecutor = Arc<dyn Fn(usize, String) -> ChildFuture + Send + Sync>;

struct ChildExecution {
    output: Result<String, String>,
    cost_units: u64,
}

fn indexed_child_future(index: usize, prompt: String, child: ChildFuture) -> IndexedChildFuture {
    Box::pin(async move {
        let child = match AssertUnwindSafe(child).catch_unwind().await {
            Ok(child) => child,
            Err(_) => {
                let output = Err("subagent panicked".to_string());
                ChildExecution {
                    cost_units: usage_cost_units(&Usage::new(), &prompt, &output),
                    output,
                }
            }
        };
        (index, child)
    })
}

#[derive(Debug)]
enum TaskOutcome {
    Success(String),
    Failed(String),
    Cancelled(String),
    NotStarted(String),
}

#[derive(Debug)]
enum StopReason {
    ChildFailure(usize),
    OutputLimit,
    CostLimit,
    Deadline,
}

impl StopReason {
    fn description(&self, limits: TaskLimits) -> String {
        match self {
            Self::ChildFailure(index) => format!("task {} failed", index + 1),
            Self::OutputLimit => format!(
                "aggregate output limit of {} bytes reached",
                limits.max_output_bytes
            ),
            Self::CostLimit => format!(
                "aggregate cost limit of {} units reached",
                limits.max_cost_units
            ),
            Self::Deadline => format!(
                "wall-clock deadline of {}s reached",
                limits.timeout.as_secs()
            ),
        }
    }
}

struct TaskReport {
    prompts: Vec<String>,
    outcomes: Vec<TaskOutcome>,
    started: usize,
    completed: usize,
    cost_units: u64,
    stop_reason: Option<StopReason>,
    limits: TaskLimits,
}

impl TaskReport {
    fn render(&self) -> String {
        let mut rendered = String::new();
        if let Some(reason) = &self.stop_reason {
            rendered.push_str(&format!(
                "[partial: {}; started={}; completed={}; cost_units={}/{}]\n",
                reason.description(self.limits),
                self.started,
                self.completed,
                self.cost_units,
                self.limits.max_cost_units
            ));
        }

        let outputs: Vec<_> = self
            .outcomes
            .iter()
            .enumerate()
            .map(|(index, outcome)| {
                let text = match outcome {
                    TaskOutcome::Success(response) => response.clone(),
                    TaskOutcome::Failed(error) => format!("[failed: {error}]"),
                    TaskOutcome::Cancelled(reason) => format!("[cancelled: {reason}]"),
                    TaskOutcome::NotStarted(reason) => format!("[not started: {reason}]"),
                };
                (index, self.prompts[index].clone(), text)
            })
            .collect();
        rendered.push_str(&combine_results(&outputs));

        truncate_total_bytes(
            &rendered,
            self.limits.max_output_bytes,
            "\n…[task output truncated at aggregate limit]",
        )
    }
}

async fn execute_tasks(
    prompts: Vec<String>,
    limits: TaskLimits,
    executor: TaskExecutor,
) -> TaskReport {
    let task_count = prompts.len();
    let mut outcomes: Vec<Option<TaskOutcome>> =
        std::iter::repeat_with(|| None).take(task_count).collect();
    let mut started = vec![false; task_count];
    let mut next_index = 0usize;
    let mut completed = 0usize;
    let mut output_bytes = 0usize;
    let mut cost_units = 0u64;
    let mut stop_reason = None;
    let deadline = Instant::now() + limits.timeout;
    let mut in_flight: FuturesUnordered<IndexedChildFuture> = FuturesUnordered::new();

    while next_index < task_count && in_flight.len() < limits.max_concurrency {
        let index = next_index;
        next_index += 1;
        started[index] = true;
        let prompt = prompts[index].clone();
        let child = executor(index, prompt.clone());
        in_flight.push(indexed_child_future(index, prompt, child));
    }

    while !in_flight.is_empty() {
        let next = tokio::time::timeout_at(deadline, in_flight.next()).await;
        let next_result = match next {
            Ok(result) => result,
            Err(_) => {
                stop_reason = Some(StopReason::Deadline);
                break;
            }
        };
        let Some((index, child)) = next_result else {
            break;
        };

        completed += 1;
        cost_units = cost_units.saturating_add(child.cost_units);
        let section_overhead = section_overhead_bytes(index, &prompts[index], task_count);
        let remaining_output = limits
            .max_output_bytes
            .saturating_sub(output_bytes)
            .saturating_sub(section_overhead);
        let (outcome, body_len, child_failed, output_exhausted) = match child.output {
            Ok(response) => {
                let response = truncate_cjk(
                    &response,
                    MAX_SUBAGENT_RESPONSE_BYTES,
                    &format!(
                        "\n…[subagent response truncated at {}B]",
                        MAX_SUBAGENT_RESPONSE_BYTES
                    ),
                );
                let output_exhausted = response.len() > remaining_output;
                let response = truncate_total_bytes(
                    &response,
                    remaining_output,
                    "\n…[response stopped at aggregate output limit]",
                );
                let len = response.len();
                (TaskOutcome::Success(response), len, false, output_exhausted)
            }
            Err(error) => {
                let output_exhausted = error.len() > remaining_output;
                let error = truncate_total_bytes(
                    &error,
                    remaining_output,
                    "\n…[error stopped at aggregate output limit]",
                );
                let len = error.len();
                (TaskOutcome::Failed(error), len, true, output_exhausted)
            }
        };
        outcomes[index] = Some(outcome);
        output_bytes = output_bytes
            .saturating_add(section_overhead)
            .saturating_add(body_len);

        let work_remains = next_index < task_count || !in_flight.is_empty();
        if child_failed {
            stop_reason = Some(StopReason::ChildFailure(index));
        } else if output_exhausted || (output_bytes >= limits.max_output_bytes && work_remains) {
            stop_reason = Some(StopReason::OutputLimit);
        } else if cost_units > limits.max_cost_units
            || (cost_units == limits.max_cost_units && work_remains)
        {
            stop_reason = Some(StopReason::CostLimit);
        }
        if stop_reason.is_some() {
            break;
        }

        while next_index < task_count && in_flight.len() < limits.max_concurrency {
            let index = next_index;
            next_index += 1;
            started[index] = true;
            let prompt = prompts[index].clone();
            let child = executor(index, prompt.clone());
            in_flight.push(indexed_child_future(index, prompt, child));
        }
    }

    // Dropping the futures cancels every in-flight child before we construct
    // the report. Executors must not detach work into untracked tasks.
    drop(in_flight);

    if let Some(reason) = &stop_reason {
        let reason = reason.description(limits);
        for index in 0..task_count {
            if outcomes[index].is_none() {
                outcomes[index] = Some(if started[index] {
                    TaskOutcome::Cancelled(reason.clone())
                } else {
                    TaskOutcome::NotStarted(reason.clone())
                });
            }
        }
    }

    TaskReport {
        prompts,
        outcomes: outcomes
            .into_iter()
            .map(|outcome| {
                outcome.unwrap_or_else(|| {
                    TaskOutcome::Failed("task ended without an outcome".to_string())
                })
            })
            .collect(),
        started: started.into_iter().filter(|started| *started).count(),
        completed,
        cost_units,
        stop_reason,
        limits,
    }
}

fn section_overhead_bytes(index: usize, prompt: &str, task_count: usize) -> usize {
    // Reserve the exact heading/separator bytes plus one conservative trailing
    // newline. This makes the scheduler's aggregate bound include rendering,
    // not just child response bodies.
    let trailing_newline = 1;
    if task_count == 1 {
        return trailing_newline;
    }

    let separator = usize::from(index > 0);
    let label = prompt.chars().take(60).collect::<String>();
    separator
        .saturating_add(format!("## Task {}: {}\n\n", index + 1, label).len())
        .saturating_add(trailing_newline)
}

fn usage_cost_units(usage: &Usage, prompt: &str, response: &Result<String, String>) -> u64 {
    let itemized = usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.cached_input_tokens)
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.tool_use_prompt_tokens)
        .saturating_add(usage.reasoning_tokens);
    let reported = usage.total_tokens.max(itemized);
    if reported > 0 {
        return reported;
    }

    // Some providers do not report usage. Keep the budget enforceable with a
    // conservative text estimate instead of treating unknown cost as free.
    let response_len = response
        .as_ref()
        .map_or_else(|error| error.len(), |text| text.len());
    prompt
        .len()
        .saturating_add(response_len)
        .div_ceil(4)
        .try_into()
        .unwrap_or(u64::MAX)
}

fn truncate_total_bytes(value: &str, max_bytes: usize, marker: &str) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    if max_bytes == 0 {
        return String::new();
    }

    let marker = if marker.len() <= max_bytes {
        marker
    } else {
        let mut end = max_bytes;
        while !marker.is_char_boundary(end) {
            end -= 1;
        }
        return marker[..end].to_string();
    };
    let mut end = max_bytes - marker.len();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = value[..end].to_string();
    truncated.push_str(marker);
    truncated
}

/// Combine per-task outputs into a single Markdown string, ordered by the
/// original prompt index. Multiple tasks get `## Task N:` headings; a single
/// task is emitted as-is.
pub(crate) fn combine_results(outputs: &[(usize, String, String)]) -> String {
    let mut combined = String::new();
    for (idx, (_, prompt_text, response)) in outputs.iter().enumerate() {
        if outputs.len() > 1 {
            if idx > 0 {
                combined.push('\n');
            }
            let label = prompt_text.chars().take(60).collect::<String>();
            combined.push_str(&format!("## Task {}: {}\n\n", idx + 1, label));
        }
        combined.push_str(response);
        if !combined.ends_with('\n') {
            combined.push('\n');
        }
    }
    combined
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Clone)]
    struct FakeStep {
        delay: Duration,
        output: Result<String, String>,
        cost_units: u64,
    }

    #[derive(Default)]
    struct FakeCounters {
        started: AtomicUsize,
        live: AtomicUsize,
        peak: AtomicUsize,
    }

    struct LiveGuard(Arc<FakeCounters>);

    impl Drop for LiveGuard {
        fn drop(&mut self) {
            self.0.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn fake_executor(steps: Vec<FakeStep>, counters: Arc<FakeCounters>) -> TaskExecutor {
        Arc::new(move |index, _prompt| {
            let step = steps[index].clone();
            let counters = Arc::clone(&counters);
            Box::pin(async move {
                counters.started.fetch_add(1, Ordering::SeqCst);
                let live = counters.live.fetch_add(1, Ordering::SeqCst) + 1;
                counters.peak.fetch_max(live, Ordering::SeqCst);
                let _guard = LiveGuard(Arc::clone(&counters));
                tokio::time::sleep(step.delay).await;
                ChildExecution {
                    output: step.output,
                    cost_units: step.cost_units,
                }
            })
        })
    }

    fn limits() -> TaskLimits {
        TaskLimits {
            max_prompts: 8,
            max_concurrency: 2,
            max_output_bytes: 16 * 1024,
            max_cost_units: 1_000,
            timeout: Duration::from_secs(1),
        }
    }

    fn prompts(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("prompt {index}")).collect()
    }

    #[test]
    fn task_tool_limits_reject_prompt_overflow_before_execution() {
        let counters = Arc::new(FakeCounters::default());
        let request = prompts(3);
        let limits = TaskLimits {
            max_prompts: 2,
            ..limits()
        };

        let error = validate_prompts(&request, limits).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("received 3 prompts, maximum is 2")
        );
        assert_eq!(counters.started.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn task_tool_limits_reject_blank_prompt_before_execution() {
        let error = validate_prompts(&["valid".into(), "  ".into()], limits()).unwrap_err();
        assert!(error.to_string().contains("prompt 2 must not be empty"));
    }

    #[tokio::test]
    async fn task_tool_limits_bound_peak_concurrency() {
        let counters = Arc::new(FakeCounters::default());
        let steps = (0..5)
            .map(|index| FakeStep {
                delay: Duration::from_millis(10),
                output: Ok(format!("result {index}")),
                cost_units: 1,
            })
            .collect();

        let report = execute_tasks(
            prompts(5),
            limits(),
            fake_executor(steps, Arc::clone(&counters)),
        )
        .await;

        assert_eq!(report.started, 5);
        assert_eq!(report.completed, 5);
        assert!(counters.peak.load(Ordering::SeqCst) <= 2);
        assert_eq!(counters.live.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn task_tool_limits_cancel_in_flight_and_queued_work_after_failure() {
        let counters = Arc::new(FakeCounters::default());
        let steps = vec![
            FakeStep {
                delay: Duration::from_millis(200),
                output: Ok("late success".into()),
                cost_units: 1,
            },
            FakeStep {
                delay: Duration::from_millis(10),
                output: Err("boom".into()),
                cost_units: 7,
            },
            FakeStep {
                delay: Duration::ZERO,
                output: Ok("must not start".into()),
                cost_units: 1,
            },
            FakeStep {
                delay: Duration::ZERO,
                output: Ok("must not start".into()),
                cost_units: 1,
            },
        ];

        let report = execute_tasks(
            prompts(4),
            limits(),
            fake_executor(steps, Arc::clone(&counters)),
        )
        .await;
        let rendered = report.render();

        assert_eq!(report.started, 2);
        assert_eq!(report.completed, 1);
        assert_eq!(report.cost_units, 7);
        assert_eq!(counters.started.load(Ordering::SeqCst), 2);
        assert_eq!(counters.live.load(Ordering::SeqCst), 0);
        assert!(rendered.starts_with("[partial: task 2 failed"));
        assert!(rendered.contains("## Task 1:"));
        assert!(rendered.contains("[cancelled: task 2 failed]"));
        assert!(rendered.contains("## Task 2:"));
        assert!(rendered.contains("[failed: boom]"));
        assert!(rendered.contains("[not started: task 2 failed]"));
        let task_1 = rendered.find("## Task 1:").unwrap();
        let task_2 = rendered.find("## Task 2:").unwrap();
        let task_3 = rendered.find("## Task 3:").unwrap();
        assert!(task_1 < task_2 && task_2 < task_3);
    }

    #[tokio::test]
    async fn task_tool_limits_mixed_results_keep_order_and_accounting() {
        let counters = Arc::new(FakeCounters::default());
        let steps = vec![
            FakeStep {
                delay: Duration::from_millis(5),
                output: Ok("completed first".into()),
                cost_units: 3,
            },
            FakeStep {
                delay: Duration::from_millis(20),
                output: Err("second failed".into()),
                cost_units: 7,
            },
            FakeStep {
                delay: Duration::from_millis(200),
                output: Ok("must be cancelled".into()),
                cost_units: 11,
            },
            FakeStep {
                delay: Duration::ZERO,
                output: Ok("must not start".into()),
                cost_units: 13,
            },
        ];

        let report = execute_tasks(
            prompts(4),
            limits(),
            fake_executor(steps, Arc::clone(&counters)),
        )
        .await;
        let rendered = report.render();

        assert_eq!(report.started, 3);
        assert_eq!(report.completed, 2);
        assert_eq!(report.cost_units, 10);
        assert_eq!(counters.started.load(Ordering::SeqCst), 3);
        assert_eq!(counters.live.load(Ordering::SeqCst), 0);
        let success = rendered.find("completed first").unwrap();
        let failure = rendered.find("[failed: second failed]").unwrap();
        let cancelled = rendered.find("[cancelled: task 2 failed]").unwrap();
        let queued = rendered.find("[not started: task 2 failed]").unwrap();
        assert!(success < failure && failure < cancelled && cancelled < queued);
    }

    #[tokio::test]
    async fn task_tool_limits_stop_at_aggregate_output_bound() {
        let counters = Arc::new(FakeCounters::default());
        let steps = vec![
            FakeStep {
                delay: Duration::from_millis(10),
                output: Ok("x".repeat(2_000)),
                cost_units: 1,
            },
            FakeStep {
                delay: Duration::from_millis(200),
                output: Ok("must be cancelled".into()),
                cost_units: 1,
            },
            FakeStep {
                delay: Duration::ZERO,
                output: Ok("must not start".into()),
                cost_units: 1,
            },
        ];
        let limits = TaskLimits {
            max_concurrency: 2,
            max_output_bytes: 512,
            ..limits()
        };

        let report = execute_tasks(
            prompts(3),
            limits,
            fake_executor(steps, Arc::clone(&counters)),
        )
        .await;
        let rendered = report.render();

        assert_eq!(report.started, 2);
        assert_eq!(report.completed, 1);
        assert!(matches!(report.stop_reason, Some(StopReason::OutputLimit)));
        assert!(rendered.starts_with("[partial: aggregate output limit"));
        assert!(rendered.len() <= limits.max_output_bytes);
        assert_eq!(counters.live.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn task_tool_limits_stop_launching_after_cost_exhaustion() {
        let counters = Arc::new(FakeCounters::default());
        let steps = vec![
            FakeStep {
                delay: Duration::from_millis(10),
                output: Ok("first".into()),
                cost_units: 120,
            },
            FakeStep {
                delay: Duration::from_millis(200),
                output: Ok("must be cancelled".into()),
                cost_units: 1,
            },
            FakeStep {
                delay: Duration::ZERO,
                output: Ok("must not start".into()),
                cost_units: 1,
            },
        ];
        let limits = TaskLimits {
            max_concurrency: 2,
            max_cost_units: 100,
            ..limits()
        };

        let report = execute_tasks(
            prompts(3),
            limits,
            fake_executor(steps, Arc::clone(&counters)),
        )
        .await;
        let rendered = report.render();

        assert_eq!(report.started, 2);
        assert_eq!(report.completed, 1);
        assert_eq!(report.cost_units, 120);
        assert!(matches!(report.stop_reason, Some(StopReason::CostLimit)));
        assert!(rendered.starts_with("[partial: aggregate cost limit"));
        assert!(rendered.contains("[cancelled: aggregate cost limit"));
        assert!(rendered.contains("[not started: aggregate cost limit"));
        assert_eq!(counters.started.load(Ordering::SeqCst), 2);
        assert_eq!(counters.live.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn task_tool_limits_deadline_cancels_every_live_child() {
        let counters = Arc::new(FakeCounters::default());
        let steps = (0..4)
            .map(|_| FakeStep {
                delay: Duration::from_secs(1),
                output: Ok("too late".into()),
                cost_units: 1,
            })
            .collect();
        let limits = TaskLimits {
            timeout: Duration::from_millis(20),
            ..limits()
        };

        let report = execute_tasks(
            prompts(4),
            limits,
            fake_executor(steps, Arc::clone(&counters)),
        )
        .await;
        let rendered = report.render();

        assert_eq!(report.started, 2);
        assert_eq!(report.completed, 0);
        assert!(matches!(report.stop_reason, Some(StopReason::Deadline)));
        assert!(rendered.starts_with("[partial: wall-clock deadline"));
        assert_eq!(counters.live.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn task_tool_limits_render_successes_in_prompt_order() {
        let counters = Arc::new(FakeCounters::default());
        let steps = vec![
            FakeStep {
                delay: Duration::from_millis(20),
                output: Ok("result zero".into()),
                cost_units: 1,
            },
            FakeStep {
                delay: Duration::ZERO,
                output: Ok("result one".into()),
                cost_units: 1,
            },
            FakeStep {
                delay: Duration::from_millis(5),
                output: Ok("result two".into()),
                cost_units: 1,
            },
        ];

        let rendered = execute_tasks(
            prompts(3),
            limits(),
            fake_executor(steps, Arc::clone(&counters)),
        )
        .await
        .render();

        let zero = rendered.find("result zero").unwrap();
        let one = rendered.find("result one").unwrap();
        let two = rendered.find("result two").unwrap();
        assert!(zero < one && one < two);
    }

    #[test]
    fn task_tool_limits_cost_units_use_provider_usage_or_text_fallback() {
        let usage = Usage {
            total_tokens: 10,
            input_tokens: 8,
            output_tokens: 5,
            cached_input_tokens: 3,
            cache_creation_input_tokens: 2,
            ..Usage::new()
        };
        assert_eq!(
            usage_cost_units(&usage, "prompt", &Ok("response".into())),
            18
        );
        assert_eq!(
            usage_cost_units(&Usage::new(), "1234", &Ok("5678".into())),
            2
        );
    }

    #[cfg(feature = "hooks")]
    #[test]
    fn hooks_forced_subagent_continuation_saturates_usage_and_budget_cost() {
        let near_max = Usage {
            input_tokens: u64::MAX - 1,
            output_tokens: u64::MAX - 1,
            total_tokens: u64::MAX - 1,
            cached_input_tokens: u64::MAX - 1,
            cache_creation_input_tokens: u64::MAX - 1,
            tool_use_prompt_tokens: u64::MAX - 1,
            reasoning_tokens: u64::MAX - 1,
        };
        let increment = Usage {
            input_tokens: 10,
            output_tokens: 10,
            total_tokens: 10,
            cached_input_tokens: 10,
            cache_creation_input_tokens: 10,
            tool_use_prompt_tokens: 10,
            reasoning_tokens: 10,
        };
        let merged = merge_forced_continuation_run(
            SubagentRunOutput {
                response: Ok("original".to_string()),
                usage: near_max,
            },
            SubagentRunOutput {
                response: Ok("continued".to_string()),
                usage: increment,
            },
        );

        assert_eq!(merged.response.as_deref(), Ok("continued"));
        assert_eq!(merged.usage.input_tokens, u64::MAX);
        assert_eq!(merged.usage.output_tokens, u64::MAX);
        assert_eq!(merged.usage.total_tokens, u64::MAX);
        assert_eq!(merged.usage.cached_input_tokens, u64::MAX);
        assert_eq!(merged.usage.cache_creation_input_tokens, u64::MAX);
        assert_eq!(merged.usage.tool_use_prompt_tokens, u64::MAX);
        assert_eq!(merged.usage.reasoning_tokens, u64::MAX);
        assert_eq!(
            usage_cost_units(&merged.usage, "prompt", &merged.response),
            u64::MAX,
            "aggregate task budgeting must fail closed at the saturated maximum"
        );
    }

    #[test]
    fn task_tool_limits_total_truncation_is_utf8_safe_and_exact() {
        let result = truncate_total_bytes("記憶".repeat(100).as_str(), 64, "…[cut]");
        assert!(result.len() <= 64);
        assert!(result.ends_with("…[cut]"));
    }
}
