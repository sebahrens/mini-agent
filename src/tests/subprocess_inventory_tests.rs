use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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
        "let status = std::process::Command::new(\"less\").arg(&doc_path).status()?;",
        1,
        "TC-SUPPORT-UTILITY",
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
        ".output()",
        12,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        ".output();",
        4,
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
        13,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        "let output = Command::new(\"git\").args([\"status\", \"--porcelain\"]).output();",
        1,
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
        "src/extras/js/tool.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/tool.rs",
        "requests.spawn(async move {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/loop/headless.rs",
        ".output()",
        1,
        "TC-LOOP-VALIDATION",
    ),
    (
        "src/extras/loop/headless.rs",
        "match tokio::process::Command::new(shell)",
        1,
        "TC-LOOP-VALIDATION",
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
    (
        "src/ui/event_handler.rs",
        ".output()",
        1,
        "TC-LOOP-VALIDATION",
    ),
    (
        "src/ui/event_handler.rs",
        "match tokio::process::Command::new(shell)",
        1,
        "TC-LOOP-VALIDATION",
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
        "match std::process::Command::new(\"git\").args([\"stash\"]).output() {",
        1,
        "TC-INTERNAL-GIT",
    ),
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
    "TC-PROJECT-AUTOMATION",
    "TC-SUPPORT-UTILITY",
];

/// Allowed classes per source family. This prevents a known source site from
/// satisfying the lexical inventory by borrowing an unrelated class token.
const FAMILY_CLASSES: &[(&str, &[&str])] = &[
    ("src/agent/tools/bash.rs", &["TEST-ONLY"]),
    ("src/docs.rs", &["TC-SUPPORT-UTILITY"]),
    ("src/extras/acp/mod.rs", &["NON-PROCESS"]),
    ("src/extras/export.rs", &["NON-PROCESS"]),
    (
        "src/extras/git_worktree/mod.rs",
        &["TC-INTERNAL-GIT", "NON-PROCESS"],
    ),
    ("src/extras/hooks/subprocess.rs", &["TC-PROJECT-AUTOMATION"]),
    ("src/extras/js/engine.rs", &["NON-PROCESS"]),
    ("src/extras/js/host.rs", &["NON-PROCESS"]),
    ("src/extras/js/skills/admission.rs", &["NON-PROCESS"]),
    ("src/extras/js/skills/embed.rs", &["NON-PROCESS"]),
    ("src/extras/js/skills/proposal.rs", &["NON-PROCESS"]),
    ("src/extras/js/skills/telemetry.rs", &["NON-PROCESS"]),
    ("src/extras/js/skills/turn.rs", &["NON-PROCESS"]),
    ("src/extras/js/skills/verify.rs", &["NON-PROCESS"]),
    ("src/extras/js/tool.rs", &["NON-PROCESS"]),
    ("src/extras/loop/headless.rs", &["TC-LOOP-VALIDATION"]),
    ("src/extras/loop/mod.rs", &["TC-INTERNAL-VERIFICATION"]),
    ("src/extras/lsp/client.rs", &["TC-LSP-SERVICE"]),
    ("src/extras/mcp/client.rs", &["TC-MCP-STDIO"]),
    (
        "src/sandbox.rs",
        &["TC-MODEL-ACTION", "TC-LIFECYCLE-HELPER"],
    ),
    ("src/session/mod.rs", &["TC-INTERNAL-GIT"]),
    ("src/startup.rs", &["TC-EXPLICIT-USER-SHELL"]),
    (
        "src/ui/app.rs",
        &["TC-EXPLICIT-USER-SHELL", "TC-SUPPORT-UTILITY"],
    ),
    ("src/ui/event_handler.rs", &["TC-LOOP-VALIDATION"]),
    ("src/ui/input/mod.rs", &["TC-SUPPORT-UTILITY"]),
    ("src/ui/renderer.rs", &["TC-SUPPORT-UTILITY"]),
    ("src/ui/slash/memory.rs", &["TC-SUPPORT-UTILITY"]),
    ("src/ui/slash/session.rs", &["TC-INTERNAL-GIT"]),
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

fn validate_current_class_assignments() -> Result<(), String> {
    let expected = checked_inventory();
    let inventory_paths: BTreeSet<_> = expected.keys().map(|(path, _, _)| path.as_str()).collect();

    for ((path, source, occurrence), classification) in &expected {
        if *classification == "TC-BROKER-JS-WORKER" {
            return Err(format!(
                "broker-only JS worker cannot classify a current site: {path} occurrence {occurrence}: {source}"
            ));
        }
        if !ALLOWED_CURRENT_CLASSES.contains(classification) {
            return Err(format!(
                "class {classification} is not allowed for current launch inventory"
            ));
        }
        let family_classes = FAMILY_CLASSES
            .iter()
            .find_map(|(family, classes)| (*family == path).then_some(*classes))
            .ok_or_else(|| format!("source family {path} has no ownership rule"))?;
        if !family_classes.contains(classification) {
            return Err(format!(
                "class {classification} cannot own {path} occurrence {occurrence}: {source}"
            ));
        }
    }

    for (path, _) in FAMILY_CLASSES {
        if !inventory_paths.contains(path) {
            return Err(format!("stale source-family ownership rule for {path}"));
        }
    }
    Ok(())
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
        || line.contains(".output(")
        || line.contains(".status(")
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
        let contents = std::fs::read_to_string(&source).expect("Rust source must be UTF-8");
        for line in contents
            .lines()
            .map(str::trim)
            .filter(|line| is_inventory_line(line))
        {
            let fingerprint = (relative.clone(), line.to_string());
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
fn current_subprocess_inventory_rejects_broker_and_cross_family_classes() {
    validate_current_class_assignments().expect("current subprocess classes must be allowed");
}
