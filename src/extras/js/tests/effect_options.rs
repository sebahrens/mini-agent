//! Contained-worker coverage for the js effect options and behaviours added by the
//! 2026-11-21 harness review: `write_file` parent directories, read decoding, per-item
//! `read_files` failures, discovery `include_ignored`, and `spawn` options. Each test drives
//! the model-facing `js` tool, so every effect crosses the parent broker.

use std::sync::Arc;

use rig::tool::Tool;

use super::TestTempDir;
use crate::extras::js::tool::{JsArgs, JsTool};

fn plain_tool(workspace: &Arc<crate::paths::WorkspaceBinding>, audits: &TestTempDir) -> JsTool {
    let root = audits.path().join(format!("tool-{}", uuid::Uuid::new_v4()));
    let paths = crate::paths::AppPaths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        local_data_dir: root.join("local"),
        state_dir: root.join("state"),
        cache_dir: root.join("cache"),
        credentials_dir: root.join("credentials"),
        project_dir: None,
    };
    let audit = crate::extras::js::audit::EffectAudit::open(paths.effect_audit()).unwrap();
    JsTool::new_with_runtime_for_test(
        crate::sandbox::Sandbox::new(false, "bwrap")
            .with_workspace_binding(workspace.clone())
            .with_complete_process_tree_for_test(),
        None,
        None,
        crate::extras::js::host::AllowConfig::unrestricted(workspace.root())
            .with_workspace_binding(workspace.clone()),
        Arc::new(
            crate::extras::js::supervisor::JsWorkerSupervisor::with_launcher_for_test(
                crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
            ),
        ),
        Arc::new(std::sync::Mutex::new(audit)),
    )
}

async fn run(tool: &JsTool, code: &str) -> String {
    tool.call(JsArgs {
        code: code.to_string(),
    })
    .await
    .expect("js call")
}

fn fixture(label: &str) -> (TestTempDir, crate::tests::OwnedTestDirectory, JsTool) {
    let audits = TestTempDir::new(&format!("js-opts-{label}"));
    let root = crate::tests::OwnedTestDirectory::new(&format!("js-opts-{label}-ws"));
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let tool = plain_tool(&workspace, &audits);
    (audits, root, tool)
}

#[tokio::test]
async fn write_file_creates_missing_parent_directories_in_the_workspace() {
    let (_audits, root, tool) = fixture("parents");
    let out = run(&tool, "write_file('a/b/c.txt', 'x'); 'ok'").await;
    assert_eq!(out, "ok");
    assert_eq!(
        std::fs::read_to_string(root.path().join("a/b/c.txt")).unwrap(),
        "x"
    );
    // An absolute spelling takes the ambient path and creates its chain the same way.
    let absolute = root.path().join("x/y/z.txt");
    let out = run(
        &tool,
        &format!("write_file({:?}, 'z'); 'ok'", absolute.to_string_lossy()),
    )
    .await;
    assert_eq!(out, "ok");
    assert_eq!(std::fs::read_to_string(absolute).unwrap(), "z");
}

#[cfg(unix)]
#[tokio::test]
async fn write_file_refuses_a_symlinked_component_on_the_way_to_a_new_file() {
    let (_audits, root, tool) = fixture("symlink");
    let outside = crate::tests::OwnedTestDirectory::new("js-opts-symlink-outside");
    std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
    for path in ["link/new/x.txt", "link/x.txt"] {
        let out = run(
            &tool,
            &format!(
                "try {{ write_file({path:?}, 'x'); 'wrote' }} catch (e) {{ 'ERR ' + e.code }}"
            ),
        )
        .await;
        assert!(out.starts_with("ERR"), "{path}: {out}");
    }
    assert!(
        std::fs::read_dir(outside.path()).unwrap().next().is_none(),
        "nothing may be created through the symlink"
    );
}

#[tokio::test]
async fn read_file_decodes_utf16_and_bom_and_names_non_text_files() {
    let (_audits, root, tool) = fixture("decode");
    let dir = root.path();
    let utf16: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain("hi".encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    std::fs::write(dir.join("u16.txt"), utf16).unwrap();
    std::fs::write(dir.join("bom.json"), b"\xef\xbb\xbf{\"a\":1}").unwrap();
    std::fs::write(dir.join("latin1.txt"), b"na\xefve").unwrap();
    let out = run(
        &tool,
        "const t = (p) => { try { return read_file(p) } catch (e) { return 'ERR ' + e.code + ' ' + e.message } };\
         JSON.stringify([t('u16.txt'), JSON.parse(t('bom.json')), t('latin1.txt'), read_lines('latin1.txt', 1, 1).lines[0]])",
    )
    .await;
    let rows: serde_json::Value = serde_json::from_str(&out).expect(&out);
    assert_eq!(rows[0], "hi");
    assert_eq!(rows[1]["a"], 1);
    let latin = rows[2].as_str().unwrap();
    assert!(latin.starts_with("ERR not_text"), "{latin}");
    assert!(latin.contains("read_lines"), "{latin}");
    assert!(rows[3].as_str().unwrap().starts_with("na"));
}

#[tokio::test]
async fn read_files_reports_the_failing_index_and_how_many_items_fit() {
    let (_audits, root, tool) = fixture("index");
    let dir = root.path();
    std::fs::write(dir.join("ok.txt"), "one").unwrap();
    std::fs::write(dir.join("big.bin"), vec![b'x'; 2 * 1024 * 1024]).unwrap();
    std::fs::write(dir.join("ok2.txt"), "two").unwrap();
    std::fs::write(dir.join("latin1.txt"), b"na\xefve").unwrap();
    let out = run(
        &tool,
        "const t = (paths) => { try { return read_files(paths).length } catch (e) { return [e.code, e.index] } };\
         JSON.stringify([t(['ok.txt', 'big.bin', 'ok2.txt']), t(['ok.txt', 'ok2.txt', 'latin1.txt']), t(['ok.txt', 'ok2.txt'])])",
    )
    .await;
    assert_eq!(out, r#"[["too_large",1],["not_text",2],2]"#);
}

#[tokio::test]
async fn discovery_reports_ignored_entries_and_include_ignored_lists_them() {
    let (_audits, root, tool) = fixture("ignored");
    let dir = root.path();
    std::fs::write(dir.join(".gitignore"), "build/\n").unwrap();
    std::fs::create_dir_all(dir.join("build")).unwrap();
    std::fs::write(dir.join("build/out.log"), "needle\n").unwrap();
    std::fs::write(dir.join("kept.txt"), "needle\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("kept.txt", dir.join("alias.txt")).unwrap();
    let out = run(
        &tool,
        "const names = (r) => r.entries.map(e => e.name).sort();\
         const a = list_dir('.'); const b = list_dir('.', {include_ignored: true});\
         const g1 = glob('**/*.log'); const g2 = glob('**/*.log', {include_ignored: true});\
         const s1 = grep('needle'); const s2 = grep('needle', {include_ignored: true});\
         JSON.stringify({a: names(a), ai: a.ignored > 0, b: names(b), bi: b.ignored,\
           g1: g1.paths, g1i: g1.ignored > 0, g2: g2.paths,\
           s1: s1.matches.map(m => m.path), s2: s2.matches.map(m => m.path).sort(),\
           kinds: Object.fromEntries(a.entries.map(e => [e.name, e.kind]))})",
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&out).expect(&out);
    assert!(
        !value["a"].as_array().unwrap().iter().any(|n| n == "build"),
        "{out}"
    );
    assert_eq!(value["ai"], true, "{out}");
    assert!(
        value["b"].as_array().unwrap().iter().any(|n| n == "build"),
        "{out}"
    );
    assert_eq!(value["bi"], 0, "{out}");
    assert_eq!(value["g1"], serde_json::json!([]), "{out}");
    assert_eq!(value["g1i"], true, "{out}");
    assert_eq!(value["g2"], serde_json::json!(["build/out.log"]), "{out}");
    assert_eq!(value["s1"], serde_json::json!(["kept.txt"]), "{out}");
    assert_eq!(
        value["s2"],
        serde_json::json!(["build/out.log", "kept.txt"]),
        "{out}"
    );
    #[cfg(unix)]
    assert_eq!(value["kinds"]["alias.txt"], "symlink", "{out}");
}

#[cfg(unix)]
#[tokio::test]
async fn spawn_options_cwd_env_stdin_and_timeout() {
    let (_audits, root, tool) = fixture("spawn");
    std::fs::create_dir_all(root.path().join("sub")).unwrap();
    let out = run(
        &tool,
        "const t = (f) => { try { return f() } catch (e) { return 'ERR ' + e.code } };\
         const cwd = spawn('pwd', [], {cwd: 'sub'});\
         const env = spawn('sh', ['-c', 'echo $GREETING'], {env: {GREETING: 'hello'}});\
         const stdin = spawn('cat', [], {stdin: 'round trip'});\
         const slow = spawn('sleep', ['5'], {timeout_ms: 300});\
         JSON.stringify({cwd: cwd.stdout.trim().endsWith('/sub'), env: env.stdout.trim(), stdin: stdin.stdout,\
           timed_out: slow.timed_out, after: 'still running',\
           outside: t(() => spawn('pwd', [], {cwd: '..'})),\
           secret: t(() => spawn('env', [], {env: {MY_TOKEN: 'x'}})),\
           loader: t(() => spawn('env', [], {env: {LD_PRELOAD: 'x'}})),\
           unknown: t(() => spawn('pwd', [], {shell: true}))})",
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&out).expect(&out);
    assert_eq!(value["cwd"], true, "{out}");
    assert_eq!(value["env"], "hello", "{out}");
    assert_eq!(value["stdin"], "round trip", "{out}");
    assert_eq!(value["timed_out"], true, "{out}");
    for key in ["outside", "secret", "loader", "unknown"] {
        assert!(
            value[key].as_str().unwrap().starts_with("ERR"),
            "{key}: {out}"
        );
    }
}
