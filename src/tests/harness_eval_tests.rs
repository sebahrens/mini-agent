use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rig::agent::AgentBuilder;
use rig::completion::{Message, Usage};
use rig::message::{AssistantContent, ToolResultContent, UserContent};
use rig::tool::{Tool, ToolDyn};
use serde::Deserialize;

use crate::agent::runner::{convert_history, run_print};
use crate::agent::tools::{EditTool, ToolError, WriteTool};
use crate::extras::js::host::AllowConfig;
use crate::extras::js::tool::JsTool;
use crate::extras::subagents::task_tool::{TaskArgs, run_scripted_task_for_eval};
use crate::retry::RetryConfig;
use crate::sandbox::Sandbox;
use crate::session::{MessageRole, Session};
use crate::tests::fake_model::{MockCompletionModel, MockStreamEvent};

const FIXTURES: &[&str] = &[
    include_str!("../../tests/harness_eval/fixtures/crlf_edit/fixture.json"),
    include_str!("../../tests/harness_eval/fixtures/js_aggregation/fixture.json"),
    include_str!("../../tests/harness_eval/fixtures/subagent_fanout/fixture.json"),
    include_str!("../../tests/harness_eval/fixtures/compaction_mid_task/fixture.json"),
    include_str!("../../tests/harness_eval/fixtures/persona_invocation/fixture.json"),
];

const PERSONA_FIXTURE: &str = include_str!("../../tests/harness_eval/personas/fixture.json");

#[cfg(all(feature = "skills", feature = "sandbox"))]
const GYM_TASKS: &str = include_str!("../../tests/harness_eval/task.json");

/// `defaults` supplies the fallback for every field a task omits. Nothing here
/// is a per-task value: a task that declares its own `prompt`, `initial_files`,
/// `oracle`, `budgets`, `scripted_provider_turns` or `expected_export`
/// overrides the default outright (see [`GymTask::resolve`]).
#[cfg(all(feature = "skills", feature = "sandbox"))]
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GymDefaults {
    prompt: String,
    initial_files: BTreeMap<PathBuf, String>,
    oracle: GymOracle,
    budgets: GymBudgets,
    scripted_provider_turns: BTreeMap<String, Vec<String>>,
    /// Name of the learned-skill export the library arm is expected to call.
    /// Shared with the Python runner's schema (`scripts/gym/mine_tasks.py`).
    expected_export: String,
    library: String,
}

#[cfg(all(feature = "skills", feature = "sandbox"))]
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct GymOracle {
    expected_files: BTreeMap<PathBuf, String>,
    id: String,
}

#[cfg(all(feature = "skills", feature = "sandbox"))]
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GymBudgets {
    max_provider_turns: usize,
    max_tool_calls: usize,
    max_total_tokens: u64,
}

#[cfg(all(feature = "skills", feature = "sandbox"))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GymTaskFile {
    defaults: GymDefaults,
    tasks: Vec<GymTask>,
}

#[cfg(all(feature = "skills", feature = "sandbox"))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GymTask {
    name: String,
    tags: Vec<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    initial_files: Option<BTreeMap<PathBuf, String>>,
    #[serde(default)]
    oracle: Option<GymOracle>,
    #[serde(default)]
    budgets: Option<GymBudgets>,
    #[serde(default)]
    scripted_provider_turns: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default)]
    expected_export: Option<String>,
}

/// One task's effective fixture: its own overrides where present, `defaults`
/// everywhere else.
#[cfg(all(feature = "skills", feature = "sandbox"))]
struct GymResolvedTask<'a> {
    prompt: &'a str,
    initial_files: &'a BTreeMap<PathBuf, String>,
    oracle: &'a GymOracle,
    budgets: &'a GymBudgets,
    scripted_provider_turns: &'a BTreeMap<String, Vec<String>>,
    expected_export: &'a str,
}

#[cfg(all(feature = "skills", feature = "sandbox"))]
impl GymTask {
    fn resolve<'a>(&'a self, defaults: &'a GymDefaults) -> GymResolvedTask<'a> {
        GymResolvedTask {
            prompt: self.prompt.as_deref().unwrap_or(&defaults.prompt),
            initial_files: self
                .initial_files
                .as_ref()
                .unwrap_or(&defaults.initial_files),
            oracle: self.oracle.as_ref().unwrap_or(&defaults.oracle),
            budgets: self.budgets.as_ref().unwrap_or(&defaults.budgets),
            scripted_provider_turns: self
                .scripted_provider_turns
                .as_ref()
                .unwrap_or(&defaults.scripted_provider_turns),
            expected_export: self
                .expected_export
                .as_deref()
                .unwrap_or(&defaults.expected_export),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Fixture {
    name: String,
    prompt: String,
    max_provider_turns: usize,
    max_tool_calls: usize,
    max_total_tokens: u64,
    initial_files: BTreeMap<PathBuf, String>,
    expected_files: BTreeMap<PathBuf, String>,
}

#[derive(Debug, Deserialize)]
struct PersonaFixture {
    repository_files: BTreeMap<PathBuf, String>,
    cases: Vec<PersonaCase>,
}

#[derive(Debug, Deserialize)]
struct PersonaCase {
    agent_type: String,
    prompt: String,
    expected_finding: String,
    response: String,
}

#[derive(Debug)]
struct Metrics {
    name: String,
    provider_turns: usize,
    tool_calls: usize,
    total_tokens: u64,
}

struct EvalDirectory(PathBuf);

impl EvalDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mini-agent-harness-eval-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).expect("create harness workspace");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for EvalDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct EvalTaskTool {
    workspace: Arc<crate::paths::WorkspaceBinding>,
    responses: Vec<String>,
}

impl Tool for EvalTaskTool {
    const NAME: &'static str = "task";
    type Error = ToolError;
    type Args = TaskArgs;
    type Output = String;

    fn description(&self) -> String {
        "Run deterministic read-only fixture subagents through the production task scheduler."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompts": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1
                },
                "agent_type": { "type": "string" }
            },
            "required": ["prompts"]
        })
    }

    async fn call(&self, args: TaskArgs) -> Result<String, ToolError> {
        run_scripted_task_for_eval(args, self.workspace.clone(), self.responses.clone()).await
    }
}

fn parse_fixtures() -> Vec<Fixture> {
    FIXTURES
        .iter()
        .map(|fixture| serde_json::from_str(fixture).expect("valid harness fixture"))
        .collect()
}

fn usage(input_tokens: u64, output_tokens: u64) -> Usage {
    Usage {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens + output_tokens,
        ..Usage::new()
    }
}

fn tool_turn(id: &str, name: &str, args: serde_json::Value) -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::tool_call(id, name, args),
        MockStreamEvent::final_response(usage(100, 20)),
    ]
}

fn done_turn() -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::text("done"),
        MockStreamEvent::final_response(usage(80, 20)),
    ]
}

fn structured_subagent_report(finding: &str, covered: &str) -> String {
    format!(
        "## Findings\n- [confidence: high] {finding}\n\n\
         ## Unverified\n- None.\n\n\
         ## Coverage\n- Covered: {covered}.\n- Skipped: None."
    )
}

fn write_fixture(root: &Path, fixture: &Fixture) {
    for (relative, content) in &fixture.initial_files {
        let target = root.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("create fixture directory");
        }
        std::fs::write(target, content.as_bytes()).expect("write fixture file");
    }
}

fn collect_files(root: &Path) -> BTreeMap<PathBuf, String> {
    fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, String>) {
        let mut entries = std::fs::read_dir(directory)
            .expect("read fixture directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("read fixture entries");
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let kind = entry.file_type().expect("read fixture entry type");
            if kind.is_dir() {
                visit(root, &path, files);
            } else if kind.is_file() {
                let relative = path.strip_prefix(root).expect("fixture-relative path");
                let content = std::fs::read_to_string(&path).expect("UTF-8 fixture output");
                files.insert(relative.to_path_buf(), content);
            } else {
                panic!("fixture contains unsupported entry: {}", path.display());
            }
        }
    }

    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

fn count_tool_calls(interactions: &[Message]) -> usize {
    interactions
        .iter()
        .map(|message| match message {
            Message::Assistant { content, .. } => content
                .iter()
                .filter(|item| matches!(item, AssistantContent::ToolCall(_)))
                .count(),
            Message::User { .. } | Message::System { .. } => 0,
        })
        .sum()
}

fn tool_call_args<'a>(interactions: &'a [Message], name: &str) -> &'a serde_json::Value {
    interactions
        .iter()
        .find_map(|message| match message {
            Message::Assistant { content, .. } => content.iter().find_map(|item| match item {
                AssistantContent::ToolCall(call) if call.function.name == name => {
                    Some(&call.function.arguments)
                }
                _ => None,
            }),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing {name} tool call"))
}

fn tool_result_text<'a>(interactions: &'a [Message], id: &str) -> &'a str {
    interactions
        .iter()
        .find_map(|message| match message {
            Message::User { content } => content.iter().find_map(|item| match item {
                UserContent::ToolResult(result) if result.id == id => {
                    result.content.iter().find_map(|part| match part {
                        ToolResultContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                }
                _ => None,
            }),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing result for tool call {id}"))
}

fn scripted_case(
    fixture: &Fixture,
    workspace: Arc<crate::paths::WorkspaceBinding>,
) -> (MockCompletionModel, Vec<Box<dyn ToolDyn>>, Vec<Message>) {
    let root = workspace.root().to_path_buf();
    let edit =
        || Box::new(EditTool::new(None, None).with_workspace(root.clone())) as Box<dyn ToolDyn>;
    let write = || {
        Box::new(WriteTool::new(None, None, None).with_workspace(root.clone())) as Box<dyn ToolDyn>
    };

    match fixture.name.as_str() {
        "crlf_edit" => (
            MockCompletionModel::from_stream_turns(vec![
                tool_turn(
                    "edit-crlf",
                    "edit",
                    serde_json::json!({
                        "path": "settings.ini",
                        "block": "<<<<<<< SEARCH\ntitle = old\n=======\ntitle = new\n>>>>>>> REPLACE"
                    }),
                ),
                done_turn(),
            ]),
            vec![edit()],
            Vec::new(),
        ),
        "js_aggregation" => {
            let allow = AllowConfig::unrestricted(&root).with_workspace_binding(workspace.clone());
            let sandbox = Sandbox::new(false, "harness-eval").with_workspace_binding(workspace);
            let js = JsTool::new(sandbox, None, None, allow);
            (
                MockCompletionModel::from_stream_turns(vec![
                    tool_turn(
                        "aggregate-js",
                        "js",
                        serde_json::json!({
                            "code": "const a = JSON.parse(read_file('data/a.json')); const b = JSON.parse(read_file('data/b.json')); const total = [...a, ...b].reduce((sum, n) => sum + n, 0); write_file('total.txt', `${total}\\n`); total;"
                        }),
                    ),
                    done_turn(),
                ]),
                vec![Box::new(js)],
                Vec::new(),
            )
        }
        "subagent_fanout" => {
            let task = EvalTaskTool {
                workspace,
                responses: vec![
                    structured_subagent_report("frontend: ui clean", "frontend.txt"),
                    structured_subagent_report("backend: api stable", "backend.txt"),
                ],
            };
            (
                MockCompletionModel::from_stream_turns(vec![
                    tool_turn(
                        "fanout",
                        "task",
                        serde_json::json!({
                            "prompts": ["inspect frontend.txt", "inspect backend.txt"]
                        }),
                    ),
                    tool_turn(
                        "write-report",
                        "write",
                        serde_json::json!({
                            "path": "report.md",
                            "content": "frontend: ui clean\nbackend: api stable\n"
                        }),
                    ),
                    done_turn(),
                ]),
                vec![Box::new(task), write()],
                Vec::new(),
            )
        }
        "compaction_mid_task" => {
            let mut session = Session::new("eval", "scripted", 4096, &root.to_string_lossy());
            session.add_message(MessageRole::User, "Inspect the pending state.");
            session.add_message(MessageRole::Assistant, "The state still needs updating.");
            session.add_message(
                MessageRole::User,
                "Preserve the decision and continue later.",
            );
            session.compress(
                "State inspection finished; update state.txt to complete.".into(),
                2,
                20,
            );
            (
                MockCompletionModel::from_stream_turns(vec![
                    tool_turn(
                        "finish-state",
                        "edit",
                        serde_json::json!({
                            "path": "state.txt",
                            "block": "<<<<<<< SEARCH\npending\n=======\ncomplete\n>>>>>>> REPLACE"
                        }),
                    ),
                    done_turn(),
                ]),
                vec![edit()],
                convert_history(&session),
            )
        }
        "persona_invocation" => {
            let task = EvalTaskTool {
                workspace,
                responses: vec![structured_subagent_report(
                    "rust-security-review: pass",
                    "src/lib.rs integer arithmetic",
                )],
            };
            (
                MockCompletionModel::from_stream_turns(vec![
                    tool_turn(
                        "persona",
                        "task",
                        serde_json::json!({
                            "prompts": ["audit src/lib.rs for integer overflow"],
                            "agent_type": "rust-security-review"
                        }),
                    ),
                    tool_turn(
                        "write-audit",
                        "write",
                        serde_json::json!({
                            "path": "audit.txt",
                            "content": "rust-security-review: pass\n"
                        }),
                    ),
                    done_turn(),
                ]),
                vec![Box::new(task), write()],
                Vec::new(),
            )
        }
        other => panic!("no scripted provider case for {other}"),
    }
}

#[test]
fn harness_eval_fixture_contract_is_complete() {
    let fixtures = parse_fixtures();
    // No length check against `FIXTURES` here: `parse_fixtures` maps that
    // array, so its length can only restate the constant. The name set below
    // is read out of the fixture files themselves and does pin the suite.
    let names = fixtures
        .iter()
        .map(|fixture| fixture.name.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(names.len(), fixtures.len(), "fixture names must be unique");
    assert_eq!(
        names,
        BTreeSet::from([
            "compaction_mid_task",
            "crlf_edit",
            "js_aggregation",
            "persona_invocation",
            "subagent_fanout",
        ])
    );
    for fixture in fixtures {
        assert!(!fixture.prompt.trim().is_empty(), "{} prompt", fixture.name);
        assert!(
            fixture.max_provider_turns > 0,
            "{} turn budget",
            fixture.name
        );
        assert!(fixture.max_tool_calls > 0, "{} tool budget", fixture.name);
        assert!(
            fixture.max_total_tokens > 0,
            "{} token budget",
            fixture.name
        );
        assert!(
            !fixture.initial_files.is_empty(),
            "{} initial tree",
            fixture.name
        );
        assert!(
            !fixture.expected_files.is_empty(),
            "{} expected tree",
            fixture.name
        );
        for path in fixture
            .initial_files
            .keys()
            .chain(fixture.expected_files.keys())
        {
            assert!(
                !path.is_absolute(),
                "{} absolute fixture path",
                fixture.name
            );
            assert!(
                path.components()
                    .all(|part| matches!(part, Component::Normal(_))),
                "{} unsafe fixture path: {}",
                fixture.name,
                path.display()
            );
        }
        assert!(
            fixture
                .initial_files
                .keys()
                .all(|path| fixture.expected_files.contains_key(path)),
            "{} must specify the final state of every initial file",
            fixture.name
        );
    }
}

/// Fixture-shape contract for `tests/harness_eval/task.json`.
///
/// The gym is only an eval if its tasks differ. This test measures the
/// *resolved* fixtures (per-task override, else `defaults`) and fails if the
/// file collapses back into one task wearing twenty names: it requires twenty
/// distinct prompts, input trees, oracles and scripted turn pairs, at least
/// four different learned-skill exports across the suite, and a baseline arm
/// that never names a learned-skill export. It also keeps the fallback path
/// covered by requiring at least one task that inherits `defaults` wholesale.
#[cfg(all(feature = "skills", feature = "sandbox"))]
#[test]
fn gym_task_fixtures_are_distinct_and_library_free_in_the_baseline_arm() {
    let specification: GymTaskFile = serde_json::from_str(GYM_TASKS).expect("valid gym task file");
    assert_eq!(specification.tasks.len(), 20);
    let names = specification
        .tasks
        .iter()
        .map(|task| task.name.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        names.len(),
        specification.tasks.len(),
        "task names are unique"
    );

    let resolved = specification
        .tasks
        .iter()
        .map(|task| (task, task.resolve(&specification.defaults)))
        .collect::<Vec<_>>();
    let exports = resolved
        .iter()
        .map(|(_, fixture)| fixture.expected_export)
        .collect::<BTreeSet<_>>();
    assert!(
        exports.len() >= 4,
        "the suite must exercise at least four learned-skill exports, saw {exports:?}"
    );
    for (task, fixture) in &resolved {
        assert!(!task.tags.is_empty(), "{} tags", task.name);
        assert!(!fixture.prompt.trim().is_empty(), "{} prompt", task.name);
        assert!(
            fixture.budgets.max_provider_turns > 0,
            "{} turns",
            task.name
        );
        assert!(
            fixture.budgets.max_tool_calls > 0,
            "{} tool calls",
            task.name
        );
        assert!(fixture.budgets.max_total_tokens > 0, "{} tokens", task.name);
        assert!(
            !fixture.initial_files.is_empty(),
            "{} initial tree",
            task.name
        );
        for path in fixture
            .initial_files
            .keys()
            .chain(fixture.oracle.expected_files.keys())
        {
            assert!(!path.is_absolute(), "{} absolute path", task.name);
            assert!(
                path.components()
                    .all(|part| matches!(part, Component::Normal(_))),
                "{} unsafe path: {}",
                task.name,
                path.display()
            );
        }
        for (path, content) in fixture.initial_files {
            assert_eq!(
                fixture.oracle.expected_files.get(path),
                Some(content),
                "{} must declare the final state of {}",
                task.name,
                path.display()
            );
        }
        assert!(
            fixture
                .oracle
                .expected_files
                .contains_key(Path::new("output.txt")),
            "{} oracle must require an output.txt",
            task.name
        );
        let arms = &fixture.scripted_provider_turns;
        for arm in ["none", "library"] {
            let script = arms
                .get(arm)
                .unwrap_or_else(|| panic!("{} is missing the {arm} arm", task.name));
            assert_eq!(script.len(), 1, "{} {arm} scripted turns", task.name);
        }
        assert_ne!(
            arms["none"][0], arms["library"][0],
            "{} runs the same script in both arms",
            task.name
        );
        assert!(
            arms["library"][0].contains(fixture.expected_export),
            "{} declares export {} but its library turn never calls it",
            task.name,
            fixture.expected_export
        );
        // The baseline arm exists to measure the cost of *not* having the
        // library, so it must hand-write the transformation itself. (That the
        // declared names really are the admitted library's exports is checked
        // against the seeded store in
        // `task_json_library_axis_uses_real_store_and_records_oracles`.)
        for export in &exports {
            assert!(
                !arms["none"][0].contains(export),
                "{} baseline arm calls learned-skill export {export}",
                task.name
            );
        }
    }

    let count = resolved.len();
    let distinct = |mut values: Vec<String>| {
        values.sort();
        values.dedup();
        values.len()
    };
    assert_eq!(
        distinct(
            resolved
                .iter()
                .map(|(_, fixture)| fixture.prompt.to_string())
                .collect()
        ),
        count,
        "every task needs its own prompt"
    );
    assert_eq!(
        distinct(
            resolved
                .iter()
                .map(|(_, fixture)| format!("{:?}", fixture.initial_files))
                .collect()
        ),
        count,
        "every task needs its own input tree"
    );
    assert_eq!(
        distinct(
            resolved
                .iter()
                .map(|(_, fixture)| format!("{:?}", fixture.oracle))
                .collect()
        ),
        count,
        "every task needs its own oracle"
    );
    assert_eq!(
        distinct(
            resolved
                .iter()
                .map(|(_, fixture)| format!("{:?}", fixture.scripted_provider_turns))
                .collect()
        ),
        count,
        "every task needs its own scripted turns"
    );
    assert!(
        specification
            .tasks
            .iter()
            .any(|task| task.prompt.is_none() && task.oracle.is_none()),
        "at least one task must inherit defaults so the fallback path stays covered"
    );
    assert!(
        specification
            .tasks
            .iter()
            .any(|task| task.budgets.is_some()),
        "at least one task must override the default budgets"
    );
}

#[cfg(all(feature = "skills", feature = "sandbox"))]
#[derive(Debug)]
struct GymArmMeasurement {
    arm: &'static str,
    turn_id: String,
    /// Identifier of the admitted seed whose export this task's library turn
    /// names. `None` for the no-library arm, which is given an empty bundle on
    /// purpose.
    skill_id: Option<String>,
    provider_turns: usize,
    tool_calls: usize,
    total_tokens: u64,
}

/// Library-axis gym eval over `tests/harness_eval/task.json`.
///
/// Every axis in the emitted `HARNESS_EVAL` record is measured from what the
/// arm actually did; none of them is a fixture constant:
///
/// * `provider_turns` — completion requests the scripted provider really
///   received (`MockCompletionModel::requests`).
/// * `tool_calls` — assistant tool calls in the transcript `run_print`
///   returned.
/// * `total_tokens` — cumulative usage `run_print` reconciled for the run.
/// * `js_round_trips` — durable `skill_events` rows of kind `invoked` carrying
///   that arm's own `turn_id`, i.e. learned-skill exports the contained worker
///   really called.
///
/// The delta gate therefore compares two independently measured arms: the
/// no-library arm must record zero learned-skill invocations, the library arm
/// exactly one — and specifically one of the seed that exports the name the
/// task's fixture declares — and the two arms must agree on the three cost
/// axes. A library arm that silently fell back to hand-written JavaScript emits
/// no `invoked` event and fails the gate.
///
/// The library itself is the one operators get: `library: "seeds"` dispatches
/// to `operations::run(.., LibraryOperation::InstallSeeds, ..)`, which admits
/// the bundled `assets/learned-skills` packages through canonicalization and
/// the contained held-out verifier, followed by `Approve` and `Activate` —
/// the same operator episode `src/extras/js/skills/operations.rs` tests. Nothing
/// is hand-inserted into the store. Admission runs a contained worker per seed,
/// so seeding happens once for the whole arm rather than once per task; the
/// per-task work stays the two agent runs.
#[cfg(all(feature = "skills", feature = "sandbox"))]
#[tokio::test]
async fn task_json_library_axis_uses_real_store_and_records_oracles() {
    use crate::agent::runner::TaskOutcomeRecorder;
    use crate::extras::js::skills::index::RetrievalPolicy;
    use crate::extras::js::skills::operations::{LibraryOperation, run as run_library_operation};
    use crate::extras::js::skills::store::SkillStore;
    use crate::extras::js::skills::telemetry::TelemetryDispatcher;
    use crate::extras::js::skills::turn::{SkillRuntime, SkillTurnContext, TurnSkillBundle};
    use crate::extras::skills::index::AgentSkillSearchPolicy;

    let specification: GymTaskFile = serde_json::from_str(GYM_TASKS).unwrap();
    assert_eq!(specification.tasks.len(), 20);

    let state = EvalDirectory::new();
    let paths = crate::paths::AppPaths {
        config_dir: state.path().join("config"),
        data_dir: state.path().join("data"),
        local_data_dir: state.path().join("local"),
        state_dir: state.path().join("state"),
        cache_dir: state.path().join("cache"),
        credentials_dir: state.path().join("credentials"),
        project_dir: None,
    };
    // Dispatch on the fixture's library axis rather than asserting its value:
    // an unrecognised axis must fail loudly instead of silently seeding a
    // library the fixture never asked for.
    let seeded_ids: Vec<String> = match specification.defaults.library.as_str() {
        "seeds" => {
            run_library_operation(
                None,
                false,
                None,
                Some(LibraryOperation::InstallSeeds),
                &paths,
                None,
            )
            .unwrap_or_else(|error| panic!("install-seeds failed: {error:#}"));
            // `install-seeds` only returns `Ok` for a package whose proposal
            // reached the reviewable end of admission, so this reads back the
            // ids admission actually produced instead of naming them here.
            let verified = {
                let store = SkillStore::open_at(&paths).unwrap();
                let mut statement = store
                    .conn()
                    .prepare(
                        "SELECT DISTINCT skill_id FROM skill_proposals
                          WHERE status IN ('awaiting_approval', 'approved')
                          ORDER BY skill_id",
                    )
                    .unwrap();
                statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .unwrap()
                    .collect::<Result<Vec<String>, _>>()
                    .unwrap()
            };
            assert!(
                !verified.is_empty(),
                "install-seeds admitted no learned skill through the held-out verifier"
            );
            for id in &verified {
                run_library_operation(
                    None,
                    false,
                    None,
                    Some(LibraryOperation::Approve(id.as_str())),
                    &paths,
                    None,
                )
                .unwrap_or_else(|error| panic!("approving seed {id} failed: {error:#}"));
                run_library_operation(
                    None,
                    false,
                    None,
                    Some(LibraryOperation::Activate(id.as_str())),
                    &paths,
                    None,
                )
                .unwrap_or_else(|error| panic!("activating seed {id} failed: {error:#}"));
            }
            let store = SkillStore::open_at(&paths).unwrap();
            for id in &verified {
                assert_eq!(
                    store.metadata(id).unwrap().unwrap().status,
                    "active",
                    "seed {id} never reached the active lifecycle state"
                );
            }
            verified
        }
        other => panic!("unsupported gym library axis {other:?}"),
    };
    let runtime = SkillRuntime::open(&paths, None)
        .unwrap()
        .with_test_policies(
            RetrievalPolicy {
                // Every admitted seed must fit in one bundle so a task can name
                // the export it needs; the floors keep the deterministic test
                // embedder from dropping candidates before fusion.
                max_skills: seeded_ids.len().max(RetrievalPolicy::default().max_skills),
                dense_score_floor: -1.0,
                lexical_score_floor: -1.0,
                ..RetrievalPolicy::default()
            },
            AgentSkillSearchPolicy::default(),
        );
    runtime.settle_learned_rebuild_for_test().await;
    let dispatcher = Arc::new(TelemetryDispatcher::spawn(&paths).unwrap());

    // One probe turn maps the exports the admitted library actually publishes
    // to the artifacts that own them. Nothing is run against this bundle; it is
    // the ground truth the fixtures' scripted turns are matched against, so a
    // task cannot claim a skill the library never admitted.
    let library_exports: BTreeMap<String, String> = {
        let discovery = runtime.prepare_turn("learned skill library probe").await;
        discovery
            .learned_js
            .skills
            .iter()
            .flat_map(|skill| {
                skill
                    .exports
                    .iter()
                    .map(move |export| (export.name.clone(), skill.id.clone()))
            })
            .collect()
    };
    let reachable = library_exports.values().collect::<BTreeSet<_>>();
    assert_eq!(
        reachable.len(),
        seeded_ids.len(),
        "retrieval reached {} of the {} admitted seeds",
        reachable.len(),
        seeded_ids.len()
    );

    let mut runs: Vec<(String, Vec<GymArmMeasurement>)> = Vec::new();
    for task in &specification.tasks {
        assert!(!task.tags.is_empty(), "{} tags", task.name);
        // Per-task overrides where the fixture declares them, `defaults`
        // otherwise; every axis below is read through this view.
        let fixture = task.resolve(&specification.defaults);
        // Budgets are not asserted as constants here: they are handed to the
        // agent as the real turn/token caps below and then compared against the
        // measured cost of each arm.
        let budgets = fixture.budgets;
        let mut measurements: Vec<GymArmMeasurement> = Vec::new();
        for arm in ["none", "library"] {
            let directory = EvalDirectory::new();
            for (relative, content) in fixture.initial_files {
                let target = directory.path().join(relative);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(target, content.as_bytes()).unwrap();
            }
            let scripts = &fixture.scripted_provider_turns[arm];
            assert_eq!(scripts.len(), 1, "{} {arm} scripted turns", task.name);
            // Which learned-skill export this turn reaches is read off the
            // scripted turn itself rather than declared beside it, so the
            // fixture cannot claim an invocation its JavaScript never makes.
            let named = library_exports
                .iter()
                .filter(|(export, _)| scripts[0].contains(export.as_str()))
                .collect::<Vec<_>>();
            let mut skill_id = None;
            let context = if arm == "library" {
                assert_eq!(
                    named.len(),
                    1,
                    "{} library turn must call exactly one admitted export, it names {:?}",
                    task.name,
                    named.iter().map(|(export, _)| export).collect::<Vec<_>>()
                );
                let (export, expected_id) = named[0];
                assert_eq!(
                    export.as_str(),
                    fixture.expected_export,
                    "{} declares export {} but its library turn calls {export}",
                    task.name,
                    fixture.expected_export
                );
                let query = format!("{} {}", fixture.prompt, task.tags.join(" "));
                let discovery = runtime.prepare_turn(&query).await;
                assert!(
                    discovery
                        .learned_js
                        .skills
                        .iter()
                        .any(|item| &item.id == expected_id),
                    "{} retrieved no admitted seed exporting {export}",
                    task.name
                );
                skill_id = Some(expected_id.clone());
                runtime.turn_context()
            } else {
                // The baseline arm must pay for the work itself: naming any
                // admitted export here would make the delta meaningless.
                assert!(
                    named.is_empty(),
                    "{} baseline turn calls admitted export(s) {:?}",
                    task.name,
                    named.iter().map(|(export, _)| export).collect::<Vec<_>>()
                );
                Arc::new(SkillTurnContext::new(TurnSkillBundle::empty("gym-none")))
            };
            // Captured before the run so the arm's telemetry can be counted by
            // turn afterwards; the bundle is only replaced at a turn boundary.
            let turn_id = context.snapshot().turn_id.clone();
            let workspace =
                Arc::new(crate::paths::WorkspaceBinding::capture(directory.path()).unwrap());
            let tool = JsTool::new(
                Sandbox::new(false, "gym-eval").with_workspace_binding(workspace.clone()),
                None,
                None,
                AllowConfig::unrestricted(directory.path()).with_workspace_binding(workspace),
            )
            .with_skill_turn_context(context.clone())
            .with_shared_telemetry(dispatcher.clone());
            // Run the arm through the production agent loop (as
            // `harness_regression_eval` does) so provider turns, tool calls and
            // tokens are observed rather than assumed.
            let model = MockCompletionModel::from_stream_turns(vec![
                tool_turn(
                    &format!("gym-{arm}"),
                    "js",
                    serde_json::json!({ "code": scripts[0] }),
                ),
                done_turn(),
            ]);
            let agent = AgentBuilder::new(model.clone())
                .tools(vec![Box::new(tool) as Box<dyn ToolDyn>])
                .default_max_turns(budgets.max_provider_turns)
                .build();
            let (response, usage, interactions) = run_print(
                &agent,
                fixture.prompt,
                true,
                &RetryConfig::default(),
                Some(budgets.max_total_tokens),
                Vec::<Message>::new(),
                #[cfg(feature = "hooks")]
                None,
            )
            .await
            .unwrap_or_else(|error| panic!("{} {arm} run failed: {error:#}", task.name));

            assert_eq!(response, "done", "{} {arm} terminal response", task.name);
            let provider_turns = model.requests().len();
            let tool_calls = count_tool_calls(&interactions);
            assert!(
                provider_turns <= budgets.max_provider_turns,
                "{} {arm} used {provider_turns} provider turns (max {})",
                task.name,
                budgets.max_provider_turns
            );
            assert!(
                tool_calls <= budgets.max_tool_calls,
                "{} {arm} used {tool_calls} tool calls (max {})",
                task.name,
                budgets.max_tool_calls
            );
            assert!(
                usage.total_tokens <= budgets.max_total_tokens,
                "{} {arm} used {} tokens (max {})",
                task.name,
                usage.total_tokens,
                budgets.max_total_tokens
            );

            let produced = collect_files(directory.path());
            let passed = produced == fixture.oracle.expected_files;
            TaskOutcomeRecorder::new(dispatcher.clone(), context, false).record_oracle(
                &fixture.oracle.id,
                passed,
                1,
            );
            assert!(
                passed,
                "{} {arm} oracle: produced {produced:?}, expected {:?}",
                task.name, fixture.oracle.expected_files
            );
            measurements.push(GymArmMeasurement {
                arm,
                turn_id,
                skill_id,
                provider_turns,
                tool_calls,
                total_tokens: usage.total_tokens,
            });
        }
        runs.push((task.name.clone(), measurements));
    }

    let expected_outcomes = i64::try_from(runs.len() * 2).unwrap();
    // One learned-skill call per library arm and none from the no-library arms,
    // so exactly one outcome per task carries a skill attribution. A link only
    // exists if the `invoked` event was already durable when the oracle outcome
    // was ingested, so this also pins the telemetry ordering.
    let expected_links = i64::try_from(runs.len()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let store = SkillStore::open_at(&paths).unwrap();
        let outcomes: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM skill_task_outcomes WHERE production = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let links: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM skill_task_outcome_links", [], |row| {
                row.get(0)
            })
            .unwrap();
        if outcomes == expected_outcomes && links == expected_links {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gym telemetry was not durably ordered: outcomes {outcomes}/{expected_outcomes}, \
             links {links}/{expected_links}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let store = SkillStore::open_at(&paths).unwrap();
    // Every `invoked` event recorded in this arm's turn, whichever seed it
    // names: a library arm that reached a *different* seed than the fixture
    // declares fails the per-skill count below while still counting here.
    fn round_trips(store: &SkillStore, turn_id: &str) -> usize {
        let count: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM skill_events
                 WHERE event_kind = 'invoked' AND turn_id = ?",
                [turn_id],
                |row| row.get(0),
            )
            .unwrap();
        usize::try_from(count).unwrap()
    }
    fn round_trips_for(store: &SkillStore, skill_id: &str, turn_id: &str) -> usize {
        let count: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM skill_events
                 WHERE event_kind = 'invoked' AND skill_id = ? AND turn_id = ?",
                [skill_id, turn_id],
                |row| row.get(0),
            )
            .unwrap();
        usize::try_from(count).unwrap()
    }
    fn attributed(store: &SkillStore, skill_id: &str, turn_id: &str) -> usize {
        let count: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM skill_task_outcome_links AS link
                 JOIN skill_task_outcomes AS outcome
                   ON outcome.evidence_id = link.evidence_id
                 WHERE outcome.turn_id = ? AND link.skill_id = ?",
                [turn_id, skill_id],
                |row| row.get(0),
            )
            .unwrap();
        usize::try_from(count).unwrap()
    }

    for (name, arms) in &runs {
        assert_eq!(arms.len(), 2, "{name} arms");
        let none = &arms[0];
        let library = &arms[1];
        assert_eq!(none.arm, "none", "{name} first arm");
        assert_eq!(library.arm, "library", "{name} second arm");
        assert!(
            none.skill_id.is_none(),
            "{name} no-library arm was given a skill bundle"
        );
        let library_skill = library
            .skill_id
            .as_deref()
            .unwrap_or_else(|| panic!("{name} library arm resolved no seed"));
        let none_round_trips = round_trips(&store, &none.turn_id);
        let library_round_trips = round_trips(&store, &library.turn_id);
        assert_eq!(
            none_round_trips, 0,
            "{name} no-library arm invoked a learned skill"
        );
        assert_eq!(
            library_round_trips, 1,
            "{name} library arm must invoke exactly one learned skill"
        );
        assert_eq!(
            round_trips_for(&store, library_skill, &library.turn_id),
            1,
            "{name} library arm must invoke the seed it retrieved ({library_skill})"
        );
        assert_eq!(
            attributed(&store, library_skill, &library.turn_id),
            1,
            "{name} oracle outcome must be attributed to {library_skill}"
        );
        assert_eq!(
            (none.provider_turns, none.tool_calls, none.total_tokens),
            (
                library.provider_turns,
                library.tool_calls,
                library.total_tokens
            ),
            "{name} library invocation regression"
        );
        for (arm, js_round_trips) in [(none, none_round_trips), (library, library_round_trips)] {
            println!(
                "HARNESS_EVAL {}",
                serde_json::json!({
                    "name": name,
                    "library": arm.arm,
                    "skill_id": &arm.skill_id,
                    "success": true,
                    "provider_turns": arm.provider_turns,
                    "tool_calls": arm.tool_calls,
                    "total_tokens": arm.total_tokens,
                    "js_round_trips": js_round_trips,
                    "production": false,
                })
            );
        }
    }

    // The suite must spread across the seeded library rather than hammering one
    // artifact: a fixture set that collapses back onto a single export fails
    // here even if every per-task assertion above still passes.
    let distinct_attributed: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(DISTINCT skill_id) FROM skill_task_outcome_links",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        distinct_attributed >= 4,
        "gym outcomes were attributed to only {distinct_attributed} seeded skills"
    );
}

/// Drives `TaskOutcome` emission through the real completion-verification gate.
///
/// Two runs go through `run_print_with_verification` with a
/// `TaskOutcomeRecorder` attached, each with its own turn context so the rows
/// can be counted by `turn_id`:
///
/// * a run whose `Config` carries a verify command, whose scripted model calls
///   the mutating `write` tool (so the gate arms), must leave exactly one
///   `skill_task_outcomes` row of kind `verify_command` whose `source_id` is
///   the SHA-256 of the *configured* command and whose `verify_passed` bit is
///   set;
/// * an otherwise identical run with no verify command configured must leave
///   exactly one row of kind `no_verify_command` with a null `source_id`.
///
/// Unix-only for the same reason as the runner's own verification tests: the
/// verify command is POSIX shell text.
#[cfg(all(unix, feature = "skills", feature = "sandbox"))]
#[tokio::test]
async fn completion_verification_gate_records_both_task_outcome_source_kinds() {
    use crate::agent::runner::{
        CompletionVerification, TaskOutcomeRecorder, run_print_with_verification,
    };
    use crate::extras::js::skills::store::SkillStore;
    use crate::extras::js::skills::telemetry::TelemetryDispatcher;
    use crate::extras::js::skills::turn::{SkillTurnContext, TurnSkillBundle};
    use sha2::Digest;

    // No leading or trailing whitespace: `from_config` trims before hashing, so
    // the hash below has to be taken over the same bytes the gate hashes.
    const VERIFY_COMMAND: &str = "printf 'gate ran'";

    let state = EvalDirectory::new();
    let paths = crate::paths::AppPaths {
        config_dir: state.path().join("config"),
        data_dir: state.path().join("data"),
        local_data_dir: state.path().join("local"),
        state_dir: state.path().join("state"),
        cache_dir: state.path().join("cache"),
        credentials_dir: state.path().join("credentials"),
        project_dir: None,
    };
    let dispatcher = Arc::new(TelemetryDispatcher::spawn(&paths).unwrap());
    let sandbox = Sandbox::new(false, "gym-verify");

    let mut turn_ids = Vec::new();
    for configured_command in [Some(VERIFY_COMMAND), None] {
        let directory = EvalDirectory::new();
        let context = Arc::new(SkillTurnContext::new(TurnSkillBundle::empty("verify-gate")));
        turn_ids.push(context.snapshot().turn_id.clone());
        let cfg = crate::config::Config {
            verify_command: configured_command.map(|command| command.into()),
            verify_timeout_secs: Some(30),
            verify_max_attempts: Some(1),
            ..Default::default()
        };
        let verification = CompletionVerification::with_task_outcomes(
            CompletionVerification::from_config(&cfg, sandbox.clone()),
            &cfg,
            sandbox.clone(),
            TaskOutcomeRecorder::new(dispatcher.clone(), context, false),
        );
        // `write` is a mutating tool, so both runs arm the gate; the only
        // difference between them is whether a command is configured.
        let model = MockCompletionModel::from_stream_turns(vec![
            tool_turn(
                "gate-write",
                "write",
                serde_json::json!({ "path": "verified.txt", "content": "ok\n" }),
            ),
            done_turn(),
        ]);
        let write = Box::new(
            WriteTool::new(None, None, None).with_workspace(directory.path().to_path_buf()),
        ) as Box<dyn ToolDyn>;
        let agent = AgentBuilder::new(model)
            .tools(vec![write])
            .default_max_turns(2)
            .build();
        let (response, _, interactions) = run_print_with_verification(
            &agent,
            "write the file and finish",
            true,
            false,
            &RetryConfig::default(),
            None,
            Vec::<Message>::new(),
            Some(verification),
            #[cfg(feature = "hooks")]
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("verification-gate run failed: {error:#}"));
        assert_eq!(response, "done", "verification-gate terminal response");
        assert_eq!(
            count_tool_calls(&interactions),
            1,
            "the gate must be armed by a real mutating tool call"
        );
        assert!(
            directory.path().join("verified.txt").is_file(),
            "the scripted write must have reached the workspace"
        );
    }
    assert_eq!(turn_ids.len(), 2, "one turn per verification-gate run");
    let verify_turn = turn_ids[0].clone();
    let no_verify_turn = turn_ids[1].clone();
    assert_ne!(
        verify_turn, no_verify_turn,
        "each run needs its own turn so the rows can be told apart"
    );

    let expected_source_id =
        crate::hex::encode_lower(sha2::Sha256::digest(VERIFY_COMMAND.as_bytes()));
    fn rows_for(store: &SkillStore, turn_id: &str) -> Vec<(String, Option<String>, i64)> {
        let mut statement = store
            .conn()
            .prepare(
                "SELECT source_kind, source_id, verify_passed
                   FROM skill_task_outcomes
                  WHERE turn_id = ?
                  ORDER BY source_kind",
            )
            .unwrap();
        statement
            .query_map([turn_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let (verify_rows, no_verify_rows) = loop {
        let store = SkillStore::open_at(&paths).unwrap();
        let verify_rows = rows_for(&store, &verify_turn);
        let no_verify_rows = rows_for(&store, &no_verify_turn);
        if verify_rows.len() == 1 && no_verify_rows.len() == 1 {
            break (verify_rows, no_verify_rows);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "verification-gate outcomes were never durable: verify {verify_rows:?}, \
             no-verify {no_verify_rows:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };

    assert_eq!(
        verify_rows[0].0, "verify_command",
        "the armed gate must record a verify_command outcome"
    );
    assert_eq!(
        verify_rows[0].1.as_deref(),
        Some(expected_source_id.as_str()),
        "verify_command source_id must be the SHA-256 of the configured command"
    );
    assert_eq!(
        verify_rows[0].2, 1,
        "the verify command succeeded, so the outcome must record a pass"
    );
    assert_eq!(
        no_verify_rows[0].0, "no_verify_command",
        "a run with no verify command must record a no_verify_command outcome"
    );
    assert_eq!(
        no_verify_rows[0].1, None,
        "no_verify_command carries no source_id"
    );
}

#[tokio::test]
#[ignore = "deterministic task-level harness regression eval; run by scheduled CI"]
async fn harness_regression_eval() {
    let mut all_metrics = Vec::new();
    for fixture in parse_fixtures() {
        let directory = EvalDirectory::new();
        write_fixture(directory.path(), &fixture);
        let workspace = Arc::new(
            crate::paths::WorkspaceBinding::capture(directory.path())
                .expect("capture harness workspace"),
        );
        let (model, tools, history) = scripted_case(&fixture, workspace);
        let agent = AgentBuilder::new(model.clone())
            .tools(tools)
            .default_max_turns(fixture.max_provider_turns)
            .build();

        let (response, usage, interactions) = run_print(
            &agent,
            &fixture.prompt,
            true,
            &RetryConfig::default(),
            Some(fixture.max_total_tokens),
            history,
            #[cfg(feature = "hooks")]
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("{} failed: {error:#}", fixture.name));

        assert_eq!(response, "done", "{} terminal response", fixture.name);
        let provider_turns = model.requests().len();
        let tool_calls = count_tool_calls(&interactions);
        assert!(
            provider_turns <= fixture.max_provider_turns,
            "{} used {provider_turns} provider turns (max {})",
            fixture.name,
            fixture.max_provider_turns
        );
        assert!(
            tool_calls <= fixture.max_tool_calls,
            "{} used {tool_calls} tool calls (max {})",
            fixture.name,
            fixture.max_tool_calls
        );
        assert!(
            usage.total_tokens <= fixture.max_total_tokens,
            "{} used {} tokens (max {})",
            fixture.name,
            usage.total_tokens,
            fixture.max_total_tokens
        );
        assert_eq!(
            collect_files(directory.path()),
            fixture.expected_files,
            "{} workspace differs; the task failed, rolled back, or mutated an unexpected file",
            fixture.name
        );

        if fixture.name == "subagent_fanout" {
            let result = tool_result_text(&interactions, "fanout");
            let frontend = result.find("frontend: ui clean").expect("frontend result");
            let backend = result.find("backend: api stable").expect("backend result");
            assert!(
                frontend < backend,
                "fan-out results must retain prompt order"
            );
        }

        if fixture.name == "persona_invocation" {
            let args = tool_call_args(&interactions, "task");
            assert_eq!(args["agent_type"], "rust-security-review");
            assert!(tool_result_text(&interactions, "persona").contains("pass"));
        }

        if fixture.name == "compaction_mid_task" {
            let first_request = &model.requests()[0];
            assert!(first_request.chat_history.iter().any(|message| {
                matches!(message, Message::Assistant { content, .. }
                    if content.iter().any(|item| matches!(item, AssistantContent::Text(text)
                        if text.text.contains("[Recap of my prior work in this conversation]"))))
            }));
            assert!(first_request.chat_history.iter().any(|message| {
                matches!(message, Message::User { content }
                    if content.iter().any(|item| matches!(item, UserContent::Text(text)
                        if text.text.contains("Preserve the decision"))))
            }));
        }

        let metrics = Metrics {
            name: fixture.name,
            provider_turns,
            tool_calls,
            total_tokens: usage.total_tokens,
        };
        println!(
            "HARNESS_EVAL {}",
            serde_json::json!({
                "name": metrics.name,
                "success": true,
                "provider_turns": metrics.provider_turns,
                "tool_calls": metrics.tool_calls,
                "total_tokens": metrics.total_tokens,
            })
        );
        all_metrics.push(metrics);
    }

    // Measured, not counted: comparing `all_metrics.len()` with `FIXTURES.len()`
    // would only restate this loop's own input. Every fixture must instead have
    // driven a real provider turn, a real tool call and real token usage, so a
    // harness that silently stopped exercising the agent fails here.
    let names = all_metrics
        .iter()
        .map(|metrics| metrics.name.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        names.len(),
        all_metrics.len(),
        "each fixture must report exactly one metrics row"
    );
    for metrics in &all_metrics {
        assert!(
            metrics.provider_turns > 0 && metrics.tool_calls > 0 && metrics.total_tokens > 0,
            "{} recorded no measured work: {metrics:?}",
            metrics.name
        );
    }
}

#[tokio::test]
#[ignore = "deterministic persona contract regression eval; run by scheduled CI"]
async fn persona_regression_eval() {
    let fixture: PersonaFixture =
        serde_json::from_str(PERSONA_FIXTURE).expect("valid persona harness fixture");
    // A fixture-file shape guard, not a claim about the persona set: each case
    // is separately proven to name a persona that really resolves, below.
    assert_eq!(
        fixture.cases.len(),
        9,
        "the persona fixture must keep all nine cases"
    );
    let names = fixture
        .cases
        .iter()
        .map(|case| case.agent_type.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(names.len(), fixture.cases.len(), "persona cases are unique");

    let directory = EvalDirectory::new();
    for (relative, content) in &fixture.repository_files {
        let target = directory.path().join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("create persona fixture directory");
        }
        std::fs::write(target, content).expect("write persona fixture file");
    }
    let workspace = Arc::new(
        crate::paths::WorkspaceBinding::capture(directory.path())
            .expect("capture persona harness workspace"),
    );

    for case in fixture.cases {
        assert!(
            crate::context::agents::lookup_for_workspace(&case.agent_type, Some(&workspace))
                .is_some(),
            "persona {} must resolve",
            case.agent_type
        );
        let report = run_scripted_task_for_eval(
            TaskArgs {
                prompts: vec![case.prompt],
                briefs: None,
                agent_type: Some(case.agent_type.clone()),
            },
            workspace.clone(),
            vec![case.response],
        )
        .await
        .unwrap_or_else(|error| panic!("{} persona eval failed: {error}", case.agent_type));

        let findings = report.find("## Findings").expect("Findings section");
        let unverified = report.find("## Unverified").expect("Unverified section");
        let coverage = report.find("## Coverage").expect("Coverage section");
        assert!(findings < unverified && unverified < coverage);
        assert!(report.contains("[confidence:"));
        assert!(report.contains("- Covered:"));
        assert!(report.contains("- Skipped:"));
        assert!(
            report.contains(&case.expected_finding),
            "{} omitted expected finding {:?}",
            case.agent_type,
            case.expected_finding
        );
        println!(
            "PERSONA_EVAL {}",
            serde_json::json!({
                "agent_type": case.agent_type,
                "success": true,
                "expected_finding": case.expected_finding,
            })
        );
    }
}

// ── Partial headless turns survive a terminal failure ──────────────────

/// A run that completes a real file write and then fails must still hand the
/// caller the completed tool records and the usage those turns incurred, so a
/// resumed session has a durable record of effects that already happened.
#[tokio::test]
async fn a_failed_headless_turn_retains_completed_tool_records_and_usage() {
    let directory = EvalDirectory::new();
    let model = MockCompletionModel::from_stream_turns(vec![
        tool_turn(
            "partial-write",
            "write",
            serde_json::json!({ "path": "effect.txt", "content": "written\n" }),
        ),
        // The provider then fails non-retryably after the effect happened.
        vec![MockStreamEvent::Error(
            rig::test_utils::MockError::provider("invalid_request_error: bad request"),
        )],
    ]);
    let write =
        Box::new(WriteTool::new(None, None, None).with_workspace(directory.path().to_path_buf()))
            as Box<dyn ToolDyn>;
    let agent = AgentBuilder::new(model)
        .tools(vec![write])
        .default_max_turns(4)
        .build();

    let turn = crate::agent::runner::run_print_with_verification(
        &agent,
        "write the file",
        true,
        false,
        &RetryConfig::default(),
        None,
        Vec::<Message>::new(),
        None,
        #[cfg(feature = "hooks")]
        None,
    )
    .await;

    assert!(
        turn.failure.is_some(),
        "a non-retryable provider failure must not become a successful turn"
    );
    assert!(
        directory.path().join("effect.txt").is_file(),
        "the scripted write must have reached the workspace"
    );
    assert_eq!(
        count_tool_calls(&turn.interactions),
        1,
        "the completed tool call must survive the failure: {:?}",
        turn.interactions
    );
    assert!(
        turn.usage.input_tokens > 0,
        "usage observed before the failure must survive it"
    );

    // The retained records persist and reload exactly like a successful turn.
    let mut session = crate::session::Session::new("openai", "gpt-4", 128_000, "");
    crate::print::persist_headless_turn(
        &mut session,
        "write the file",
        &turn.response,
        &turn.interactions,
    );
    assert!(
        session
            .messages
            .iter()
            .any(|message| message.role == crate::session::MessageRole::ToolResult),
        "the failed turn's tool record must be persisted"
    );
    assert!(
        session
            .messages
            .iter()
            .any(|message| message.content.contains("write the file")),
        "the prompt must be persisted with the failed turn"
    );
}
