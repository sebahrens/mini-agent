/// Canonicalizes a tool name into zerostack's own name, accepting either
/// zerostack's own name or Claude Code's equivalent name. Case-insensitive
/// and idempotent (already-canonical names pass through unchanged).
pub(crate) fn canonical_tool_name(name: &str) -> String {
    let lower = to_snake_case(name);
    match lower.as_str() {
        "glob" => "find_files".to_string(),
        _ => lower,
    }
}

fn to_snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}
