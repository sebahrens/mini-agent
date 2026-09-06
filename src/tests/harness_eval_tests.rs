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
