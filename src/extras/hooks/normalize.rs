/// Canonicalizes a tool name into mini-agent's own name, accepting either
/// mini-agent's own name or Claude Code's equivalent name. Case-insensitive
/// and idempotent for builtins. Explicit `mcp__` matcher labels retain raw
/// component case; they are never parsed back into MCP provenance.
pub(crate) fn canonical_tool_name(name: &str) -> String {
    if name.starts_with("mcp__") {
        return name.to_owned();
    }
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

/// Parent-captured MCP provenance, retained separately from its model alias.
/// Never reconstruct this tuple from a registered or canonical name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct McpToolIdentity {
    pub(crate) server: String,
    pub(crate) tool: String,
}

impl McpToolIdentity {
    pub(crate) fn matcher_name(&self) -> String {
        format!("mcp__{}__{}", self.server, self.tool)
    }

    pub(crate) fn permission_input(&self) -> String {
        format!("mcp_tool:{}:{}", self.server, self.tool)
    }
}
