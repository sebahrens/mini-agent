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

#[cfg(all(feature = "skills", feature = "sandbox"))]
#[derive(Clone, Debug, Deserialize)]
struct GymDefaults {
    prompt: String,
    initial_files: BTreeMap<PathBuf, String>,
    oracle: GymOracle,
    budgets: GymBudgets,
    scripted_provider_turns: BTreeMap<String, Vec<String>>,
    library: String,
}

#[cfg(all(feature = "skills", feature = "sandbox"))]
#[derive(Clone, Debug, Deserialize)]
struct GymOracle {
    expected_files: BTreeMap<PathBuf, String>,
    id: String,
}

#[cfg(all(feature = "skills", feature = "sandbox"))]
#[derive(Clone, Debug, Deserialize)]
struct GymBudgets {
    max_provider_turns: usize,
    max_tool_calls: usize,
    max_total_tokens: u64,
}

#[cfg(all(feature = "skills", feature = "sandbox"))]
#[derive(Debug, Deserialize)]
struct GymTaskFile {
    defaults: GymDefaults,
    tasks: Vec<GymTask>,
}

#[cfg(all(feature = "skills", feature = "sandbox"))]
#[derive(Debug, Deserialize)]
struct GymTask {
    name: String,
    tags: Vec<String>,
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
    assert_eq!(fixtures.len(), 5);
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

#[cfg(all(feature = "skills", feature = "sandbox"))]
#[derive(Debug)]
struct GymArmMeasurement {
    arm: &'static str,
    turn_id: String,
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
/// exactly one, and the two arms must agree on the three cost axes. A library
/// arm that silently fell back to hand-written JavaScript emits no `invoked`
/// event and fails the gate.
///
/// Deliberately *not* covered here (tracked separately): the library is seeded
/// straight into the store instead of through operator admission
/// (mini-agent-4q06), and every task shares one `defaults` entry
/// (mini-agent-vmfi).
#[cfg(all(feature = "skills", feature = "sandbox"))]
#[tokio::test]
async fn task_json_library_axis_uses_real_store_and_records_oracles() {
    use crate::agent::runner::TaskOutcomeRecorder;
    use crate::extras::js::skills::index::RetrievalPolicy;
    use crate::extras::js::skills::store::SkillStore;
    use crate::extras::js::skills::telemetry::TelemetryDispatcher;
    use crate::extras::js::skills::turn::{SkillRuntime, SkillTurnContext, TurnSkillBundle};
    use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
    use crate::extras::skills::index::AgentSkillSearchPolicy;

    let specification: GymTaskFile = serde_json::from_str(GYM_TASKS).unwrap();
    assert_eq!(specification.tasks.len(), 20);
    // Budgets are not asserted as constants here: they are handed to the agent
    // as the real turn/token caps below and then compared against the measured
    // cost of each arm.
    let budgets = &specification.defaults.budgets;

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
    let skill = SkillArtifact::new(
        "function gymNormalize(_cap, text) { return text.trim(); }".into(),
        "Normalize gym text by trimming surrounding whitespace.".into(),
        vec!["normalize".into(), "gym".into(), "text".into()],
        vec![SkillExport {
            name: "gymNormalize".into(),
            signature: "(text: string) => string".into(),
        }],
        vec!["gymNormalize(' x ') === 'x'".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    // Dispatch on the fixture's library axis rather than asserting its value:
    // an unrecognised axis must fail loudly instead of silently seeding a
    // library the fixture never asked for.
    match specification.defaults.library.as_str() {
        "seeds" => {
            SkillStore::open_at(&paths)
                .and_then(|mut store| store.insert_verified(&skill))
                .unwrap();
        }
        other => panic!("unsupported gym library axis {other:?}"),
    }
    let runtime = SkillRuntime::open(&paths, None)
        .unwrap()
        .with_test_policies(
            RetrievalPolicy {
                dense_score_floor: -1.0,
                lexical_score_floor: -1.0,
                ..RetrievalPolicy::default()
            },
            AgentSkillSearchPolicy::default(),
        );
    runtime.settle_learned_rebuild_for_test().await;
    let dispatcher = Arc::new(TelemetryDispatcher::spawn(&paths).unwrap());

    let mut runs: Vec<(String, Vec<GymArmMeasurement>)> = Vec::new();
    for task in &specification.tasks {
        assert!(!task.tags.is_empty(), "{} tags", task.name);
        let mut measurements: Vec<GymArmMeasurement> = Vec::new();
        for arm in ["none", "library"] {
            let directory = EvalDirectory::new();
            for (relative, content) in &specification.defaults.initial_files {
                let target = directory.path().join(relative);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(target, content.as_bytes()).unwrap();
            }
            let context = if arm == "library" {
                let query = format!("{} {}", specification.defaults.prompt, task.tags.join(" "));
                let discovery = runtime.prepare_turn(&query).await;
                assert!(
                    discovery
                        .learned_js
                        .skills
                        .iter()
                        .any(|item| item.id == skill.id),
                    "{} must retrieve the seeded library",
                    task.name
                );
                runtime.turn_context()
            } else {
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
            let scripts = &specification.defaults.scripted_provider_turns[arm];
            assert_eq!(scripts.len(), 1, "{} {arm} scripted turns", task.name);

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
                &specification.defaults.prompt,
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

            let passed =
                collect_files(directory.path()) == specification.defaults.oracle.expected_files;
            TaskOutcomeRecorder::new(dispatcher.clone(), context, false).record_oracle(
                &specification.defaults.oracle.id,
                passed,
                1,
            );
            assert!(passed, "{} {arm} oracle", task.name);
            measurements.push(GymArmMeasurement {
                arm,
                turn_id,
                provider_turns,
                tool_calls,
                total_tokens: usage.total_tokens,
            });
        }
        runs.push((task.name.clone(), measurements));
    }

    let expected_outcomes = i64::try_from(runs.len() * 2).unwrap();
    let expected_links = i64::try_from(runs.len()).unwrap();
    // One `gymNormalize` call per library arm, and none from the no-library
    // arms, so the seeded skill must show exactly one `invoked` event per task.
    let expected_invocations = expected_links;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
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
            .query_row(
                "SELECT COUNT(*) FROM skill_task_outcome_links WHERE skill_id = ?",
                [&skill.id],
                |row| row.get(0),
            )
            .unwrap();
        let invocations: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM skill_events
                 WHERE event_kind = 'invoked' AND skill_id = ?",
                [&skill.id],
                |row| row.get(0),
            )
            .unwrap();
        if outcomes == expected_outcomes
            && links == expected_links
            && invocations == expected_invocations
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gym telemetry was not durably ordered: outcomes {outcomes}/{expected_outcomes}, \
             links {links}/{expected_links}, invocations {invocations}/{expected_invocations}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let store = SkillStore::open_at(&paths).unwrap();
    let round_trips = |turn_id: &str| -> usize {
        let count: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM skill_events
                 WHERE event_kind = 'invoked' AND skill_id = ? AND turn_id = ?",
                [skill.id.as_str(), turn_id],
                |row| row.get(0),
            )
            .unwrap();
        usize::try_from(count).unwrap()
    };

    for (name, arms) in &runs {
        assert_eq!(arms.len(), 2, "{name} arms");
        let none = &arms[0];
        let library = &arms[1];
        assert_eq!(none.arm, "none", "{name} first arm");
        assert_eq!(library.arm, "library", "{name} second arm");
        let none_round_trips = round_trips(&none.turn_id);
        let library_round_trips = round_trips(&library.turn_id);
        assert_eq!(
            none_round_trips, 0,
            "{name} no-library arm invoked a learned skill"
        );
        assert_eq!(
            library_round_trips, 1,
            "{name} library arm must invoke the seeded skill exactly once"
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

    assert_eq!(all_metrics.len(), FIXTURES.len());
}

#[tokio::test]
#[ignore = "deterministic persona contract regression eval; run by scheduled CI"]
async fn persona_regression_eval() {
    let fixture: PersonaFixture =
        serde_json::from_str(PERSONA_FIXTURE).expect("valid persona harness fixture");
    assert_eq!(fixture.cases.len(), 9, "one case per shipped persona");
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
