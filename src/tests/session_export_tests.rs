use crate::extras::export::{parse_jsonl_import, session_to_html, session_to_jsonl};
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
    let jsonl = session_to_jsonl(&session);
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
    let jsonl = session_to_jsonl(&session);
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
