//! Evidence-handle axis of the deterministic harness eval (mini-agent-g442v).
//!
//! Unlike `run_scripted_task_for_eval`, every case here goes through the real
//! `TaskTool::call` executor: the child is a real explore agent whose provider
//! is a scripted OpenAI-compatible loopback server, so its tool calls run the
//! real `read`/`grep` tools, the host really records (or does not record)
//! evidence, validates citations, runs the repair turn and renders the
//! `## Evidence` section. Each case runs with `task_evidence` on and off.
//!
//! The parent is a fixed, deterministic verification policy (the same for
//! every case, never told which case it is in): it takes the child's
//! `ANSWER:` claim and, when the host offers evidence, dereferences cited
//! records through the real parent-only `task_evidence` tool and prefers what
//! the recorded tool output says. Accuracy is the task-level verdict of that
//! answer against an oracle recomputed from the generated workspace; cost is
//! the bytes and estimated tokens that entered the parent's context (the task
//! output plus every `task_evidence` call and result). The policy stands in
//! for a careful parent model, so the eval measures what information each arm
//! makes available and what it costs, not how a particular model behaves.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rig::tool::Tool;
use serde::Deserialize;

use crate::extras::subagents::background::{EvidenceArgs, EvidenceTool, Pool};
use crate::extras::subagents::task_tool::TaskTool;

const EVIDENCE_FIXTURE: &str = include_str!("../../../tests/harness_eval/evidence/fixture.json");
/// Default generator seed; `HARNESS_EVAL_SEED` overrides it for drift runs.
const DEFAULT_SEED: u64 = 0x6d69_6e69_6167_656e;
/// Probing budget of the parent policy: at most this many records per case.
const MAX_DEREFERENCES: usize = 4;
/// Pages the parent follows for one record before calling it incomplete.
const MAX_PAGES: usize = 4;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceFixture {
    #[allow(dead_code)]
    about: String,
    cases: Vec<EvidenceCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EvidenceCase {
    pub(super) name: String,
    values: BTreeMap<String, [u64; 2]>,
    #[serde(default)]
    files: BTreeMap<PathBuf, String>,
    #[serde(default)]
    images: BTreeMap<PathBuf, [String; 2]>,
    question: Question,
    parent: ParentKind,
    turns: Vec<ScriptTurn>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Question {
    Value { subject: String, key: String },
    Count { subject: String, needle: String },
    Image { subject: String },
}

impl Question {
    fn subject(&self) -> &str {
        match self {
            Self::Value { subject, .. } | Self::Count { subject, .. } | Self::Image { subject } => {
                subject
            }
        }
    }

    fn prompt(&self) -> String {
        let ask = match self {
            Self::Value { subject, key } => format!("What is `{key}` in {subject}?"),
            Self::Count { subject, needle } => {
                format!("How many lines of {subject} contain `{needle}`?")
            }
            Self::Image { subject } => format!("What are the pixel dimensions of {subject}?"),
        };
        format!("{ask} State the result in Findings as `ANSWER: <value>`.")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ParentKind {
    Vision,
    Text,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptTurn {
    #[serde(default)]
    tools: Vec<ScriptCall>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptCall {
    name: String,
    args: serde_json::Value,
}

/// SplitMix64: a tiny seeded generator, so no dependency is needed.
struct Generator(u64);

impl Generator {
    fn for_case(seed: u64, name: &str) -> Self {
        // FNV-1a of the case name, mixed with the run seed.
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in name.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self(seed ^ hash)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn range(&mut self, low: u64, high: u64) -> u64 {
        low + self.next() % (high - low + 1)
    }
}

/// Draw every named value of a case, pairwise distinct, so a wrong claim can
/// never coincide with the truth by accident.
fn draw_values(case: &EvidenceCase, seed: u64) -> BTreeMap<String, u64> {
    let mut generator = Generator::for_case(seed, &case.name);
    let mut drawn = BTreeMap::new();
    for (name, [low, high]) in &case.values {
        let value = loop {
            let candidate = generator.range(*low, *high);
            if !drawn.values().any(|value| *value == candidate) {
                break candidate;
            }
        };
        drawn.insert(name.clone(), value);
    }
    drawn
}

/// Expand `{{name}}` and `{{rep:name:TEXT}}` (TEXT repeated `name` times, one
/// line each, `#` replaced by the 1-based line index).
fn expand(template: &str, values: &BTreeMap<String, u64>) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let end = rest[start..].find("}}").expect("closed placeholder") + start;
        let token = &rest[start + 2..end];
        if let Some(spec) = token.strip_prefix("rep:") {
            let (name, text) = spec.split_once(':').expect("rep:name:text");
            let count = values[name];
            for index in 1..=count {
                out.push_str(&text.replace('#', &index.to_string()));
                out.push('\n');
            }
        } else {
            out.push_str(&values[token].to_string());
        }
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    out
}

/// A header-only PNG (signature, CRC-checked IHDR, IEND) of the given size:
/// enough for the host's header inspection, which never inflates pixels.
fn png_of(width: u32, height: u32) -> Vec<u8> {
    fn chunk(kind: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crate::agent::tools::crc::crc32(&body).to_be_bytes());
        out
    }
    let mut ihdr = width.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend(chunk(b"IHDR", &ihdr));
    // A stored-size stand-in for pixel data, so encoded size is realistic.
    png.extend(chunk(b"tEXt", &vec![b'a'; 12 * 1024]));
    png.extend(chunk(b"IEND", &[]));
    png
}

/// The ground truth, recomputed from the generated workspace on disk.
fn oracle(question: &Question, root: &std::path::Path) -> String {
    let path = root.join(question.subject());
    match question {
        Question::Value { key, .. } => {
            extract_value(&std::fs::read_to_string(path).unwrap(), key).expect("oracle value")
        }
        Question::Count { needle, .. } => std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|line| line.contains(needle.as_str()))
            .count()
            .to_string(),
        Question::Image { .. } => {
            let bytes = std::fs::read(path).unwrap();
            let info = crate::agent::tools::read_image::inspect(&bytes)
                .unwrap()
                .unwrap();
            format!("{}x{}", info.width, info.height)
        }
    }
}

fn answer_token(text: &str) -> Option<String> {
    let token: String = text
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        .collect();
    (!token.is_empty()).then_some(token)
}

/// `key = value` / `key: value` / `KEY=value`, also behind `read` line numbers.
fn extract_value(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let Some(position) = line.find(key) else {
            continue;
        };
        let before = line[..position].chars().next_back();
        if before.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
            continue;
        }
        let after = line[position + key.len()..].trim_start();
        if let Some(value) = after
            .strip_prefix('=')
            .or_else(|| after.strip_prefix(':'))
            .and_then(|value| answer_token(value.trim_start().trim_start_matches('"')))
        {
            return Some(value);
        }
    }
    None
}

/// The scripted child provider: an OpenAI-compatible streaming endpoint whose
/// reply depends only on how many assistant turns the request already holds.
struct Loopback {
    address: std::net::SocketAddr,
    requests: Arc<Mutex<usize>>,
    bodies: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(turns: Vec<ScriptTurn>) -> Loopback {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(0usize));
    let counter = requests.clone();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let captured = bodies.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0; 8192];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0 && bytes.len() + count <= 64 * 1024);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let length = std::str::from_utf8(&bytes[..header_end])
                .unwrap()
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                })
                .unwrap();
            assert!(length <= 16 * 1024 * 1024);
            while bytes.len() < header_end + length {
                let mut chunk = [0; 65536];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
            *counter.lock().unwrap() += 1;
            captured.lock().unwrap().push(body.to_string());
            let turn_index = body["messages"]
                .as_array()
                .map(|messages| {
                    messages
                        .iter()
                        .filter(|message| message["role"] == "assistant")
                        .count()
                })
                .unwrap_or(0);
            let turn = turns
                .get(turn_index)
                .or_else(|| turns.iter().rev().find(|turn| turn.text.is_some()))
                .cloned()
                .unwrap();
            let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                serde_json::json!({"id":"eval","object":"chat.completion.chunk","created":0,
                    "model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
            };
            let (first, finish) = if turn.tools.is_empty() {
                (
                    chunk(
                        serde_json::json!({"role":"assistant","content":turn.text.unwrap_or_default()}),
                        serde_json::Value::Null,
                    ),
                    "stop",
                )
            } else {
                let calls: Vec<_> = turn
                    .tools
                    .iter()
                    .enumerate()
                    .map(|(index, call)| {
                        serde_json::json!({"index": index,
                            "id": format!("call_{turn_index}_{index}"), "type": "function",
                            "function": {"name": call.name,
                                "arguments": call.args.to_string()}})
                    })
                    .collect();
                (
                    chunk(
                        serde_json::json!({"role":"assistant","tool_calls":calls}),
                        serde_json::Value::Null,
                    ),
                    "tool_calls",
                )
            };
            let mut last = chunk(serde_json::json!({}), serde_json::json!(finish));
            last["usage"] =
                serde_json::json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120});
            let response = format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n");
            let _ = socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()
                    )
                    .as_bytes(),
                )
                .await;
            let _ = socket.shutdown().await;
        }
    });
    Loopback {
        address,
        requests,
        bodies,
        task,
    }
}

/// What entered the parent's context, and what the parent concluded.
#[derive(Debug, Default)]
pub(super) struct ParentRun {
    pub(super) answer: Option<String>,
    pub(super) context_bytes: usize,
    pub(super) image_tokens: u64,
    pub(super) evidence_calls: usize,
    pub(super) inline_images: usize,
    pub(super) verified_from_evidence: bool,
}

impl ParentRun {
    pub(super) fn est_tokens(&self) -> u64 {
        (self.context_bytes as u64).div_ceil(4) + self.image_tokens
    }
}

/// The child's claim: the first `ANSWER:` token in the quoted report.
fn claimed_answer(output: &str) -> Option<String> {
    let start = output.find("ANSWER:")? + "ANSWER:".len();
    answer_token(&output[start..])
}

struct EvidenceRow {
    id: String,
    target: String,
}

/// Parse the host `## Evidence` section: its task_id, record total and rows.
fn evidence_section(output: &str) -> Option<(String, usize, Vec<EvidenceRow>)> {
    let section = &output[output.find("\n## Evidence\n")?..];
    let marker = "task_evidence {\"task_id\":\"";
    let task_id = section[section.find(marker)? + marker.len()..]
        .split('"')
        .next()?
        .to_string();
    let total = section
        .split("Host-recorded child tool results: ")
        .nth(1)?
        .split(|ch: char| !ch.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    let rows = section
        .lines()
        .filter_map(|line| {
            let cells: Vec<_> = line.split('|').map(str::trim).collect();
            (cells.len() > 3 && cells[1].starts_with('E') && cells[1] != "id").then(|| {
                EvidenceRow {
                    id: cells[1].to_string(),
                    target: cells[3].to_string(),
                }
            })
        })
        .collect();
    Some((task_id, total, rows))
}

/// One dereferenced record: its header target, every text page, its images.
struct Dereferenced {
    target: String,
    text: String,
    complete: bool,
    images: Vec<(u32, u32)>,
}

async fn dereference(
    tool: &EvidenceTool,
    run: &crate::spend::run::Run,
    task_id: &str,
    id: &str,
    parent: &mut ParentRun,
) -> Option<Dereferenced> {
    let mut text = String::new();
    let mut images = Vec::new();
    let mut target = String::new();
    let mut offset = 1usize;
    for _ in 0..MAX_PAGES {
        let args = serde_json::json!({"task_id": task_id, "id": id, "offset": offset});
        parent.context_bytes += args.to_string().len() + "task_evidence".len();
        parent.evidence_calls += 1;
        let parsed: EvidenceArgs = serde_json::from_value(args).unwrap();
        let output = match crate::spend::run::scope(Some(run.clone()), tool.call(parsed)).await {
            Ok(output) => output,
            Err(error) => {
                parent.context_bytes += error.to_string().len();
                return None;
            }
        };
        let response = match serde_json::from_str::<serde_json::Value>(&output) {
            Ok(envelope) if envelope.get("parts").is_some() => {
                for part in envelope["parts"].as_array().into_iter().flatten() {
                    use base64::Engine;
                    let data = part["data"].as_str().unwrap_or_default();
                    parent.image_tokens +=
                        crate::extras::subagents::image_cost::base64_image_tokens(data);
                    // The scripted parent "looks at" the image by its header.
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data)
                        && let Ok(Some(info)) = crate::agent::tools::read_image::inspect(&bytes)
                    {
                        images.push((info.width, info.height));
                    }
                }
                envelope["response"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            }
            _ => output,
        };
        parent.context_bytes += response.len();
        if target.is_empty()
            && let Some(rest) = response.split(", target ").nth(1)
        {
            target = rest.split("; untrusted").next().unwrap_or_default().into();
        }
        text.push_str(&response);
        text.push('\n');
        match response
            .split("[more: call again with offset=")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .and_then(|next| next.parse().ok())
        {
            Some(next) => offset = next,
            None => {
                return Some(Dereferenced {
                    target,
                    text,
                    complete: true,
                    images,
                });
            }
        }
    }
    Some(Dereferenced {
        target,
        text,
        complete: false,
        images,
    })
}

fn extract_from(question: &Question, record: &Dereferenced) -> Option<String> {
    match question {
        Question::Value { key, .. } => extract_value(&record.text, key),
        // A count is only trusted from a record the parent read to the end.
        Question::Count { needle, .. } => record.complete.then(|| {
            record
                .text
                .lines()
                .filter(|line| line.contains(needle.as_str()))
                .count()
                .to_string()
        }),
        Question::Image { .. } => record
            .images
            .first()
            .map(|(width, height)| format!("{width}x{height}")),
    }
}

/// The fixed parent policy, identical for every case and both arms.
async fn parent_policy(
    output: &str,
    question: &Question,
    tool: &EvidenceTool,
    run: &crate::spend::run::Run,
) -> ParentRun {
    // An inline-image envelope: the text report plus host-attached images.
    let mut inline = Vec::new();
    let mut inline_tokens = 0;
    let text = match serde_json::from_str::<serde_json::Value>(output) {
        Ok(envelope) if envelope.get("parts").is_some() => {
            for part in envelope["parts"].as_array().into_iter().flatten() {
                use base64::Engine;
                let data = part["data"].as_str().unwrap_or_default();
                inline_tokens += crate::extras::subagents::image_cost::base64_image_tokens(data);
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data)
                    && let Ok(Some(info)) = crate::agent::tools::read_image::inspect(&bytes)
                {
                    inline.push((info.width, info.height));
                }
            }
            envelope["response"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        }
        _ => output.to_string(),
    };
    let output = text.as_str();
    let mut parent = ParentRun {
        answer: claimed_answer(output),
        context_bytes: output.len(),
        image_tokens: inline_tokens,
        inline_images: inline.len(),
        ..ParentRun::default()
    };
    // An attached image answers an image question without a round trip.
    if let (Question::Image { .. }, Some((width, height))) = (question, inline.first()) {
        parent.answer = Some(format!("{width}x{height}"));
        parent.verified_from_evidence = true;
        return parent;
    }
    // No evidence on offer, or none cited: the parent takes the report.
    let Some((task_id, total, rows)) = evidence_section(output) else {
        return parent;
    };
    if rows.is_empty() {
        return parent;
    }
    let subject = question.subject();
    // Listed rows about the asked subject first; if none is, the citation
    // points elsewhere, so probe the other records in call order.
    let mut queue: Vec<String> = rows
        .iter()
        .filter(|row| row.target.contains(subject))
        .map(|row| row.id.clone())
        .collect();
    if queue.is_empty() {
        let listed: Vec<_> = rows.iter().map(|row| row.id.as_str()).collect();
        queue = (1..=total)
            .map(|number| format!("E{number}"))
            .filter(|id| !listed.contains(&id.as_str()))
            .collect();
    }
    for id in queue.into_iter().take(MAX_DEREFERENCES) {
        let Some(record) = dereference(tool, run, &task_id, &id, &mut parent).await else {
            continue;
        };
        if !record.target.contains(subject) {
            continue;
        }
        if let Some(answer) = extract_from(question, &record) {
            parent.answer = Some(answer);
            parent.verified_from_evidence = true;
            break;
        }
    }
    parent
}

/// The arm under test.
#[derive(Clone, Copy, Debug)]
pub(super) struct Arm {
    pub(super) evidence: bool,
    /// `[subagents] task_inline_images`.
    pub(super) inline_images: u8,
    /// `task_max_cost_units`, when the default is not wanted.
    pub(super) max_cost_units: Option<u64>,
    /// Identical prompts in one call (fan-out).
    pub(super) prompts: usize,
}

impl Arm {
    pub(super) const fn evidence(evidence: bool) -> Self {
        Self {
            evidence,
            inline_images: 0,
            max_cost_units: None,
            prompts: 1,
        }
    }
}

/// One case under one arm, through the real task executor.
pub(super) struct ArmResult {
    pub(super) oracle: String,
    pub(super) parent: ParentRun,
    pub(super) output: String,
    pub(super) child_requests: usize,
    pub(super) child_tool_calls: usize,
    /// Every child request body, as sent.
    pub(super) bodies: Vec<String>,
}

async fn run_arm(case: &EvidenceCase, values: &BTreeMap<String, u64>, arm: Arm) -> ArmResult {
    let root = crate::tests::OwnedTestDirectory::new("evidence-axis");
    let workspace_root = root.path().join("workspace");
    for (relative, template) in &case.files {
        let path = workspace_root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, expand(template, values)).unwrap();
    }
    for (relative, [width, height]) in &case.images {
        let path = workspace_root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, png_of(values[width] as u32, values[height] as u32)).unwrap();
    }
    std::fs::create_dir_all(&workspace_root).unwrap();
    let oracle = oracle(&case.question, &workspace_root);
    let turns: Vec<ScriptTurn> = case
        .turns
        .iter()
        .map(|turn| ScriptTurn {
            tools: turn
                .tools
                .iter()
                .map(|call| ScriptCall {
                    name: call.name.clone(),
                    args: serde_json::from_str(&expand(&call.args.to_string(), values)).unwrap(),
                })
                .collect(),
            text: turn.text.as_deref().map(|text| expand(text, values)),
        })
        .collect();
    let child_tool_calls = turns.iter().map(|turn| turn.tools.len()).sum();
    let server = serve(turns).await;

    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "MINI_AGENT_DATA_DIR",
            Some(root.path().join("data").into_os_string()),
        ),
        (
            "MINI_AGENT_CONFIG_DIR",
            Some(root.path().join("config").into_os_string()),
        ),
    ]);
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&workspace_root).unwrap());
    let mut config = serde_json::json!({
        "no_context_files": true, "session_title_model": "off",
        "max_tokens": 1024, "subagent_max_tokens": 1024,
        "task_evidence": arm.evidence,
        "retry": {"max_attempts": 1},
        "custom_providers": {"eval-wire": {"provider_type": "openai",
            "base_url": format!("http://{}/v1", server.address), "api_style": "completions"}}
    });
    if arm.inline_images > 0 {
        config["subagents"] = serde_json::json!({"task_inline_images": arm.inline_images});
    }
    if let Some(units) = arm.max_cost_units {
        config["task_max_cost_units"] = serde_json::json!(units);
    }
    let config: crate::config::Config = serde_json::from_value(config).unwrap();
    let client = crate::provider::create_client(
        "eval-wire",
        Some("inert-loopback-key"),
        config.custom_providers_map(),
        None,
    )
    .unwrap();
    let configured = crate::extras::subagents::SubagentConfig {
        client,
        provider_name: "eval-wire".into(),
        session_provider: "eval-wire".into(),
        model_name: "fixture".into(),
        fallback_notice: None,
        api_key: None,
        max_turns: 6,
        config: config.into(),
    };

    let mut session = crate::session::Session::new("openai", "fixture", 32000, "");
    session.working_dir = workspace_root.to_str().unwrap().into();
    session.initialize_spend_account(None).unwrap();
    let pool = Pool::for_session(&mut session, 2).unwrap();
    let run = crate::spend::run::Run::fixture(
        session.spend_account.clone(),
        Arc::new(crate::spend::Ledger::new(&root.path().join("state"))),
        &session.id,
        crate::spend::PriceSnapshot::capture(Some((2.0, 4.0)), Default::default(), false),
    )
    .with_background_pool(&pool);

    let parent_support = match case.parent {
        ParentKind::Vision => crate::agent::tools::read_image::ImageReadSupport::captured(
            "anthropic",
            "claude-fable-5-1",
            false,
        ),
        ParentKind::Text => crate::agent::tools::read_image::ImageReadSupport::captured(
            "openai",
            "gpt-3.5-turbo",
            false,
        ),
    };
    let task = TaskTool::new(None, None, true)
        .with_workspace_binding(workspace.clone())
        .with_parent_image_support(parent_support);
    let prompts: Vec<_> = (0..arm.prompts).map(|_| case.question.prompt()).collect();
    let args = serde_json::from_value(serde_json::json!({"prompts": prompts})).unwrap();
    let scope = crate::agent::runner::AgentWorkScope::new();
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        // Heap each wrapper layer: nested by value, the task-local scopes and
        // the task call compound into megabyte-sized unoptimized poll frames
        // that overflow the 2 MiB test thread.
        Box::pin(crate::spend::run::scope(
            Some(run.clone()),
            Box::pin(
                scope.run(Box::pin(crate::extras::subagents::scope_test_config(
                    configured,
                    Box::pin(task.call(args)),
                ))),
            ),
        )),
    )
    .await;
    cleanup.settle().await;
    let output = output
        .unwrap_or_else(|_| panic!("{} did not settle", case.name))
        .unwrap_or_else(|error| panic!("{} task failed: {error}", case.name));

    let tool = EvidenceTool::new(None, None, workspace.clone(), parent_support);
    let parent = parent_policy(&output, &case.question, &tool, &run).await;
    let child_requests = *server.requests.lock().unwrap();
    let bodies = server.bodies.lock().unwrap().clone();
    drop(pool);
    ArmResult {
        oracle,
        parent,
        output,
        child_requests,
        child_tool_calls,
        bodies,
    }
}

pub(super) fn cases() -> Vec<EvidenceCase> {
    let fixture: EvidenceFixture =
        serde_json::from_str(EVIDENCE_FIXTURE).expect("valid evidence fixture");
    fixture.cases
}

pub(super) fn seed() -> u64 {
    std::env::var("HARNESS_EVAL_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SEED)
}

pub(super) async fn run_case(case: &EvidenceCase, seed: u64, arm: Arm) -> ArmResult {
    let values = draw_values(case, seed);
    Box::pin(run_arm(case, &values, arm)).await
}

impl EvidenceCase {
    pub(super) fn has_images(&self) -> bool {
        !self.images.is_empty()
    }

    pub(super) fn parent(&self) -> ParentKind {
        self.parent
    }
}

#[test]
fn evidence_axis_generator_is_seeded_distinct_and_oracle_independent() {
    let cases = cases();
    assert!(
        cases.len() >= 10,
        "the evidence axis keeps its diverse cases"
    );
    let kinds: std::collections::HashSet<_> = cases
        .iter()
        .map(|case| std::mem::discriminant(&case.question))
        .collect();
    assert_eq!(kinds.len(), 3, "value, count and image questions");
    for case in &cases {
        let first = draw_values(case, 7);
        assert_eq!(first, draw_values(case, 7), "{} is reproducible", case.name);
        let distinct: std::collections::BTreeSet<_> = first.values().collect();
        assert_eq!(distinct.len(), first.len(), "{} values distinct", case.name);
        // The fixture never states a literal answer: every claim is drawn.
        for turn in &case.turns {
            if let Some(text) = &turn.text {
                let claim = text.split("ANSWER:").nth(1).unwrap().trim_start();
                assert!(claim.starts_with("{{"), "{}: literal claim", case.name);
            }
        }
    }
    // Different seeds move the values, so answers cannot be memorised.
    assert!(
        cases
            .iter()
            .any(|case| draw_values(case, 1) != draw_values(case, 2))
    );
    let values = BTreeMap::from([("n".to_string(), 3u64)]);
    assert_eq!(expand("a{{rep:n:x#}}b={{n}}", &values), "ax1\nx2\nx3\nb=3");
    assert_eq!(
        extract_value("  12| retry_limit = 7\n", "retry_limit"),
        Some("7".into())
    );
    assert_eq!(extract_value("MAX_CONNECTIONS=88\n", "CONNECTIONS"), None);
    let png = png_of(640, 480);
    let info = crate::agent::tools::read_image::inspect(&png)
        .unwrap()
        .unwrap();
    assert_eq!((info.width, info.height), (640, 480));
}

// ── Inline image lane (mini-agent-6m0om), end to end through the executor ──

fn lane_case(parent: &str, attach: &str) -> EvidenceCase {
    serde_json::from_value(serde_json::json!({
        "name": "inline_lane",
        "values": {"aw": [640, 640], "ah": [480, 480], "bw": [300, 300], "bh": [200, 200],
                   "cw": [520, 520], "ch": [510, 510], "gw": [2000, 2000], "gh": [900, 900]},
        "files": {"notes.txt": "plain text evidence\n"},
        "images": {"a.png": ["aw", "ah"], "b.png": ["bw", "bh"], "c.png": ["cw", "ch"],
                   "big.png": ["gw", "gh"]},
        "question": {"kind": "image", "subject": "a.png"},
        "parent": parent,
        "turns": [
            {"tools": [
                {"name": "read", "args": {"path": "a.png"}},
                {"name": "read", "args": {"path": "b.png"}},
                {"name": "read", "args": {"path": "c.png"}},
                {"name": "read", "args": {"path": "big.png"}},
                {"name": "read", "args": {"path": "notes.txt"}}
            ]},
            {"text": format!("## Findings\n- [confidence: high] ANSWER: {{{{aw}}}}x{{{{ah}}}} for a.png. [evidence: E1] {attach}\n\n## Unverified\n- None.\n\n## Coverage\n- Covered: a.png, b.png, c.png, big.png, notes.txt.\n- Skipped: None.")}
        ]
    }))
    .unwrap()
}

async fn run_lane(case: &EvidenceCase, arm: Arm) -> ArmResult {
    let values = draw_values(case, 1);
    Box::pin(run_arm(case, &values, arm)).await
}

fn envelope_images(output: &str) -> (String, Vec<(u32, u32)>) {
    use base64::Engine;
    let value: serde_json::Value = serde_json::from_str(output).expect("image envelope");
    let images = value["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| {
            assert_eq!(part["type"], "image");
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(part["data"].as_str().unwrap())
                .unwrap();
            let info = crate::agent::tools::read_image::inspect(&bytes)
                .unwrap()
                .unwrap();
            (info.width, info.height)
        })
        .collect();
    (value["response"].as_str().unwrap().to_string(), images)
}

#[tokio::test]
async fn inline_lane_returns_the_host_envelope_with_caps_and_omissions() {
    #[cfg(feature = "hooks")]
    let _dispatcher_guard = crate::tests::fake_model::dispatcher_guard::acquire();
    let case = lane_case("vision", "[attach: E5, E4, E1, E2, E3]");
    let arm = Arm {
        inline_images: 2,
        ..Arm::evidence(true)
    };
    let result = run_lane(&case, arm).await;
    let (response, images) = envelope_images(&result.output);
    // Images follow the whole text report, in attachment order, capped at 2.
    assert_eq!(images, vec![(640, 480), (300, 200)], "{response}");
    assert!(
        response.starts_with("[subagent output begins]\n"),
        "{response}"
    );
    assert!(response.contains("> ## Findings\n"));
    for expected in [
        "E5: not an image result; text is never inlined",
        "E4: not inlined, 2000x900 exceeds the 1568 px inline limit",
        "E1: attached inline (640x480, image/png, ~414 tokens)",
        "E2: attached inline (300x200, image/png, ~88 tokens)",
        "E3: not inlined, this child's limit of 2 inline images was reached",
    ] {
        assert!(
            response.contains(expected),
            "missing {expected:?}: {response}"
        );
    }
    // Every attached record is listed as an evidence row.
    for id in ["E1", "E2", "E3", "E4", "E5"] {
        assert!(response.contains(&format!("| {id} | read |")), "{response}");
    }
    // rig lifts exactly these images into image content.
    let lifted = rig::message::ToolResultContent::from_tool_output(result.output.clone());
    let kinds: Vec<_> = lifted
        .iter()
        .map(|part| matches!(part, rig::message::ToolResultContent::Image(_)))
        .collect();
    assert_eq!(kinds, vec![false, true, true]);
    // The child was told about the lane and its cap.
    assert!(result.bodies[0].contains("Inline image attachments (host policy)"));
    assert!(result.bodies[0].contains("At most 2 images are attached"));
}

#[tokio::test]
async fn inline_lane_degrades_to_evidence_rows_for_a_text_only_parent_and_is_off_by_default() {
    #[cfg(feature = "hooks")]
    let _dispatcher_guard = crate::tests::fake_model::dispatcher_guard::acquire();
    let case = lane_case("text", "[attach: E1]");
    let degraded = run_lane(
        &case,
        Arm {
            inline_images: 3,
            ..Arm::evidence(true)
        },
    )
    .await;
    assert!(
        serde_json::from_str::<serde_json::Value>(&degraded.output).is_err(),
        "{}",
        degraded.output
    );
    assert!(degraded.output.contains("| E1 | read | a.png |"));
    assert!(
        degraded
            .output
            .contains("E1: not inlined (Model has no vision"),
        "{}",
        degraded.output
    );

    // Default 0: the child is not told, the tag is ordinary text, no section.
    let vision = lane_case("vision", "[attach: E1]");
    let off = run_lane(&vision, Arm::evidence(true)).await;
    assert!(!off.output.starts_with('{'));
    assert!(!off.output.contains("Inline images"));
    assert!(!off.bodies[0].contains("Inline image attachments"));
    // Without evidence handles the lane is off whatever the setting.
    let no_evidence = run_lane(
        &vision,
        Arm {
            inline_images: 3,
            ..Arm::evidence(false)
        },
    )
    .await;
    assert!(!no_evidence.output.starts_with('{'));
    assert!(!no_evidence.bodies[0].contains("Inline image attachments"));
}

#[tokio::test]
async fn inline_images_are_charged_to_the_task_cost_budget_and_capped_per_call() {
    #[cfg(feature = "hooks")]
    let _dispatcher_guard = crate::tests::fake_model::dispatcher_guard::acquire();
    let case = lane_case("vision", "[attach: E1]");
    // Two child turns cost 2 x (100 + 5 x 20) = 400 units; a.png is
    // ceil(640/28) x ceil(480/28) = 414 tokens, so 600 units cannot cover it.
    let tight = run_lane(
        &case,
        Arm {
            inline_images: 1,
            max_cost_units: Some(600),
            ..Arm::evidence(true)
        },
    )
    .await;
    assert!(!tight.output.starts_with('{'), "{}", tight.output);
    assert!(
        tight
            .output
            .contains("E1: not inlined, the task cost budget cannot cover its ~414 tokens"),
        "{}",
        tight.output
    );
    let roomy = run_lane(
        &case,
        Arm {
            inline_images: 1,
            max_cost_units: Some(900),
            ..Arm::evidence(true)
        },
    )
    .await;
    let (response, images) = envelope_images(&roomy.output);
    assert_eq!(images, vec![(640, 480)]);
    assert!(!response.contains("[partial:"), "{response}");

    // Two children with three attachments each: five images per call.
    let fanout = lane_case("vision", "[attach: E1, E2, E3]");
    let result = run_lane(
        &fanout,
        Arm {
            inline_images: 3,
            prompts: 2,
            ..Arm::evidence(true)
        },
    )
    .await;
    let (response, images) = envelope_images(&result.output);
    assert_eq!(images.len(), 5, "{response}");
    assert_eq!(
        response
            .matches("not inlined, the per-call limit of 5 inline images was reached")
            .count(),
        1,
        "{response}"
    );
}
