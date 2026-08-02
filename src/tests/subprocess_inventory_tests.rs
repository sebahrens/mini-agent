use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use proc_macro2::{Delimiter, TokenStream, TokenTree};

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalCall {
    line: usize,
    name: String,
    guarded: bool,
}

fn terminal_calls(source: &str) -> Result<Vec<TerminalCall>, String> {
    fn is_terminal(name: &str) -> bool {
        matches!(
            name,
            "spawn" | "output" | "status" | "spawn_guarded" | "output_guarded" | "status_guarded"
        )
    }

    fn is_method_or_ufcs(tokens: &[TokenTree], index: usize) -> bool {
        let punct = |token: Option<&TokenTree>, expected| matches!(token, Some(TokenTree::Punct(punct)) if punct.as_char() == expected);
        if punct(index.checked_sub(1).and_then(|i| tokens.get(i)), '.') {
            return true;
        }
        if !punct(index.checked_sub(1).and_then(|i| tokens.get(i)), ':')
            || !punct(index.checked_sub(2).and_then(|i| tokens.get(i)), ':')
        {
            return false;
        }
        let Some(TokenTree::Ident(owner)) = index.checked_sub(3).and_then(|i| tokens.get(i)) else {
            return false;
        };
        !matches!(
            owner.to_string().as_str(),
            // These are reviewed task/thread or domain constructors, not OS-process terminals.
            // Every other UFCS owner fails closed unless its exact line is inventoried NON-PROCESS.
            "tokio" | "thread" | "LspClient" | "SanitizedTarget"
        )
    }

    fn scan(stream: TokenStream, calls: &mut Vec<TerminalCall>) {
        let tokens: Vec<_> = stream.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(group) = token {
                scan(group.stream(), calls);
            }
            let TokenTree::Ident(ident) = token else {
                continue;
            };
            let name = ident.to_string();
            if !is_terminal(&name) || !is_method_or_ufcs(&tokens, index) {
                continue;
            }
            let Some(TokenTree::Group(arguments)) = tokens.get(index + 1) else {
                continue;
            };
            if arguments.delimiter() != Delimiter::Parenthesis {
                continue;
            }
            calls.push(TerminalCall {
                line: ident.span().start().line,
                guarded: name.ends_with("_guarded"),
                name,
            });
        }
    }

    let stream = TokenStream::from_str(source)
        .map_err(|error| format!("Rust source did not tokenize: {error}"))?;
    let mut calls = Vec::new();
    scan(stream, &mut calls);
    calls.sort_by_key(|call| call.line);
    Ok(calls)
}

/// `(path, trimmed source fingerprint, occurrence count, trust class)`.
///
/// Counts make adding an identical launch expression visible without coupling
/// the inventory to source line numbers.
const UNIFORM_SITES: &[(&str, &str, usize, &str)] = &[
    ("src/agent/tools/bash.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/agent/tools/bash.rs",
        "std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/docs.rs",
        "let status = std::process::Command::new(\"less\")",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    ("src/docs.rs", ".status()?;", 1, "TC-SUPPORT-UTILITY"),
    (
        "src/extras/acp/mod.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/mod.rs",
        ".status(ToolCallStatus::Completed)",
        1,
        "NON-PROCESS",
    ),
    ("src/extras/acp/mod.rs", "cx.spawn({", 1, "NON-PROCESS"),
    (
        "src/extras/export.rs",
        "if !response.status().is_success() {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/export.rs",
        "let status = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        ".output()",
        12,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        ".output();",
        5,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        "// freezes the TUI during worktree merges. Migrate to tokio::process::Command",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        "Command::new(\"git\")",
        2,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        "let branch_output = Command::new(\"git\")",
        1,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        "let output = Command::new(\"git\")",
        14,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        "let output = Command::new(\"git\").args(args).output().ok()?;",
        1,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/hooks/subprocess.rs",
        "let mut child = match cmd.spawn() {",
        1,
        "TC-PROJECT-AUTOMATION",
    ),
    (
        "src/extras/hooks/subprocess.rs",
        "let mut cmd = Command::new(program);",
        1,
        "TC-PROJECT-AUTOMATION",
    ),
    (
        "src/extras/hooks/subprocess.rs",
        "use tokio::process::{Child, Command};",
        1,
        "TC-PROJECT-AUTOMATION",
    ),
    (
        "src/extras/js/engine.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/host.rs",
        "if is_followable_redirect(response.status()) {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/host.rs",
        "status: response.status().as_u16(),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/admission.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/embed.rs",
        "let status = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/proposal.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/telemetry.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/verify.rs",
        "handle.spawn(&program, &args).map_err(|reason| {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/supervisor.rs",
        ".spawn(move || {",
        3,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/tool.rs",
        "requests.spawn(async move {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/worker/macos.rs",
        ".output()",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let output = std::process::Command::new(SW_VERS)",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    ("src/extras/loop/validation.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/extras/loop/validation.rs",
        "assert!(!headless.contains(\"tokio::process::Command::new\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/loop/validation.rs",
        "assert!(!interactive.contains(\"tokio::process::Command::new\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/loop/validation.rs",
        "std::process::Command::new(\"/bin/kill\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/loop/mod.rs",
        ".status()",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/extras/loop/mod.rs",
        "let status = std::process::Command::new(\"bash\")",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    ("src/extras/lsp/client.rs", ".spawn()", 1, "TC-LSP-SERVICE"),
    (
        "src/extras/lsp/client.rs",
        "child: tokio::process::Child,",
        1,
        "TC-LSP-SERVICE",
    ),
    (
        "src/extras/lsp/client.rs",
        "let mut child = tokio::process::Command::new(cfg.command.as_str())",
        1,
        "TC-LSP-SERVICE",
    ),
    (
        "src/extras/lsp/client.rs",
        "stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,",
        1,
        "TC-LSP-SERVICE",
    ),
    ("src/extras/mcp/client.rs", ".spawn()", 1, "TC-MCP-STDIO"),
    (
        "src/extras/mcp/client.rs",
        "mut stderr: tokio::process::ChildStderr,",
        1,
        "TC-MCP-STDIO",
    ),
    (
        "src/extras/mcp/client.rs",
        "use tokio::process::Command;",
        1,
        "TC-MCP-STDIO",
    ),
    ("src/sandbox.rs", ".status();", 2, "TC-LIFECYCLE-HELPER"),
    (
        "src/sandbox.rs",
        "let _ = std::process::Command::new(\"kill\")",
        2,
        "TC-LIFECYCLE-HELPER",
    ),
    (
        "src/sandbox.rs",
        "let mut child = match cmd.spawn() {",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(\"zerobox\");",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(&self.shell);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(bwrap);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(seatbelt);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "use tokio::process::{Child, Command};",
        1,
        "TC-MODEL-ACTION",
    ),
    ("src/sandbox/worker.rs", ".spawn()", 1, "TEST-ONLY"),
    (
        "src/sandbox/worker.rs",
        "let mut command = Command::new(executable);",
        1,
        "TEST-ONLY",
    ),
    ("src/sandbox/worker/linux.rs", ".status()", 1, "TEST-ONLY"),
    ("src/sandbox/worker/linux.rs", ".spawn()?;", 1, "TEST-ONLY"),
    (
        "src/sandbox/worker/linux.rs",
        "if std::process::Command::new(WORKER_PATH)",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "if std::thread::Builder::new().spawn(|| {}).is_ok() {",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let Ok(mut child) = command.spawn() else {",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut child = command.spawn().map_err(|source| WorkerLaunchError::Io {",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut child = Command::new(WORKER_PATH)",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut child = command.spawn()?;",
        4,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut command = Command::new(bwrap);",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    ("src/session/mod.rs", ".output()", 1, "TC-INTERNAL-GIT"),
    (
        "src/session/mod.rs",
        "let out = std::process::Command::new(\"git\")",
        1,
        "TC-INTERNAL-GIT",
    ),
    ("src/startup.rs", ".output()?;", 1, "TC-EXPLICIT-USER-SHELL"),
    (
        "src/startup.rs",
        "let output = std::process::Command::new(\"bash\")",
        1,
        "TC-EXPLICIT-USER-SHELL",
    ),
    (
        "src/ui/app.rs",
        "if std::process::Command::new(\"lazygit\")",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/app.rs",
        "let _ = std::process::Command::new(\"lazygit\").status();",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/app.rs",
        "std::process::Command::new(\"bash\")",
        1,
        "TC-EXPLICIT-USER-SHELL",
    ),
    ("src/ui/input/mod.rs", ".status();", 1, "TC-SUPPORT-UTILITY"),
    (
        "src/ui/input/mod.rs",
        "let _ = std::process::Command::new(\"sh\")",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    ("src/ui/renderer.rs", ".spawn()", 2, "TC-SUPPORT-UTILITY"),
    (
        "src/ui/renderer.rs",
        "let Ok(mut child) = std::process::Command::new(cmd)",
        2,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/slash/memory.rs",
        ".status()?;",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/slash/memory.rs",
        "let status = std::process::Command::new(shell)",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/slash/session.rs",
        "match std::process::Command::new(\"git\")",
        1,
        "TC-INTERNAL-GIT",
    ),
    ("src/ui/slash/session.rs", ".output()", 1, "TC-INTERNAL-GIT"),
];

/// Sites whose identical terminal expression inherits different classes from
/// the surrounding constructor, in source order.
const MIXED_SITES: &[(&str, &str, &[&str])] = &[(
    "src/ui/app.rs",
    ".output()",
    &["TC-EXPLICIT-USER-SHELL", "TC-SUPPORT-UTILITY"],
)];

const ALLOWED_CURRENT_CLASSES: &[&str] = &[
    "NON-PROCESS",
    "TEST-ONLY",
    "TC-EXPLICIT-USER-SHELL",
    "TC-INTERNAL-GIT",
    "TC-INTERNAL-VERIFICATION",
    "TC-LIFECYCLE-HELPER",
    "TC-LOOP-VALIDATION",
    "TC-LSP-SERVICE",
    "TC-MCP-STDIO",
    "TC-MODEL-ACTION",
    "TC-BROKER-JS-WORKER",
    "TC-PROJECT-AUTOMATION",
    "TC-SUPPORT-UTILITY",
];

/// Exact ownership for every lexical disposition and every site in a source
/// file that contains more than one production trust class.
const EXACT_UNIFORM_SITE_CLASSES: &[(&str, &str, usize, &str)] = &[
    ("src/agent/tools/bash.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/agent/tools/bash.rs",
        "std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/acp/mod.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/mod.rs",
        ".status(ToolCallStatus::Completed)",
        1,
        "NON-PROCESS",
    ),
    ("src/extras/acp/mod.rs", "cx.spawn({", 1, "NON-PROCESS"),
    (
        "src/extras/export.rs",
        "if !response.status().is_success() {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/export.rs",
        "let status = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        "// freezes the TUI during worktree merges. Migrate to tokio::process::Command",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/engine.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/host.rs",
        "if is_followable_redirect(response.status()) {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/host.rs",
        "status: response.status().as_u16(),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/admission.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/embed.rs",
        "let status = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/proposal.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/telemetry.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/verify.rs",
        "handle.spawn(&program, &args).map_err(|reason| {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/supervisor.rs",
        ".spawn(move || {",
        3,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/tool.rs",
        "requests.spawn(async move {",
        1,
        "NON-PROCESS",
    ),
    ("src/extras/loop/validation.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/extras/loop/validation.rs",
        "assert!(!headless.contains(\"tokio::process::Command::new\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/loop/validation.rs",
        "assert!(!interactive.contains(\"tokio::process::Command::new\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/loop/validation.rs",
        "std::process::Command::new(\"/bin/kill\")",
        1,
        "TEST-ONLY",
    ),
    ("src/sandbox.rs", ".status();", 2, "TC-LIFECYCLE-HELPER"),
    (
        "src/sandbox.rs",
        "let _ = std::process::Command::new(\"kill\")",
        2,
        "TC-LIFECYCLE-HELPER",
    ),
    (
        "src/sandbox.rs",
        "let mut child = match cmd.spawn() {",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(\"zerobox\");",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(&self.shell);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(bwrap);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(seatbelt);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "use tokio::process::{Child, Command};",
        1,
        "TC-MODEL-ACTION",
    ),
    ("src/sandbox/worker.rs", ".spawn()", 1, "TEST-ONLY"),
    (
        "src/sandbox/worker.rs",
        "let mut command = Command::new(executable);",
        1,
        "TEST-ONLY",
    ),
    ("src/sandbox/worker/linux.rs", ".status()", 1, "TEST-ONLY"),
    ("src/sandbox/worker/linux.rs", ".spawn()?;", 1, "TEST-ONLY"),
    (
        "src/sandbox/worker/linux.rs",
        "if std::process::Command::new(WORKER_PATH)",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "if std::thread::Builder::new().spawn(|| {}).is_ok() {",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let Ok(mut child) = command.spawn() else {",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut child = command.spawn().map_err(|source| WorkerLaunchError::Io {",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut child = Command::new(WORKER_PATH)",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut child = command.spawn()?;",
        4,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut command = Command::new(bwrap);",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/ui/app.rs",
        "if std::process::Command::new(\"lazygit\")",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/app.rs",
        "let _ = std::process::Command::new(\"lazygit\").status();",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/app.rs",
        "std::process::Command::new(\"bash\")",
        1,
        "TC-EXPLICIT-USER-SHELL",
    ),
];

const EXACT_MIXED_SITE_CLASSES: &[(&str, &str, &[&str])] = &[(
    "src/ui/app.rs",
    ".output()",
    &["TC-EXPLICIT-USER-SHELL", "TC-SUPPORT-UTILITY"],
)];

/// Files whose non-disposition launch expressions all have one owner.
const SINGLE_CLASS_FAMILIES: &[(&str, &str)] = &[
    ("src/docs.rs", "TC-SUPPORT-UTILITY"),
    ("src/extras/git_worktree/mod.rs", "TC-INTERNAL-GIT"),
    ("src/extras/hooks/subprocess.rs", "TC-PROJECT-AUTOMATION"),
    ("src/extras/loop/mod.rs", "TC-INTERNAL-VERIFICATION"),
    ("src/extras/lsp/client.rs", "TC-LSP-SERVICE"),
    ("src/extras/mcp/client.rs", "TC-MCP-STDIO"),
    ("src/sandbox/worker/macos.rs", "TC-SUPPORT-UTILITY"),
    ("src/session/mod.rs", "TC-INTERNAL-GIT"),
    ("src/startup.rs", "TC-EXPLICIT-USER-SHELL"),
    ("src/ui/input/mod.rs", "TC-SUPPORT-UTILITY"),
    ("src/ui/renderer.rs", "TC-SUPPORT-UTILITY"),
    ("src/ui/slash/memory.rs", "TC-SUPPORT-UTILITY"),
    ("src/ui/slash/session.rs", "TC-INTERNAL-GIT"),
];

fn checked_inventory() -> BTreeMap<(String, String, usize), &'static str> {
    let mut expected = BTreeMap::new();
    for &(path, source, count, classification) in UNIFORM_SITES {
        for occurrence in 1..=count {
            assert!(
                expected
                    .insert(
                        (path.to_string(), source.to_string(), occurrence),
                        classification,
                    )
                    .is_none(),
                "duplicate checked inventory entry for {path} occurrence {occurrence}: {source}"
            );
        }
    }
    for &(path, source, classifications) in MIXED_SITES {
        for (index, &classification) in classifications.iter().enumerate() {
            let occurrence = index + 1;
            assert!(
                expected
                    .insert(
                        (path.to_string(), source.to_string(), occurrence),
                        classification,
                    )
                    .is_none(),
                "duplicate checked inventory entry for {path} occurrence {occurrence}: {source}"
            );
        }
    }
    expected
}

fn checked_exact_site_classes() -> BTreeMap<(String, String, usize), &'static str> {
    let mut exact = BTreeMap::new();
    for &(path, source, count, classification) in EXACT_UNIFORM_SITE_CLASSES {
        for occurrence in 1..=count {
            assert!(
                exact
                    .insert(
                        (path.to_string(), source.to_string(), occurrence),
                        classification,
                    )
                    .is_none(),
                "duplicate exact ownership rule for {path} occurrence {occurrence}: {source}"
            );
        }
    }
    for &(path, source, classifications) in EXACT_MIXED_SITE_CLASSES {
        for (index, &classification) in classifications.iter().enumerate() {
            let occurrence = index + 1;
            assert!(
                exact
                    .insert(
                        (path.to_string(), source.to_string(), occurrence),
                        classification,
                    )
                    .is_none(),
                "duplicate exact ownership rule for {path} occurrence {occurrence}: {source}"
            );
        }
    }
    exact
}

fn validate_class_assignments(
    inventory: &BTreeMap<(String, String, usize), &'static str>,
) -> Result<(), String> {
    let exact = checked_exact_site_classes();
    let mut families = BTreeMap::new();
    for &(path, classification) in SINGLE_CLASS_FAMILIES {
        if families.insert(path, classification).is_some() {
            return Err(format!("duplicate source-family ownership rule for {path}"));
        }
    }

    for ((path, source, occurrence), classification) in inventory {
        if !ALLOWED_CURRENT_CLASSES.contains(classification) {
            return Err(format!(
                "class {classification} is not allowed for current launch inventory"
            ));
        }
        let site = (path.clone(), source.clone(), *occurrence);
        let owner = exact
            .get(&site)
            .copied()
            .or_else(|| families.get(path.as_str()).copied())
            .ok_or_else(|| {
                format!("site has no exact or single-class ownership rule: {path} occurrence {occurrence}: {source}")
            })?;
        if owner != *classification {
            return Err(format!(
                "class {classification} cannot own {path} occurrence {occurrence}: {source}; expected {owner}"
            ));
        }
    }

    for ((path, source, occurrence), _) in &exact {
        if !inventory.contains_key(&(path.clone(), source.clone(), *occurrence)) {
            return Err(format!(
                "stale exact ownership rule for {path} occurrence {occurrence}: {source}"
            ));
        }
    }
    for path in families.keys() {
        if !inventory
            .keys()
            .any(|(inventory_path, source, occurrence)| {
                inventory_path == path
                    && !exact.contains_key(&(inventory_path.clone(), source.clone(), *occurrence))
            })
        {
            return Err(format!("stale single-class ownership rule for {path}"));
        }
    }
    Ok(())
}

fn validate_current_class_assignments() -> Result<(), String> {
    validate_class_assignments(&checked_inventory())
}

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut sources = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).expect("source directory must be readable") {
            let path = entry.expect("source entry must be readable").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
    }
    sources.sort();
    sources
}

fn is_inventory_line(line: &str) -> bool {
    line.contains("Command::new")
        || line.contains("tokio::process")
        || line.contains(".spawn(")
        || line.contains(".spawn_guarded(")
        || line.contains(".output(")
        || line.contains(".output_guarded(")
        || line.contains(".status(")
        || line.contains(".status_guarded(")
}

fn normalized_inventory_line(line: &str) -> String {
    line.replace(".spawn_guarded(", ".spawn(")
        .replace(".output_guarded(", ".output(")
        .replace(".status_guarded(", ".status(")
}

fn creation_boundary_violations(
    relative: &str,
    contents: &str,
    expected: &BTreeMap<(String, String, usize), &'static str>,
) -> Result<Vec<String>, String> {
    let mut seen = BTreeMap::<(String, String), usize>::new();
    let mut classes_by_line = BTreeMap::<usize, Vec<&'static str>>::new();

    for (line_index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if !is_inventory_line(line) {
            continue;
        }
        let normalized = normalized_inventory_line(line);
        let fingerprint = (relative.to_string(), normalized);
        let occurrence = seen.entry(fingerprint.clone()).or_default();
        *occurrence += 1;
        if let Some(&classification) = expected.get(&(fingerprint.0, fingerprint.1, *occurrence)) {
            classes_by_line
                .entry(line_index + 1)
                .or_default()
                .push(classification);
        }
    }

    let mut violations = Vec::new();
    for call in terminal_calls(contents)? {
        if call.guarded {
            continue;
        }
        match classes_by_line.get(&call.line) {
            Some(classes) if classes.iter().all(|class| !class.starts_with("TC-")) => {}
            Some(classes) => violations.push(format!(
                "{relative}:{} unguarded {} terminal classified as {}",
                call.line,
                call.name,
                classes.join("/")
            )),
            None => violations.push(format!(
                "{relative}:{} unrecognized unguarded {} terminal",
                call.line, call.name
            )),
        }
    }
    Ok(violations)
}

#[test]
fn production_subprocess_sites_have_a_trust_classification() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_root = root.join("src");
    let mut seen = BTreeMap::<(String, String), usize>::new();
    let mut observed = BTreeSet::<(String, String, usize)>::new();

    for source in rust_sources(&source_root) {
        let relative = source
            .strip_prefix(root)
            .expect("source must be below manifest root")
            .to_string_lossy()
            .replace('\\', "/");
        if relative.starts_with("src/tests/") || relative.starts_with("src/extras/js/tests/") {
            continue;
        }
        if relative == "src/process_creation.rs" {
            continue;
        }
        let contents = std::fs::read_to_string(&source).expect("Rust source must be UTF-8");
        for line in contents
            .lines()
            .map(str::trim)
            .filter(|line| is_inventory_line(line))
        {
            let fingerprint = (relative.clone(), normalized_inventory_line(line));
            let occurrence = seen.entry(fingerprint.clone()).or_default();
            *occurrence += 1;
            observed.insert((fingerprint.0, fingerprint.1, *occurrence));
        }
    }

    let expected = checked_inventory();

    let unclassified: Vec<_> = observed
        .iter()
        .filter(|site| !expected.contains_key(*site))
        .collect();
    let stale: Vec<_> = expected
        .keys()
        .filter(|site| !observed.contains(*site))
        .collect();
    assert!(
        unclassified.is_empty() && stale.is_empty(),
        "subprocess inventory drifted; classify every new production Command::new/tokio::process/.spawn/.output/.status result in docs/specs/subprocess-trust.md and update the checked inventory.\nUnclassified or changed: {unclassified:#?}\nStale: {stale:#?}"
    );

    let specification = std::fs::read_to_string(root.join("docs/specs/subprocess-trust.md"))
        .expect("the normative subprocess trust specification must exist");
    for classification in expected.values() {
        if classification.starts_with("TC-") {
            let table_prefix = format!("| `{classification}`");
            assert!(
                specification
                    .lines()
                    .any(|line| line.starts_with(&table_prefix)),
                "inventory classification {classification} has no normative contract-table row"
            );
        } else {
            assert!(
                specification.contains(&format!("`{classification}`")),
                "inventory disposition {classification} has no specification entry"
            );
        }
    }
}

#[test]
fn windows_capable_production_process_terminals_use_creation_boundary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_root = root.join("src");
    let expected = checked_inventory();
    let mut unguarded = Vec::new();

    for source in rust_sources(&source_root) {
        let relative = source
            .strip_prefix(root)
            .expect("source must be below manifest root")
            .to_string_lossy()
            .replace('\\', "/");
        if relative.starts_with("src/tests/")
            || relative.starts_with("src/extras/js/tests/")
            || relative == "src/process_creation.rs"
            || matches!(
                relative.as_str(),
                "src/sandbox/worker/linux.rs" | "src/sandbox/worker/macos.rs"
            )
        {
            continue;
        }
        let contents = std::fs::read_to_string(&source).expect("Rust source must be UTF-8");
        unguarded.extend(
            creation_boundary_violations(&relative, &contents, &expected)
                .unwrap_or_else(|error| panic!("could not inspect {relative}: {error}")),
        );
    }

    assert!(
        unguarded.is_empty(),
        "Windows-capable production process terminals bypass the crate creation boundary: {unguarded:#?}"
    );
}

#[test]
fn current_subprocess_inventory_accepts_exact_broker_and_rejects_cross_family_classes() {
    validate_current_class_assignments().expect("current subprocess classes must be allowed");
}

#[test]
fn subprocess_inventory_rejects_site_specific_relabels_in_mixed_files() {
    let cases = [
        (
            "src/agent/tools/bash.rs",
            ".status()",
            1,
            "TC-LIFECYCLE-HELPER",
            "an exact TEST-ONLY fingerprint",
        ),
        (
            "src/extras/git_worktree/mod.rs",
            "// freezes the TUI during worktree merges. Migrate to tokio::process::Command",
            1,
            "TC-INTERNAL-GIT",
            "an exact NON-PROCESS fingerprint in a production family",
        ),
        (
            "src/extras/git_worktree/mod.rs",
            ".output()",
            1,
            "NON-PROCESS",
            "a production launch in a family with a NON-PROCESS comment",
        ),
        (
            "src/sandbox.rs",
            "let mut cmd = Command::new(\"zerobox\");",
            1,
            "TC-LIFECYCLE-HELPER",
            "a model action in the mixed sandbox family",
        ),
        (
            "src/sandbox.rs",
            "let _ = std::process::Command::new(\"kill\")",
            1,
            "TC-MODEL-ACTION",
            "a lifecycle helper in the mixed sandbox family",
        ),
        (
            "src/sandbox/worker/linux.rs",
            "let Ok(mut child) = command.spawn() else {",
            1,
            "TC-MODEL-ACTION",
            "the broker-only Linux preflight launch",
        ),
        (
            "src/ui/app.rs",
            ".output()",
            1,
            "TC-SUPPORT-UTILITY",
            "the explicit shell output occurrence in the mixed UI family",
        ),
        (
            "src/ui/app.rs",
            "if std::process::Command::new(\"lazygit\")",
            1,
            "TC-EXPLICIT-USER-SHELL",
            "the lazygit utility in the mixed UI family",
        ),
    ];

    for (path, source, occurrence, replacement, description) in cases {
        let mut relabeled = checked_inventory();
        let key = (path.to_string(), source.to_string(), occurrence);
        assert!(
            relabeled.contains_key(&key),
            "missing test fixture for {description}"
        );
        relabeled.insert(key, replacement);
        assert!(
            validate_class_assignments(&relabeled).is_err(),
            "ownership validation accepted relabeling {description} as {replacement}"
        );
    }
}

#[test]
fn token_terminal_discovery_rejects_multiline_creation_lock_bypasses() {
    let calls = terminal_calls(
        r#"
fn launch(command: &mut std::process::Command) {
    let _ = command.spawn
        ();
    let _ = command.output
        ();
    let _ = command.status
        ();
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(
        calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        ["spawn", "output", "status"]
    );
    assert!(calls.iter().all(|call| !call.guarded));
}

#[test]
fn token_terminal_discovery_rejects_std_and_tokio_ufcs_bypasses() {
    let calls = terminal_calls(
        r#"
fn launch(
    std_command: &mut std::process::Command,
    tokio_command: &mut tokio::process::Command,
) {
    let _ = std::process::Command::spawn(&mut *std_command);
    let _ = std::process::Command::output(&mut *std_command);
    let _ = std::process::Command::status(&mut *std_command);
    let _ = tokio::process::Command::spawn(&mut *tokio_command);
    let _ = ProcessCommand::spawn(&mut *std_command);
    let _ = Cmd::output(&mut *std_command);
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(
        calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        ["spawn", "output", "status", "spawn", "spawn", "output"]
    );
    assert!(calls.iter().all(|call| !call.guarded));
}

#[test]
fn token_terminal_discovery_distinguishes_guarded_calls_and_ignores_text() {
    let calls = terminal_calls(
        r#"
fn launch(command: &mut std::process::Command) {
    // command.spawn();
    let text = ".output()";
    let task = tokio::spawn(async {});
    let thread = std::thread::spawn(|| {});
    let _ = command.spawn_guarded
        ();
    let _ = StdCommandCreationExt::output_guarded(command);
    let _ = command.status_guarded();
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|call| call.guarded));
}

#[test]
fn boundary_validation_fails_closed_for_unclassified_multiline_and_ufcs_terminals() {
    let fixture = r#"
fn launch(
    std_command: &mut std::process::Command,
    tokio_command: &mut tokio::process::Command,
) {
    let _ = std_command.spawn
        ();
    let _ = std::process::Command::output(&mut *std_command);
    let _ = tokio::process::Command::spawn(&mut *tokio_command);
    let _ = ProcessCommand::status(&mut *std_command);
}
"#;
    let violations = creation_boundary_violations("src/fixture.rs", fixture, &BTreeMap::new())
        .expect("fixture must be inspectable");

    assert_eq!(violations.len(), 4);
    assert!(
        violations
            .iter()
            .all(|violation| violation.contains("unrecognized unguarded")),
        "every terminal missing from the inventory must fail closed: {violations:#?}"
    );
}
