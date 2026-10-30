//! Bounded configuration discovery and literal command-line overrides.
//! Config source roots never replace the working directory or grant file access.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::paths::WorkspaceBinding;

pub(super) const MAX_LAYER_BYTES: usize = 256 * 1024;
const MAX_ANCESTORS: usize = 64;
const MAX_DEPTH: usize = 32;
const MAX_ITEMS: usize = 4096;

pub(crate) struct ProjectLayers {
    pub(crate) source: Arc<WorkspaceBinding>,
    pub(crate) project: Option<PathBuf>,
    pub(crate) local: Option<PathBuf>,
}

fn exists(path: &Path) -> Result<bool, &'static str> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("config source metadata is unavailable"),
    }
}

/// Nearest pair, with a conservative repository/home/root ceiling. A present
/// link or malformed file is selected for refusal, never bypassed for a parent.
pub(crate) fn discover(
    workspace: &Arc<WorkspaceBinding>,
    home: Option<&Path>,
) -> Result<Option<ProjectLayers>, &'static str> {
    workspace.validate().map_err(|_| "workspace changed")?;
    let home = home.and_then(|path| path.canonicalize().ok());
    let mut root = workspace.root().to_path_buf();
    for level in 0..MAX_ANCESTORS {
        if root.parent().is_none() || (level > 0 && home.as_ref() == Some(&root)) {
            return Ok(None);
        }
        let source = if level == 0 {
            Arc::clone(workspace)
        } else {
            Arc::new(WorkspaceBinding::capture(&root).map_err(|_| "config root unavailable")?)
        };
        source.validate().map_err(|_| "config root changed")?;
        match std::fs::symlink_metadata(root.join(".mini-agent")) {
            Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
                return Err("nearest config directory is not a regular directory");
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err("nearest config directory is unavailable");
            }
            _ => {}
        }
        let project = root.join(".mini-agent/config.toml");
        let local = root.join(".mini-agent/config.local.toml");
        let has_project = exists(&project)?;
        let has_local = exists(&local)?;
        workspace.validate().map_err(|_| "workspace changed")?;
        source.validate().map_err(|_| "config root changed")?;
        if has_project || has_local {
            return Ok(Some(ProjectLayers {
                source,
                project: has_project.then_some(project),
                local: has_local.then_some(local),
            }));
        }
        if exists(&root.join(".git"))? || home.as_ref() == Some(&root) {
            return Ok(None);
        }
        root = root
            .parent()
            .ok_or("config root unavailable")?
            .to_path_buf();
    }
    Err("config discovery exceeds directory limit")
}

/// Preflight before TOML allocates arrays or nested tables. Quotes/comments are
/// opaque; nesting and dotted key paths share a finite depth budget.
pub(super) fn validate_toml_bounds(text: &str) -> Result<(), &'static str> {
    validate_toml_bounds_with_limit(text, MAX_LAYER_BYTES)
}

pub(super) fn validate_toml_bounds_with_limit(
    text: &str,
    byte_limit: usize,
) -> Result<(), &'static str> {
    validate_toml_bounds_at(text, byte_limit)
        .map(drop)
        .map_err(|(message, _)| message)
}

/// Static diagnostic plus the bounded scanner's exact byte position. Never
/// parse past a rejected allocation bound or include a caller-supplied value.
/// On success it returns the sorted, de-duplicated spellings of the top-level
/// keys (the first segment of each root key path and `[table]` header), the
/// same list `toml::Table::keys()` would give for valid TOML.
pub(super) fn validate_toml_bounds_diagnostic(
    text: &str,
    byte_limit: usize,
) -> Result<Vec<String>, String> {
    validate_toml_bounds_at(text, byte_limit).map_err(|(message, offset)| {
        let (line, column) = super::keys::line_column(text, offset);
        format!("line {line}, column {column}: {message} (values omitted)")
    })
}

/// First segment of a TOML key path (`a . "b"` -> `a`). Quoted segments with
/// escapes are decoded by the TOML parser itself so spellings match exactly.
fn first_key_segment(path: &str) -> Option<String> {
    let path = path.trim_start();
    let first = *path.as_bytes().first()?;
    match first {
        b'\'' => {
            let end = path[1..].find('\'')?;
            Some(path[1..1 + end].to_string())
        }
        b'"' => {
            let bytes = path.as_bytes();
            let mut index = 1;
            while index < bytes.len() {
                match bytes[index] {
                    b'\\' => index += 2,
                    b'"' => break,
                    _ => index += 1,
                }
            }
            let quoted = path.get(..=index)?;
            if !quoted.contains('\\') {
                return Some(quoted[1..quoted.len() - 1].to_string());
            }
            let table: toml::Table = toml::from_str(&format!("{quoted}=0")).ok()?;
            table.into_iter().next().map(|(key, _)| key)
        }
        _ => {
            let end = path
                .find(|c: char| c == '.' || c.is_whitespace())
                .unwrap_or(path.len());
            Some(path[..end].to_string())
        }
    }
}

fn validate_toml_bounds_at(
    text: &str,
    byte_limit: usize,
) -> Result<Vec<String>, (&'static str, usize)> {
    if text.len() > byte_limit {
        return Err(("config layer exceeds byte limit", 0));
    }
    #[derive(Clone, Copy)]
    struct Frame {
        close: u8,
        key: bool,
        dots: usize,
        start: usize,
    }
    let bytes = text.as_bytes();
    let mut stack: Vec<Frame> = Vec::with_capacity(MAX_DEPTH);
    let mut quote: Option<(u8, bool, usize)> = None;
    let mut table_depth = 0;
    let mut header = false;
    let mut line_start = true;
    let mut root_key = true;
    let mut root_dots = 0;
    let mut items = 0;
    // Top-level key spellings, collected from the tokens already walked here.
    let mut top_keys = std::collections::BTreeSet::new();
    let mut key_start: Option<usize> = None;
    let mut header_start = 0;
    // Keys after the first `[table]` header belong to that table.
    let mut in_table = false;
    // A leading byte-order mark is not part of the first key.
    let mut index = if text.starts_with('\u{feff}') { 3 } else { 0 };
    while index < bytes.len() {
        let byte = bytes[index];
        if quote.is_none()
            && key_start.is_none()
            && stack.is_empty()
            && root_key
            && !header
            && !in_table
            && !byte.is_ascii_whitespace()
            && byte != b'#'
            && byte != b'['
        {
            key_start = Some(index);
        }
        if let Some((delimiter, triple, _)) = quote {
            if delimiter == b'"' && byte == b'\\' {
                index = index.saturating_add(2);
                continue;
            }
            if byte == delimiter
                && (!triple || bytes.get(index..index + 3) == Some(&[delimiter; 3]))
            {
                quote = None;
                index += if triple { 3 } else { 1 };
                continue;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => {
                let triple = bytes.get(index..index + 3) == Some(&[byte; 3]);
                quote = Some((byte, triple, index));
                line_start = false;
                index += if triple { 3 } else { 1 };
                continue;
            }
            b'#' => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                continue;
            }
            b'\n' if stack.is_empty() => {
                line_start = true;
                root_key = true;
                root_dots = 0;
                header = false;
                key_start = None;
            }
            b'[' | b'{' => {
                if byte == b'[' && line_start && stack.is_empty() {
                    header = true;
                    header_start = index;
                    in_table = true;
                    items += 1;
                    table_depth = 0;
                }
                if stack.len()
                    + table_depth
                    + root_dots
                    + stack.iter().map(|frame| frame.dots).sum::<usize>()
                    >= MAX_DEPTH
                {
                    return Err(("config layer exceeds nesting limit", index));
                }
                stack.push(Frame {
                    close: if byte == b'[' { b']' } else { b'}' },
                    key: byte == b'{',
                    dots: 0,
                    start: index,
                });
            }
            b']' | b'}' => {
                let frame = stack
                    .pop()
                    .ok_or(("config delimiters are invalid", index))?;
                if frame.close != byte {
                    return Err(("config delimiters are invalid", index));
                }
                if header && stack.is_empty() {
                    let name = text[header_start..index].trim_start_matches('[');
                    top_keys.extend(first_key_segment(name));
                    table_depth = root_dots + 1;
                    root_dots = 0;
                }
            }
            b'=' => {
                items += 1;
                if stack.is_empty()
                    && root_key
                    && let Some(start) = key_start.take()
                {
                    top_keys.extend(first_key_segment(&text[start..index]));
                }
                root_key = false;
                if let Some(frame) = stack.last_mut() {
                    frame.key = false;
                }
            }
            b',' => {
                items += 1;
                if let Some(frame) = stack.last_mut()
                    && frame.close == b'}'
                {
                    frame.key = true;
                    frame.dots = 0;
                }
            }
            b'.' => {
                if header || (stack.is_empty() && root_key) {
                    root_dots += 1;
                    if root_dots + table_depth + stack.len() >= MAX_DEPTH {
                        return Err(("config layer exceeds key depth limit", index));
                    }
                } else if let Some(frame) = stack.last_mut()
                    && frame.key
                {
                    frame.dots += 1;
                    if frame.dots + table_depth + stack.len() >= MAX_DEPTH {
                        return Err(("config layer exceeds key depth limit", index));
                    }
                }
            }
            _ => {}
        }
        if table_depth
            + root_dots
            + stack.len()
            + stack.iter().map(|frame| frame.dots).sum::<usize>()
            > MAX_DEPTH
        {
            return Err(("config layer exceeds nesting limit", index));
        }
        if items > MAX_ITEMS {
            return Err(("config layer exceeds item limit", index));
        }
        if !byte.is_ascii_whitespace() {
            line_start = false;
        }
        index += 1;
    }
    if let Some((_, _, start)) = quote {
        return Err(("config delimiters are invalid", start));
    }
    if let Some(frame) = stack.first() {
        return Err(("config delimiters are invalid", frame.start));
    }
    Ok(top_keys.into_iter().collect())
}

/// Parse repeated operator overrides as bounded literal TOML. Diagnostics never
/// include the supplied key or value (a key can itself contain a credential).
pub(crate) fn parse_overrides(args: &[String]) -> Result<Vec<toml::Value>, &'static str> {
    if args.len() > 64 || args.iter().map(String::len).sum::<usize>() > MAX_LAYER_BYTES {
        return Err("too many or oversized config overrides (values omitted)");
    }
    let validator = jsonschema::validator_for(&super::schema::generate())
        .map_err(|_| "config schema is unavailable")?;
    let mut result = Vec::with_capacity(args.len());
    for argument in args {
        if argument.len() > 32 * 1024 {
            return Err("config override exceeds 32 KiB (values omitted)");
        }
        validate_toml_bounds(argument)?;
        let mut quote = None;
        let mut escaped = false;
        let mut separator = None;
        for (at, character) in argument.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if quote == Some('"') && character == '\\' {
                escaped = true;
                continue;
            }
            if let Some(active) = quote {
                if character == active {
                    quote = None;
                }
            } else if character == '\'' || character == '"' {
                quote = Some(character);
            } else if character == '=' {
                separator = Some(at);
                break;
            }
        }
        let at = separator.ok_or("config override requires key=literal TOML (values omitted)")?;
        let (key, value) = (&argument[..at], &argument[at + 1..]);
        let mut path: toml::Value = toml::from_str(&format!("{key}=0"))
            .map_err(|_| "invalid config override path (values omitted)")?;
        let mut literal: toml::Value = toml::from_str(&format!("value={value}"))
            .map_err(|_| "invalid config override literal (values omitted)")?;
        let table = literal
            .as_table_mut()
            .ok_or("invalid config override literal")?;
        if table.len() != 1 || !table.contains_key("value") {
            return Err("config override contains more than one value (values omitted)");
        }
        let literal = table.remove("value").expect("validated literal");
        let root = path.as_table_mut().ok_or("invalid config override path")?;
        if root.len() != 1 || super::keys::lookup(root.keys().next().expect("one key")).is_none() {
            return Err("unknown config override key (values omitted)");
        }
        super::keys::canonicalize_top_level_keys(root)
            .map_err(|_| "ambiguous config override key (values omitted)")?;
        let mut leaf = root.iter_mut().next().expect("one key").1;
        let mut depth = 1;
        while let toml::Value::Table(table) = leaf {
            if table.len() != 1 || depth >= MAX_DEPTH {
                return Err("invalid config override path");
            }
            leaf = table.iter_mut().next().expect("one key").1;
            depth += 1;
        }
        if leaf.as_integer() != Some(0) {
            return Err("invalid config override path");
        }
        *leaf = literal;
        super::finite::validate(&path)?;
        let json = serde_json::to_value(&path).map_err(|_| "invalid config override literal")?;
        if !validator.is_valid(&json) {
            return Err("config override has an invalid type or value (values omitted)");
        }
        let (key, _) = path
            .as_table()
            .expect("validated table")
            .iter()
            .next()
            .expect("one key");
        // Dynamic integration objects can be valid partial overlays (for
        // example args on an existing MCP command). Validate their complete
        // typed result only after composing with the captured file layers.
        if !matches!(
            super::keys::lookup(key).expect("known key").shape,
            super::keys::ConfigShape::Object
        ) {
            let _: super::Config = path
                .clone()
                .try_into()
                .map_err(|_| "config override has an invalid type or value (values omitted)")?;
        }
        result.push(path);
    }
    Ok(result)
}

#[cfg(feature = "mcp")]
#[derive(Clone, Default)]
pub(crate) struct McpOverrides(Vec<toml::Value>);

#[cfg(feature = "mcp")]
impl std::fmt::Debug for McpOverrides {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOverrides")
            .field("count", &self.0.len())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "mcp")]
impl McpOverrides {
    pub(crate) fn apply(&self, cfg: &mut super::Config) -> Result<(), &'static str> {
        let mut candidate = cfg.clone();
        candidate.mcp_cli_overrides = Self::default();
        apply_overrides(&mut candidate, &self.0)?;
        *cfg = candidate;
        Ok(())
    }
    fn extend(&mut self, overrides: &[toml::Value]) -> Result<(), &'static str> {
        for local in overrides {
            let table = local.as_table().expect("validated override table");
            let relevant: toml::map::Map<_, _> = table
                .iter()
                .filter(|(name, _)| {
                    super::keys::lookup(name).is_some_and(|key| {
                        key.feature == Some("mcp") || key.name == "allow_all_mcp_calls"
                    })
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            if !relevant.is_empty() {
                if self.0.len() >= 64 {
                    return Err("configured MCP override count exceeds bounds");
                }
                self.0.push(toml::Value::Table(relevant));
            }
        }
        Ok(())
    }
}

/// Typed configuration can contain JSON nulls and unsigned values that TOML
/// cannot represent. Normalize through bounded JSON without changing the input
/// layer dialect or its separate pre-parse limits.
pub(crate) fn normalized_json<T: serde::Serialize>(
    value: &T,
) -> Result<serde_json::Value, &'static str> {
    super::finite::validate(value)?;
    const MAX_BYTES: usize = 8 * 1024 * 1024;
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_BYTES.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other(
                    "normalized config exceeds byte limit",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut encoded = Bounded(Vec::new());
    serde_json::to_writer(&mut encoded, value)
        .map_err(|_| "config cannot be encoded within bounds")?;
    validate_json_structure(&encoded.0)?;
    serde_json::from_slice(&encoded.0).map_err(|_| "config cannot be decoded within bounds")
}

fn validate_json_structure(bytes: &[u8]) -> Result<(), &'static str> {
    let mut depth = 0usize;
    let mut items = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for &byte in bytes {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'{' | b'[' => {
                depth += 1;
                items += 1;
            }
            b'}' | b']' => depth = depth.checked_sub(1).ok_or("invalid normalized config")?,
            b',' | b':' => items += 1,
            _ => {}
        }
        if depth > 128 || items > 65_536 {
            return Err("normalized config exceeds structure limits");
        }
    }
    if depth != 0 || quoted {
        return Err("invalid normalized config");
    }
    Ok(())
}

pub(super) fn deep_merge_json(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => deep_merge_json(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

/// Explicit operator overrides may set financial policy; project layers may not.
pub(crate) fn apply_overrides(
    cfg: &mut super::Config,
    overrides: &[toml::Value],
) -> Result<(), &'static str> {
    if overrides.is_empty() {
        return Ok(());
    }
    let mut serializable = cfg.clone();
    super::load::strip_injected_mcp_defaults(&mut serializable);
    let mut value = normalized_json(&serializable)?;
    let mut provenance = cfg.provenance.clone();
    for local in overrides {
        provenance.record(local, super::ConfigLayer::Cli);
        deep_merge_json(&mut value, normalized_json(local)?);
        value = normalized_json(&value)?;
    }
    let mut merged: super::Config = serde_json::from_value(value)
        .map_err(|_| "config override has an invalid type or value (values omitted)")?;
    merged.provenance = provenance;
    merged.load_warnings = cfg.load_warnings.clone();
    let clamped = merged.retry.clamp_to_limits();
    merged.load_warnings.extend(
        clamped
            .into_iter()
            .map(|_| "CLI retry policy was clamped".into()),
    );
    let body_warnings = super::load::validate_extra_body(&mut merged)
        .map_err(|_| "CLI provider request settings are invalid (values omitted)")?;
    merged.load_warnings.extend(std::iter::repeat_n(
        "CLI provider request setting was ignored (name/value omitted)".into(),
        body_warnings.len(),
    ));
    #[cfg(feature = "mcp")]
    {
        if merged.mcp_servers.as_ref().is_some_and(|servers| {
            servers.len() > 128
                || servers.keys().any(|name| {
                    name.is_empty() || name.len() > 256 || name.chars().any(char::is_control)
                })
        }) {
            return Err("configured MCP catalogue exceeds bounds");
        }
        merged.mcp_origins = cfg.mcp_origins.clone();
        merged.mcp_cli_overrides = cfg.mcp_cli_overrides.clone();
        merged.mcp_cli_overrides.extend(overrides)?;
        for local in overrides {
            if let Some(servers) = local.get("mcp_servers").and_then(toml::Value::as_table) {
                if servers.len() > 128 {
                    return Err("configured MCP server count exceeds bounds");
                }
                for name in servers.keys() {
                    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
                        return Err("configured MCP name exceeds bounds");
                    }
                    let origins = merged.mcp_origins.entry(name.clone()).or_default();
                    if origins.is_empty()
                        && cfg.provenance.global_keys.contains("mcp_servers")
                        && cfg
                            .mcp_servers
                            .as_ref()
                            .is_some_and(|servers| servers.contains_key(name))
                    {
                        return Err("configured MCP source has no captured authority");
                    }
                    let prior = origins
                        .iter()
                        .find(|origin| origin.selected)
                        .and_then(|origin| origin.authority.as_ref());
                    // A legacy declared file origin cannot turn into originless
                    // CLI authority merely because an effective value changed.
                    if origins.iter().any(|origin| {
                        origin.selected && origin.authority.is_none() && origin.source != "built-in"
                    }) {
                        return Err("configured MCP source has no captured authority");
                    }
                    let authority =
                        super::super::extras::mcp::project::OriginAuthority::operator_cli(prior);
                    for origin in origins.iter_mut() {
                        origin.selected = false;
                    }
                    origins.retain(|origin| origin.source != "CLI --config");
                    origins.push(crate::extras::mcp::project::ServerOrigin {
                        source: "CLI --config".into(),
                        selected: true,
                        inactive: None,
                        digest: None,
                        authority: Some(authority),
                    });
                }
            }
        }
    }
    #[cfg(feature = "mcp")]
    super::load::inject_mcp_defaults(&mut merged);
    *cfg = merged;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_scanner_top_level_keys_match_toml_table_keys() {
        let documents = [
            "model = \"x\"\ntemperature = 0.5\n",
            "\"quoted key\" = 1\n'literal key' = 2\nbare-key = 3\n",
            "\"esc\\u0041ped\" = 1\nplain = 2\n",
            "a.b.c = 1\na.d = 2\nz = 3\n",
            "  indented = 1\n# comment = 2\n\n  \"a.b\" . c = 3\n",
            "top = 1\n[a.b]\nx = 1\n[\"q r\".s]\ny = 2\n[[arr.item]]\nz = 3\n[[arr.item]]\nz = 4\n",
            "list = [\n  1,\n  2,\n]\ninline = { k = 1, \"j\" = [1, 2] }\nmulti = \"\"\"\nnot_a_key = 1\n\"\"\"\n[ spaced . header ]\nv = 1\n",
            "\u{feff}model = 1\n",
            "a = 1 # trailing = 2\n[t] # [fake]\nb = 2\n",
            "mcp_servers.one.command = \"x\"\n[mcp_servers.two]\ncommand = \"y\"\n",
            "",
        ];
        for document in documents {
            let expected: Vec<String> = document
                .parse::<toml::Table>()
                .unwrap_or_else(|error| panic!("{document:?}: {error}"))
                .keys()
                .cloned()
                .collect();
            let scanned = validate_toml_bounds_diagnostic(document, MAX_LAYER_BYTES).unwrap();
            assert_eq!(scanned, expected, "{document:?}");
        }
    }

    #[test]
    fn config69_keyboard_literals_match_schema_and_typed_validation() {
        let mut cfg = crate::config::Config::default();
        let overrides = parse_overrides(&[
            "keys.line_start=['f2']".into(),
            "keys.undo=[]".into(),
            "vim_mode=true".into(),
        ])
        .unwrap();
        apply_overrides(&mut cfg, &overrides).unwrap();
        assert_eq!(cfg.vim_mode, Some(true));
        let bindings =
            crate::config::keybindings::KeyBindings::from_overrides(cfg.keys.as_ref()).unwrap();
        let f2 = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::F(2),
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(
            bindings.action(f2, true),
            Some(crate::config::keybindings::KeyAction::LineStart)
        );
        for invalid in [
            "keys.line_start=42",
            "vim_mode='yes'",
            "keys.unknown_action=['f2']",
        ] {
            assert!(parse_overrides(&[invalid.into()]).is_err());
        }
        assert_eq!(cfg.keys.unwrap()["line_start"], ["f2"]);
    }

    #[test]
    fn config69_json_null_global_fields_survive_literal_override() {
        let mut cfg: super::super::Config = serde_json::from_value(serde_json::json!({
            "provider":"openrouter", "max_tokens":u64::MAX,
            "extra_body":{"metadata":{"nullable":null,"unsigned":u64::MAX,"values":[1,2]}}
        }))
        .unwrap();
        let direct: super::super::Config =
            toml::from_str("[extra_body.metadata]\ndate = 1979-05-27T07:32:00Z\n").unwrap();
        let expected_date = direct.extra_body.unwrap()["metadata"]["date"].clone();
        let preserved: crate::config::Config =
            toml::from_str("future_date=1979-05-27T07:32:00Z\n").unwrap();
        cfg.preserved = preserved.preserved;
        let preserved_before = toml::to_string(&cfg.preserved).unwrap();
        let overrides = parse_overrides(&[
            "temperature=0.5".into(),
            "extra_body.metadata.date=1979-05-27T07:32:00Z".into(),
            "extra_body.metadata.values=[3]".into(),
        ])
        .unwrap();
        apply_overrides(&mut cfg, &overrides).unwrap();
        assert_eq!(cfg.max_tokens, Some(u64::MAX));
        assert_eq!(cfg.temperature, Some(0.5));
        let metadata = &cfg.extra_body.as_ref().unwrap()["metadata"];
        assert_eq!(metadata["nullable"], serde_json::Value::Null);
        assert_eq!(metadata["unsigned"].as_u64(), Some(u64::MAX));
        assert_eq!(metadata["values"], serde_json::json!([3]));
        assert_eq!(metadata["date"], expected_date);
        assert_eq!(toml::to_string(&cfg.preserved).unwrap(), preserved_before);
    }

    #[test]
    fn config69_nonfinite_toml_cannot_be_silently_erased_by_json_merge() {
        let mut cfg: super::super::Config = toml::from_str("temperature=nan\n").unwrap();
        assert!(cfg.temperature.unwrap().is_nan());
        let overrides = parse_overrides(&["max_tokens=64".into()]).unwrap();
        assert!(apply_overrides(&mut cfg, &overrides).is_err());
        assert!(cfg.temperature.unwrap().is_nan());
        assert_eq!(cfg.max_tokens, None);
        for literal in [
            "max_budget_usd=nan",
            "max_budget_usd=inf",
            "extra_body.metadata.secret=nan",
        ] {
            let error = parse_overrides(&[literal.into()]).unwrap_err();
            assert_eq!(
                error,
                "config normalization refuses non-finite values or excessive structure"
            );
            assert!(!error.contains("secret"));
        }
        for field in ["max_budget_usd", "future_price"] {
            let mut cfg: super::super::Config = if field == "max_budget_usd" {
                // The decoder already rejects an invalid saved cap; public
                // runtime fields must also fail closed at normalization.
                super::super::Config {
                    max_budget_usd: Some(f64::NAN),
                    ..Default::default()
                }
            } else {
                toml::from_str(&format!("{field}=nan\n")).unwrap()
            };
            let before = toml::to_string(&cfg).unwrap();
            assert!(apply_overrides(&mut cfg, &overrides).is_err());
            assert_eq!(toml::to_string(&cfg).unwrap(), before);
        }
    }

    #[test]
    fn config69_normalization_limits_opaque_strings_depth_and_allocation() {
        let quoted = serde_json::json!({"text":"\" \\ [[[[ {{{{ ,,, :::"});
        assert_eq!(normalized_json(&quoted).unwrap(), quoted);
        assert!(normalized_json(&"x".repeat(8 * 1024 * 1024)).is_err());
        assert!(
            validate_json_structure(format!("{}0{}", "[".repeat(129), "]".repeat(129)).as_bytes())
                .is_err()
        );
        assert!(normalized_json(&vec![0; 65_537]).is_err());
        let mut cfg = super::super::Config {
            model: Some("original".into()),
            ..Default::default()
        };
        cfg.extra_body =
            Some(serde_json::json!({"metadata":{"large":"x".repeat(8 * 1024 * 1024)}}));
        assert!(
            apply_overrides(
                &mut cfg,
                &parse_overrides(&["model='replacement'".into()]).unwrap()
            )
            .is_err()
        );
        assert_eq!(cfg.model.as_deref(), Some("original"));
    }

    #[test]
    fn config69_literal_cli_order_aliases_and_redacted_errors() {
        let arguments = vec![
            "model=\"first\"".into(),
            "model=\"second\"".into(),
            "permission_deny.read=[\"secret/**\"]".into(),
            "format.\"rs\"=\"printf '$ENV $(touch sentinel)' {file}\"".into(),
            "max_budget_usd=0.25".into(),
        ];
        let parsed = parse_overrides(&arguments).unwrap();
        let mut cfg = super::super::Config::default();
        apply_overrides(&mut cfg, &parsed).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("second"));
        assert_eq!(cfg.max_budget_usd, Some(0.25));
        assert_eq!(cfg.permission_deny.as_ref().unwrap()["read"], ["secret/**"]);
        assert_eq!(cfg.format["rs"], "printf '$ENV $(touch sentinel)' {file}");
        assert_eq!(
            cfg.provenance.sources["model"],
            [super::super::ConfigLayer::Cli].into()
        );
        for argument in [
            "secret-header=\"credential\"",
            "model=\"credential\"\nother=1",
            "model=unquotedcredential",
            "model=\"credential\"; touch sentinel",
            "model=1",
        ] {
            let error = parse_overrides(&[argument.into()])
                .and_then(|values| apply_overrides(&mut cfg, &values))
                .unwrap_err();
            assert!(!error.contains("credential"));
            assert!(!error.contains("secret-header"));
            assert_eq!(cfg.model.as_deref(), Some("second"));
        }
        assert!(parse_overrides(&vec!["model=\"x\"".into(); 65]).is_err());
    }

    #[test]
    fn config69_bounds_handle_quoted_controls_and_limit_structure_before_parse() {
        for text in [
            "model = 'brackets [[[[ and dots.......'\n",
            "[retry]\nmax_attempts = 2\n",
            "a.b = { c = [1.5, 2.5], d = { e = true } }\n",
            "text = \"\"\"#comment-like\n[[{{...\"\"\"\n",
            "[\"a.b\".c]\nx = 1\n",
        ] {
            assert!(validate_toml_bounds(text).is_ok(), "{text}");
            toml::from_str::<toml::Value>(text).unwrap();
        }
        let deep_array = format!("x = {}0{}", "[".repeat(33), "]".repeat(33));
        let deep_path = format!("{}x = 1", "a.".repeat(33));
        let many_values = format!("x = [{}]", "0,".repeat(MAX_ITEMS + 1));
        for text in [
            deep_array,
            deep_path,
            many_values,
            "#".repeat(MAX_LAYER_BYTES + 1),
        ] {
            assert!(validate_toml_bounds(&text).is_err());
        }
        assert!(validate_toml_bounds("x = [[0]").is_err());
        assert!(validate_toml_bounds("x = 'unfinished").is_err());
    }

    #[test]
    fn config69_discovery_nearest_pair_local_only_and_git_ceiling() {
        let root = crate::tests::OwnedTestDirectory::new("config69-discovery");
        let repo = root.path().join("repo");
        let child = repo.join("src/nested");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();
        std::fs::create_dir(repo.join(".mini-agent")).unwrap();
        std::fs::write(repo.join(".mini-agent/config.toml"), "model='repo'").unwrap();
        let workspace = Arc::new(WorkspaceBinding::capture(&child).unwrap());
        let found = discover(&workspace, None).unwrap().unwrap();
        assert_eq!(found.source.root(), repo.canonicalize().unwrap());
        assert!(found.local.is_none());
        assert_eq!(workspace.root(), child.canonicalize().unwrap());
        std::fs::create_dir(child.join(".mini-agent")).unwrap();
        std::fs::write(child.join(".mini-agent/config.local.toml"), "model='local'").unwrap();
        let found = discover(&workspace, None).unwrap().unwrap();
        assert_eq!(found.source.root(), workspace.root());
        assert!(found.project.is_none());
        assert!(found.local.is_some());
        std::fs::remove_dir_all(child.join(".mini-agent")).unwrap();
        std::fs::write(child.join(".git"), "gitdir: ignored external target").unwrap();
        assert!(discover(&workspace, None).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn config69_discovery_link_nearest_home_and_replaced_root_fail_closed() {
        let root = crate::tests::OwnedTestDirectory::new("config69-discovery-refusal");
        let home = root.path().join("home");
        let child = home.join("work");
        std::fs::create_dir_all(home.join(".mini-agent")).unwrap();
        std::fs::create_dir(&child).unwrap();
        std::fs::write(home.join(".mini-agent/config.toml"), "model='home'").unwrap();
        let workspace = Arc::new(WorkspaceBinding::capture(&child).unwrap());
        assert!(discover(&workspace, Some(&home)).unwrap().is_none());
        std::os::unix::fs::symlink(home.join(".mini-agent"), child.join(".mini-agent")).unwrap();
        assert!(discover(&workspace, Some(&home)).is_err());
        std::fs::remove_file(child.join(".mini-agent")).unwrap();
        std::fs::rename(&child, home.join("old-work")).unwrap();
        std::fs::create_dir(&child).unwrap();
        assert!(discover(&workspace, Some(&home)).is_err());
    }
}
