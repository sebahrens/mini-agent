use crate::extras::hooks::settings::parse_hooks_config;
use serde_json::json;

#[test]
fn parses_cc_shaped_config() {
    let value = json!({
        "PreToolUse": [
            {
                "matcher": "Bash|Write",
                "hooks": [
                    {"type": "command", "command": "echo hi", "timeout": 60}
                ]
            }
        ]
    });

    let config = parse_hooks_config(&value).expect("valid config should parse");
    let groups = config.get("PreToolUse").expect("PreToolUse group present");
    assert_eq!(groups.len(), 1);
    let group = &groups[0];
    assert_eq!(group.matcher.as_deref(), Some("Bash|Write"));
    assert_eq!(group.hooks.len(), 1);
    let handler = &group.hooks[0];
    assert_eq!(handler.kind, "command");
    assert_eq!(handler.command.as_deref(), Some("echo"));
    assert_eq!(handler.args.as_deref(), Some(["hi".to_string()].as_slice()));
    assert_eq!(handler.timeout, Some(60));
    assert!(!handler.is_async);
    assert!(handler.condition.is_none());
    assert!(!handler.once);
}

#[test]
fn if_and_once_fields_still_parse_and_are_preserved() {
    let value = json!({
        "Stop": [
            {
                "hooks": [
                    {
                        "type": "command",
                        "command": "run-tests.sh",
                        "if": "test -f ./Cargo.toml",
                        "once": true
                    }
                ]
            }
        ]
    });

    let config = parse_hooks_config(&value).expect("if/once should not fail parsing");
    let handler = &config.get("Stop").unwrap()[0].hooks[0];
    assert_eq!(handler.condition.as_deref(), Some("test -f ./Cargo.toml"));
    assert!(handler.once);
}

#[test]
fn unsupported_handler_types_are_skipped_per_hook_without_dropping_the_file() {
    let value = json!({
        "PreToolUse": [
            {
                "hooks": [
                    {"type": "http", "command": "irrelevant"},
                    {"type": "prompt", "prompt": "judge"},
                    {"type": "command", "command": "guard"},
                    {"type": "http", "url": "https://hooks.example.invalid/pre", "headers": {"X-Token": "literal"}},
                    {"type": "http", "url": "file:///etc/passwd"}
                ]
            }
        ]
    });

    let config = parse_hooks_config(&value).expect("one bad hook must not fail the file");
    let hooks = &config["PreToolUse"][0].hooks;
    assert_eq!(hooks.len(), 2);
    assert_eq!(hooks[0].kind, "command");
    assert_eq!(hooks[1].kind, "http");
    assert_eq!(
        hooks[1].command.as_deref(),
        Some("https://hooks.example.invalid/pre")
    );
    assert_eq!(
        hooks[1].env.get("X-Token").map(String::as_str),
        Some("literal")
    );
}

#[test]
fn unsupported_event_name_is_a_parse_error() {
    let value = json!({
        "PreToolUSe": [{"hooks": [{"type": "command", "command": "guard"}]}]
    });

    let error = parse_hooks_config(&value).unwrap_err();
    assert!(error.contains("PreToolUSe"), "{error}");
    assert!(error.contains("PreToolUse"), "{error}");
}

#[test]
fn args_field_parses_as_string_list() {
    let value = json!({
        "PreToolUse": [
            {
                "hooks": [
                    {"type": "command", "command": "guard", "args": ["--strict", "--verbose"]}
                ]
            }
        ]
    });

    let config = parse_hooks_config(&value).unwrap();
    let handler = &config.get("PreToolUse").unwrap()[0].hooks[0];
    assert_eq!(
        handler.args.as_deref(),
        Some(&["--strict".to_string(), "--verbose".to_string()][..])
    );
}

#[test]
fn repo_settings_shape_and_unknown_events_keep_known_hooks() {
    // Shape of this repository's .claude/settings.json (PreCompact + SessionStart).
    let value = json!({
        "PreCompact": [{"matcher": "", "hooks": [{"type": "command", "command": "bd prime"}]}],
        "SessionStart": [{"matcher": "", "hooks": [{"type": "command", "command": "bd prime"}]}],
        "Notification": [{"hooks": [{"type": "command", "command": "notify"}]}],
        "PermissionRequest": [{"hooks": [{"type": "command", "command": "perm"}]}],
        "FutureEvent": [{"hooks": [{"type": "command", "command": "later"}]}]
    });
    let config = parse_hooks_config(&value).expect("unknown events must not fail the file");
    for event in [
        "PreCompact",
        "SessionStart",
        "Notification",
        "PermissionRequest",
    ] {
        assert_eq!(config.get(event).map(Vec::len), Some(1), "{event}");
    }
}
