//! Model-facing tool argument structs reject unknown field names instead of
//! silently dropping wrong-harness parameters (`workdir`, `run_in_background`).

use serde_json::json;

use crate::agent::tools::{
    BashArgs, EditArgs, FindFilesArgs, GrepArgs, JobStatusArgs, ListDirArgs, ReadArgs, WriteArgs,
};

fn rejects<T: serde::de::DeserializeOwned>(valid: serde_json::Value, bad_key: &str) {
    assert!(
        serde_json::from_value::<T>(valid.clone()).is_ok(),
        "valid args must parse: {valid}"
    );
    let mut with_extra = valid;
    with_extra[bad_key] = json!(true);
    let error = match serde_json::from_value::<T>(with_extra) {
        Ok(_) => panic!("unknown key {bad_key} must be rejected"),
        Err(e) => e.to_string(),
    };
    assert!(
        error.contains(&format!("unknown field `{bad_key}`")),
        "{error}"
    );
    assert!(
        error.contains("expected"),
        "error must list valid names: {error}"
    );
}

#[test]
fn shell_rejects_wrong_harness_parameters() {
    rejects::<BashArgs>(json!({"command": "ls"}), "run_in_background");
    rejects::<BashArgs>(json!({"command": "ls"}), "workdir");
    let error = serde_json::from_value::<BashArgs>(json!({"command": "ls", "workdir": "a"}))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        error.contains("`background`"),
        "valid names are listed: {error}"
    );
}

#[test]
fn file_tools_reject_unknown_parameters() {
    rejects::<ReadArgs>(json!({"path": "a"}), "head_limit");
    rejects::<WriteArgs>(json!({"path": "a", "content": "b"}), "workdir");
    rejects::<EditArgs>(json!({"path": "a"}), "old_string");
    rejects::<GrepArgs>(json!({"pattern": "a"}), "glob");
    rejects::<GrepArgs>(json!({"pattern": "a"}), "head_limit");
    rejects::<FindFilesArgs>(json!({"pattern": "a"}), "glob");
    rejects::<ListDirArgs>(json!({}), "recursive");
    rejects::<JobStatusArgs>(json!({}), "job_id");
}

#[test]
fn nested_edit_ops_reject_unknown_parameters() {
    let ok = json!({"path": "a", "edits": [{"line": "1", "text": "x"}]});
    assert!(serde_json::from_value::<EditArgs>(ok).is_ok());
    let bad = json!({"path": "a", "edits": [{"line": "1", "text": "x", "oops": 1}]});
    assert!(serde_json::from_value::<EditArgs>(bad).is_err());
}
