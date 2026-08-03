use crate::extras::export::{
    MAX_SESSION_IMPORT_BYTES, MAX_SESSION_IMPORT_MESSAGES, ParsedSessionFile, parse_jsonl_import,
    parse_session_file, session_to_html, session_to_jsonl,
};
use crate::session::{MessageRole, Session};

fn sample_session() -> Session {
    let mut session = Session::new("openrouter", "test-model", 128_000, "demo session");
    session.add_message(MessageRole::User, "hello there");
    session.add_message(MessageRole::Assistant, "hi! **how** can I help?");
    session.add_message(MessageRole::ToolCall, "bash: ls -la");
    session.add_message(MessageRole::ToolResult, "bash:\ntotal 0");
    session
}

#[test]
fn jsonl_round_trip_preserves_messages() {
    let session = sample_session();
    let jsonl = session_to_jsonl(&session).unwrap();
    let messages = parse_jsonl_import(&jsonl).unwrap();
    assert_eq!(messages.len(), session.messages.len());
    for (imported, original) in messages.iter().zip(session.messages.iter()) {
        assert_eq!(imported.role, original.role);
        assert_eq!(imported.content, original.content);
    }
}

#[test]
fn jsonl_first_line_is_session_metadata() {
    let session = sample_session();
    let jsonl = session_to_jsonl(&session).unwrap();
    let header: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert_eq!(header["type"], "session");
    assert_eq!(header["name"], "demo session");
    assert_eq!(header["model"], "test-model");
}

#[test]
fn jsonl_import_accepts_bare_lines_without_metadata() {
    let jsonl =
        "{\"role\":\"user\",\"content\":\"hi\"}\n{\"role\":\"assistant\",\"content\":\"yo\"}\n";
    let messages = parse_jsonl_import(jsonl).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].content, "hi");
    assert_eq!(messages[1].role, MessageRole::Assistant);
}

#[test]
fn jsonl_import_errors_with_line_number_on_bad_json() {
    let jsonl = "{\"role\":\"user\",\"content\":\"ok\"}\nnot json\n";
    let err = parse_jsonl_import(jsonl).unwrap_err();
    assert!(
        err.to_string().contains("line 2"),
        "error should name the line: {err}"
    );
}

#[test]
fn jsonl_import_errors_when_no_messages() {
    let err = parse_jsonl_import("{\"type\":\"session\"}\n").unwrap_err();
    assert!(err.to_string().contains("no messages"));
}

#[test]
fn exported_jsonl_schema_preserves_metadata_and_messages() {
    let session = sample_session();
    let jsonl = session_to_jsonl(&session).unwrap();
    let ParsedSessionFile::Jsonl(imported) = parse_session_file(&jsonl).unwrap() else {
        panic!("exported JSONL must select the JSONL schema");
    };
    assert_eq!(imported.id, session.id);
    assert_eq!(imported.name, session.name);
    assert_eq!(imported.provider, session.provider);
    assert_eq!(imported.model, session.model);
    assert_eq!(imported.created_at, session.created_at);
    assert_eq!(imported.messages.len(), session.messages.len());
    for (imported, original) in imported.messages.iter().zip(&session.messages) {
        assert_eq!(imported.role, original.role);
        assert_eq!(imported.content, original.content);
        assert_eq!(imported.estimated_tokens, original.estimated_tokens);
    }
}

#[test]
fn native_pretty_json_with_bom_is_detected_and_fully_consumed() {
    let session = sample_session();
    let pretty = serde_json::to_string_pretty(&session).unwrap();
    let content = format!("\u{feff}  {pretty}\n");
    let ParsedSessionFile::Native(imported) = parse_session_file(&content).unwrap() else {
        panic!("pretty native JSON must select the native schema");
    };
    assert_eq!(imported.id, session.id);
    assert_eq!(imported.messages.len(), session.messages.len());

    let error = parse_session_file(&format!("{pretty}\n{{}}"))
        .err()
        .expect("trailing native JSON value must fail");
    assert!(error.to_string().contains("trailing values"));
}

#[test]
fn session_file_detection_rejects_ambiguous_and_unsupported_schemas() {
    let session = sample_session();
    let mut ambiguous = serde_json::to_value(&session).unwrap();
    let object = ambiguous.as_object_mut().unwrap();
    object.insert("type".into(), serde_json::json!("session"));
    object.insert(
        "format".into(),
        serde_json::json!("zerostack-session-jsonl"),
    );
    object.insert("version".into(), serde_json::json!(1));
    let error = parse_session_file(&ambiguous.to_string())
        .err()
        .expect("mixed schema must fail");
    assert!(error.to_string().contains("ambiguous"));

    let jsonl = session_to_jsonl(&session)
        .unwrap()
        .replacen("\"version\":1", "\"version\":2", 1);
    let error = parse_session_file(&jsonl)
        .err()
        .expect("unsupported version must fail");
    assert!(
        error
            .to_string()
            .contains("unsupported JSONL session version 2")
    );
}

#[test]
fn strict_jsonl_detection_reports_malformed_middle_line() {
    let session = sample_session();
    let mut lines = session_to_jsonl(&session)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    lines.insert(2, "not json".to_string());
    let error = parse_session_file(&lines.join("\n"))
        .err()
        .expect("malformed middle line must fail");
    assert!(error.to_string().contains("line 3 is not valid JSON"));
}

#[test]
fn session_file_detection_rejects_malformed_first_line_and_oversized_input() {
    let first_line_error = parse_session_file("not json\n{}")
        .err()
        .expect("malformed first line must fail before schema dispatch");
    assert!(
        first_line_error
            .to_string()
            .contains("first session value is not valid JSON")
    );

    let oversized = "x".repeat(MAX_SESSION_IMPORT_BYTES + 1);
    let oversized_error = parse_session_file(&oversized)
        .err()
        .expect("oversized input must fail before parsing");
    assert!(oversized_error.to_string().contains("byte limit"));
}

#[test]
fn strict_jsonl_detection_enforces_message_bound() {
    let session = sample_session();
    let header = session_to_jsonl(&session)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    let message = serde_json::json!({
        "role": "user",
        "content": "bounded",
        "estimated_tokens": 1,
    })
    .to_string();
    let mut jsonl = String::with_capacity(
        header.len() + (message.len() + 1) * (MAX_SESSION_IMPORT_MESSAGES + 1),
    );
    jsonl.push_str(&header);
    for _ in 0..=MAX_SESSION_IMPORT_MESSAGES {
        jsonl.push('\n');
        jsonl.push_str(&message);
    }

    let error = parse_session_file(&jsonl)
        .err()
        .expect("too many messages must fail");
    assert!(error.to_string().contains("more than 10000 messages"));
}

#[test]
fn bounded_jsonl_export_round_trips_empty_and_maximum_message_sessions() {
    let empty = Session::new("provider", "model", 128_000, "empty");
    let empty_jsonl = session_to_jsonl(&empty).unwrap();
    let ParsedSessionFile::Jsonl(empty_import) = parse_session_file(&empty_jsonl).unwrap() else {
        panic!("empty export must retain the JSONL schema");
    };
    assert!(empty_import.messages.is_empty());

    let mut maximum = Session::new("provider", "model", 128_000, "maximum");
    for _ in 0..MAX_SESSION_IMPORT_MESSAGES {
        maximum.add_message(MessageRole::User, "x");
    }
    let maximum_jsonl = session_to_jsonl(&maximum).unwrap();
    let ParsedSessionFile::Jsonl(maximum_import) = parse_session_file(&maximum_jsonl).unwrap()
    else {
        panic!("maximum-sized message collection must retain the JSONL schema");
    };
    assert_eq!(maximum_import.messages.len(), MAX_SESSION_IMPORT_MESSAGES);
}

#[test]
fn bounded_jsonl_export_refuses_outputs_its_importer_cannot_accept() {
    let mut too_many = Session::new("provider", "model", 128_000, "too many");
    for _ in 0..=MAX_SESSION_IMPORT_MESSAGES {
        too_many.add_message(MessageRole::User, "x");
    }
    assert!(
        session_to_jsonl(&too_many)
            .unwrap_err()
            .to_string()
            .contains("more than 10000 messages")
    );

    let mut line_too_large = Session::new("provider", "model", 128_000, "line too large");
    line_too_large.add_message(MessageRole::User, &"x".repeat(4 * 1024 * 1024));
    assert!(
        session_to_jsonl(&line_too_large)
            .unwrap_err()
            .to_string()
            .contains("line limit")
    );
}

#[test]
fn html_escapes_verbatim_content() {
    let mut session = Session::new("p", "m", 128_000, "x");
    session.add_message(MessageRole::User, "show me <script>alert(1)</script> & go");
    let html = session_to_html(&session);
    assert!(html.contains("&lt;script&gt;"), "user HTML must be escaped");
    assert!(html.contains("&amp;"), "ampersand must be escaped");
    assert!(!html.contains("<script>alert"));
}

#[test]
fn html_renders_assistant_markdown() {
    let session = sample_session();
    let html = session_to_html(&session);
    assert!(
        html.contains("<strong>how</strong>"),
        "assistant markdown should render: {html}"
    );
}

#[test]
fn html_export_sanitization_escapes_active_raw_html() {
    let mut session = Session::new("p", "m", 128_000, "x");
    session.add_message(
        MessageRole::Assistant,
        r#"<script>globalThis.pwned = true</script>
<img src=x onerror=alert(1)>
<svg onload=alert(1)><a href="javascript:alert(1)">x</a></svg>
<math><mtext href="javascript:alert(1)">x</mtext></math>
<iframe srcdoc="<script>alert(1)</script>"></iframe>
<object data="javascript:alert(1)"></object><embed src="data:text/html,x">
<style>body { background: url(javascript:alert(1)); }</style>"#,
    );

    let html = session_to_html(&session);
    let rendered_message = html
        .split_once("<div class=\"markdown\">")
        .and_then(|(_, rest)| rest.split_once("</div>"))
        .map(|(message, _)| message)
        .expect("assistant Markdown wrapper");
    for active_tag in [
        "<script", "<img", "<svg", "<math", "<iframe", "<object", "<embed", "<style",
    ] {
        assert!(
            !rendered_message.to_ascii_lowercase().contains(active_tag),
            "active tag survived export: {active_tag}: {html}"
        );
    }
    assert!(html.contains("&lt;script&gt;"));
    assert!(html.contains("&lt;svg onload=alert(1)&gt;"));
}

#[test]
fn html_export_sanitization_rejects_unsafe_markdown_urls() {
    let mut session = Session::new("p", "m", 128_000, "x");
    session.add_message(
        MessageRole::Assistant,
        r#"[script](javascript:alert(1))
[mixed](JaVaScRiPt:alert(1))
[entity](java&#x73;cript:alert(1))
[percent](%6a%61%76%61%73%63%72%69%70%74%3aalert(1))
[double](%256a%2561%2576%2561%2573%2563%2572%2569%2570%2574%253aalert(1))
[control](java%0ascript:alert(1))
[vb](vbscript:msgbox(1))
[data](data:text/html,<script>alert(1)</script>)
![image](data:image/svg+xml,<svg onload=alert(1)>)
![relative image](images/untrusted.svg)
[protocol relative](//example.invalid/path)"#,
    );

    let html = session_to_html(&session);
    let lowercase = html.to_ascii_lowercase();
    for unsafe_fragment in [
        "href=\"javascript:",
        "href=\"vbscript:",
        "href=\"data:",
        "src=\"data:",
        "src=\"images/untrusted.svg",
        "href=\"//example.invalid",
    ] {
        assert!(
            !lowercase.contains(unsafe_fragment),
            "unsafe URL survived export: {unsafe_fragment}: {html}"
        );
    }
    for label in [
        "script", "mixed", "entity", "percent", "double", "control", "vb", "data",
    ] {
        assert!(
            html.contains(label),
            "link label must remain readable: {html}"
        );
    }
}

#[test]
fn html_export_sanitization_preserves_safe_markdown_and_literal_code() {
    let mut session = Session::new("p", "m", 128_000, "x");
    session.add_message(
        MessageRole::Assistant,
        r#"# Heading

- one
- **two**

| key | value |
| --- | ----- |
| safe | yes |

[HTTPS link](https://example.com/a?q=1) [mail](mailto:user@example.com) [relative](notes/page.html)
![safe image](https://example.com/image.png)

```html
<script>alert(1)</script>
```"#,
    );

    let html = session_to_html(&session);
    assert!(html.contains("<h1>Heading</h1>"));
    assert!(html.contains("<li>one</li>"));
    assert!(html.contains("<strong>two</strong>"));
    assert!(html.contains("<table>"));
    assert!(html.contains("href=\"https://example.com/a?q=1\""));
    assert!(html.contains("href=\"mailto:user@example.com\""));
    assert!(html.contains("href=\"notes/page.html\""));
    assert!(html.contains("src=\"https://example.com/image.png\""));
    assert!(html.contains("<code class=\"language-html\">&lt;script&gt;"));
    assert!(!html.contains("<script>alert(1)</script>"));
}

#[test]
fn html_export_sanitization_applies_to_every_message_role() {
    let mut session = Session::new(
        "<svg onload=alert(1)>",
        "<img src=x onerror=alert(1)>",
        128_000,
        "</title><script>titlePwn()</script>",
    );
    for role in [
        MessageRole::User,
        MessageRole::Assistant,
        MessageRole::System,
        MessageRole::ToolCall,
        MessageRole::ToolResult,
        MessageRole::SubagentToolCall,
    ] {
        session.add_message(role, "<script>alert(1)</script>");
    }

    let html = session_to_html(&session);
    assert!(!html.contains("<script>alert(1)</script>"));
    assert!(!html.contains("<img src=x onerror=alert(1)>"));
    assert!(!html.contains("<svg onload=alert(1)>"));
    assert!(!html.contains("</title><script>"));
    assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
    assert!(html.contains("&lt;svg onload=alert(1)&gt;"));
    assert_eq!(
        html.matches("&lt;script&gt;alert(1)&lt;/script&gt;")
            .count(),
        6
    );
}

#[test]
fn html_export_has_a_restrictive_content_security_policy() {
    let html = session_to_html(&sample_session());
    assert!(html.contains("default-src 'none'"));
    assert!(html.contains("script-src 'none'"));
    assert!(html.contains("object-src 'none'"));
    assert!(html.contains("base-uri 'none'"));
}

#[test]
fn html_contains_session_metadata() {
    let session = sample_session();
    let html = session_to_html(&session);
    assert!(html.contains("demo session"));
    assert!(html.contains("openrouter / test-model"));
    assert!(html.contains("<!DOCTYPE html>"));
}

#[test]
fn html_title_falls_back_to_session_id() {
    let session = Session::new("p", "m", 128_000, "");
    let html = session_to_html(&session);
    assert!(
        html.contains("zerostack session"),
        "unnamed sessions get an id-based title: {html}"
    );
}
