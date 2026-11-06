use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSkillManifest {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    /// Preserved for interoperability only. This never grants a capability.
    pub allowed_tools: Option<String>,
    /// Exact immutable learned-JS identities associated with this instruction package.
    ///
    /// These references never activate, approve, or otherwise widen a learned skill.
    pub learned_js: Vec<String>,
    /// Claude Code `disable-model-invocation: true`: the skill is excluded from
    /// automatic selection, the catalog listing and `skills_search`, but can
    /// still be pinned explicitly with `/<name>`. Only ever narrows.
    pub disable_model_invocation: bool,
    /// Claude Code `user-invocable: false`: hidden from the `/` picker and the
    /// `/skills` list and not pinnable by the user. Defaults to true.
    pub user_invocable: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("SKILL.md must be UTF-8")]
    NonUtf8,
    #[error("SKILL.md must start with YAML frontmatter delimited by --- lines")]
    MissingFrontmatter,
    #[error("invalid Agent Skills frontmatter: {0}")]
    InvalidYaml(#[from] serde_yaml_ng::Error),
    #[error("skill name must be 1-64 lowercase ASCII letters, digits, or hyphens")]
    InvalidName,
    #[error("skill name must not start or end with a hyphen or contain consecutive hyphens")]
    InvalidNameHyphens,
    #[error("skill description must contain 1-1024 characters")]
    InvalidDescription,
    #[error("skill compatibility must contain 1-500 characters when present")]
    InvalidCompatibility,
    #[error("learned-js must contain at most 32 unique full lowercase SHA-256 identities")]
    InvalidLearnedJs,
}

/// Unknown frontmatter keys are ignored (Claude Code compatibility); only the
/// keys below are interpreted, and none of them grants a capability.
#[derive(Debug, Deserialize)]
struct RawManifest {
    name: Option<String>,
    description: Option<String>,
    license: Option<String>,
    compatibility: Option<String>,
    #[serde(default, deserialize_with = "lenient_metadata")]
    metadata: BTreeMap<String, String>,
    #[serde(default, rename = "allowed-tools", deserialize_with = "string_or_list")]
    allowed_tools: Option<String>,
    #[serde(default, rename = "learned-js")]
    learned_js: Vec<String>,
    #[serde(
        default,
        rename = "disable-model-invocation",
        deserialize_with = "lenient_bool"
    )]
    disable_model_invocation: Option<bool>,
    #[serde(default, rename = "user-invocable", deserialize_with = "lenient_bool")]
    user_invocable: Option<bool>,
}

/// Claude Code flags accept YAML booleans or the strings `true`/`false`; any
/// other value is ignored (the default applies) rather than rejecting the package.
fn lenient_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_yaml_ng::Value;
    let raw: Option<Value> = Option::deserialize(deserializer)?;
    Ok(match raw {
        Some(Value::Bool(flag)) => Some(flag),
        Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        },
        Some(Value::Number(number)) => match number.as_i64() {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        _ => None,
    })
}

fn scalar_to_string(value: &serde_yaml_ng::Value) -> Option<String> {
    use serde_yaml_ng::Value;
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// `metadata` accepts arbitrary scalar values (rendered as text); null and
/// non-scalar values are dropped instead of rejecting the package.
fn lenient_metadata<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<BTreeMap<String, serde_yaml_ng::Value>> = Option::deserialize(deserializer)?;
    Ok(raw
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(key, value)| scalar_to_string(&value).map(|text| (key, text)))
        .collect())
}

/// `allowed-tools` accepts a space-separated string or a YAML list of scalars
/// (joined with single spaces). It stays descriptive and never grants anything.
fn string_or_list<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_yaml_ng::Value;
    let raw: Option<Value> = Option::deserialize(deserializer)?;
    Ok(match raw {
        None | Some(Value::Null) => None,
        Some(Value::Sequence(items)) => {
            let parts: Vec<String> = items.iter().filter_map(scalar_to_string).collect();
            (!parts.is_empty()).then(|| parts.join(" "))
        }
        Some(other) => scalar_to_string(&other),
    })
}

pub(super) fn parse_skill_markdown(bytes: &[u8]) -> Result<AgentSkillManifest, ManifestError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ManifestError::NonUtf8)?;
    let yaml = frontmatter(text)?;
    let raw: RawManifest = serde_yaml_ng::from_str(yaml)?;
    let name = raw.name.clone().ok_or(ManifestError::InvalidName)?;
    let description = raw
        .description
        .clone()
        .ok_or(ManifestError::InvalidDescription)?;
    validate_name(&name)?;
    if !(1..=1024).contains(&description.chars().count()) {
        return Err(ManifestError::InvalidDescription);
    }
    finish_manifest(raw, name, description)
}

/// Longest description kept by the lenient parser; longer text is truncated.
const LENIENT_DESCRIPTION_CHARS: usize = 1024;

/// Claude Code compatible parse for a package whose directory name is known:
/// no frontmatter field is required. `name` defaults to the directory name,
/// `description` to the first non-empty body line (cut at 1024 characters), a
/// `SKILL.md` without frontmatter is plain instruction content, malformed YAML
/// falls back to the simple `key: value` lines, and an over-long description or
/// compatibility note is truncated or dropped instead of rejecting the package.
/// A name that is present must still be a valid Agent Skills name. Every
/// manifest the strict parser accepts parses identically here.
pub(super) fn parse_skill_markdown_lenient(
    bytes: &[u8],
    directory: &str,
) -> Result<AgentSkillManifest, ManifestError> {
    let text = String::from_utf8_lossy(bytes);
    let (yaml, body) = match split_frontmatter(&text) {
        Some((yaml, body)) => (yaml, body),
        None => ("", &*text),
    };
    let mut raw: RawManifest = if yaml.trim().is_empty() {
        empty_raw()
    } else {
        serde_yaml_ng::from_str(yaml).unwrap_or_else(|_| scan_simple_keys(yaml))
    };
    if raw
        .compatibility
        .as_ref()
        .is_some_and(|value| !(1..=500).contains(&value.chars().count()))
    {
        raw.compatibility = None;
    }
    let name = raw
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(directory)
        .to_owned();
    validate_name(&name)?;
    let description = raw
        .description
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .or_else(|| first_body_line(body))
        .unwrap_or_else(|| name.clone());
    let description: String = description
        .chars()
        .take(LENIENT_DESCRIPTION_CHARS)
        .collect();
    finish_manifest(raw, name, description)
}

fn empty_raw() -> RawManifest {
    RawManifest {
        name: None,
        description: None,
        license: None,
        compatibility: None,
        metadata: BTreeMap::new(),
        allowed_tools: None,
        learned_js: Vec::new(),
        disable_model_invocation: None,
        user_invocable: None,
    }
}

/// Recovers the scalar keys the loader uses from frontmatter that is not valid
/// YAML (an unquoted colon, a stray tab): top-level `key: value` lines only.
fn scan_simple_keys(yaml: &str) -> RawManifest {
    let mut raw = empty_raw();
    for line in yaml.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        let flag = || {
            lenient_bool(serde_yaml_ng::Value::String(value.to_owned()))
                .ok()
                .flatten()
        };
        match key.trim() {
            "name" => raw.name = Some(value.to_owned()),
            "description" => raw.description = Some(value.to_owned()),
            "license" => raw.license = Some(value.to_owned()),
            "allowed-tools" if !value.is_empty() => raw.allowed_tools = Some(value.to_owned()),
            "disable-model-invocation" => raw.disable_model_invocation = flag(),
            "user-invocable" => raw.user_invocable = flag(),
            _ => {}
        }
    }
    raw
}

fn first_body_line(body: &str) -> Option<String> {
    body.lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

fn finish_manifest(
    raw: RawManifest,
    name: String,
    description: String,
) -> Result<AgentSkillManifest, ManifestError> {
    if raw
        .compatibility
        .as_ref()
        .is_some_and(|value| !(1..=500).contains(&value.chars().count()))
    {
        return Err(ManifestError::InvalidCompatibility);
    }
    if raw.learned_js.len() > 32
        || raw.learned_js.iter().any(|id| !is_full_sha256(id))
        || raw.learned_js.windows(2).any(|ids| ids[0] == ids[1])
        || {
            let mut sorted = raw.learned_js.clone();
            sorted.sort();
            sorted.dedup();
            sorted.len() != raw.learned_js.len()
        }
    {
        return Err(ManifestError::InvalidLearnedJs);
    }
    Ok(AgentSkillManifest {
        name,
        description,
        license: raw.license,
        compatibility: raw.compatibility,
        metadata: raw.metadata,
        allowed_tools: raw.allowed_tools,
        learned_js: raw.learned_js,
        disable_model_invocation: raw.disable_model_invocation.unwrap_or(false),
        user_invocable: raw.user_invocable.unwrap_or(true),
    })
}

fn is_full_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The YAML between the leading `---` lines and the text after the closing one.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let mut lines = text.split_inclusive('\n');
    let first = lines.next()?;
    if trim_line_ending(first) != "---" {
        return None;
    }
    let yaml_start = first.len();
    let mut offset = yaml_start;
    for line in lines {
        if trim_line_ending(line) == "---" {
            return Some((&text[yaml_start..offset], &text[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

fn frontmatter(text: &str) -> Result<&str, ManifestError> {
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().ok_or(ManifestError::MissingFrontmatter)?;
    if trim_line_ending(first) != "---" {
        return Err(ManifestError::MissingFrontmatter);
    }

    let yaml_start = first.len();
    let mut offset = yaml_start;
    for line in lines {
        if trim_line_ending(line) == "---" {
            return Ok(&text[yaml_start..offset]);
        }
        offset += line.len();
    }
    Err(ManifestError::MissingFrontmatter)
}

fn trim_line_ending(line: &str) -> &str {
    let without_lf = line.strip_suffix('\n').unwrap_or(line);
    without_lf.strip_suffix('\r').unwrap_or(without_lf)
}

fn validate_name(name: &str) -> Result<(), ManifestError> {
    if !(1..=64).contains(&name.chars().count())
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(ManifestError::InvalidName);
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return Err(ManifestError::InvalidNameHyphens);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenient_manifest_defaults_name_and_description_and_accepts_yes_no_flags() {
        let bare = parse_skill_markdown_lenient(b"\n## Title line\nmore\n", "my-skill").unwrap();
        assert_eq!(
            (bare.name.as_str(), bare.description.as_str()),
            ("my-skill", "Title line")
        );
        let flags = parse_skill_markdown_lenient(
            b"---\ndisable-model-invocation: on\nuser-invocable: 0\n---\nbody\n",
            "flags",
        )
        .unwrap();
        assert!(flags.disable_model_invocation && !flags.user_invocable);
        // Malformed YAML falls back to the simple key lines.
        let broken = parse_skill_markdown_lenient(
            b"---\nname: other\ndescription: a: b: [\n\t- x\n---\nbody\n",
            "other",
        )
        .unwrap();
        assert_eq!(broken.description, "a: b: [");
        // A present name must still be valid; strict parsing still rejects a bare file.
        assert!(parse_skill_markdown_lenient(b"---\nname: Bad Name\n---\n", "x").is_err());
        assert!(parse_skill_markdown(b"just text\n").is_err());
        assert!(parse_skill_markdown(b"---\nname: only-name\n---\n").is_err());
        let empty = parse_skill_markdown_lenient(b"", "empty-one").unwrap();
        assert_eq!(empty.description, "empty-one");
    }

    #[test]
    fn agent_skill_manifest_preserves_allowed_tools_without_interpreting_them() {
        let manifest = parse_skill_markdown(
            b"---\nname: review-code\ndescription: Reviews code when asked.\nallowed-tools: Bash(git:*) Read\nmetadata:\n  owner: team\n---\nInstructions\n",
        )
        .unwrap();
        assert_eq!(manifest.name, "review-code");
        assert_eq!(manifest.allowed_tools.as_deref(), Some("Bash(git:*) Read"));
        assert_eq!(
            manifest.metadata.get("owner").map(String::as_str),
            Some("team")
        );
    }

    #[test]
    fn agent_skill_manifest_rejects_invalid_names_but_ignores_unknown_fields() {
        let bad_name = b"---\nname: Bad--Name\ndescription: Invalid name.\n---\nInstructions\n";
        assert!(parse_skill_markdown(bad_name).is_err());

        let unknown =
            b"---\nname: valid\ndescription: Valid description.\ncapabilities: all\n---\n";
        let manifest = parse_skill_markdown(unknown).unwrap();
        assert_eq!(manifest.name, "valid");
        // An ignored key never grants anything.
        assert!(manifest.allowed_tools.is_none());
    }

    #[test]
    fn agent_skill_manifest_loads_claude_code_frontmatter() {
        let markdown = b"---\nname: fix-issue\ndescription: Fix an issue.\ndisable-model-invocation: true\nuser-invocable: false\nargument-hint: \"[issue]\"\nmodel: opus\neffort: high\ncontext: fork\nagent: Explore\npaths:\n  - \"src/**\"\nhooks:\n  PreToolUse:\n    - matcher: Bash\nallowed-tools:\n  - Read\n  - Bash(git:*)\nmetadata:\n  version: 2\n  stable: true\n  nested:\n    a: b\n  empty:\n---\nBody\n";
        let manifest = parse_skill_markdown(markdown).unwrap();
        assert_eq!(manifest.name, "fix-issue");
        assert_eq!(manifest.allowed_tools.as_deref(), Some("Read Bash(git:*)"));
        assert_eq!(
            manifest.metadata.get("version").map(String::as_str),
            Some("2")
        );
        assert_eq!(
            manifest.metadata.get("stable").map(String::as_str),
            Some("true")
        );
        assert!(!manifest.metadata.contains_key("nested"));
        assert!(!manifest.metadata.contains_key("empty"));
        assert!(manifest.disable_model_invocation);
        assert!(!manifest.user_invocable);
        let plain = parse_skill_markdown(b"---\nname: p\ndescription: d\n---\n").unwrap();
        assert!(!plain.disable_model_invocation && plain.user_invocable);
        let stringy = parse_skill_markdown(
            b"---\nname: p\ndescription: d\ndisable-model-invocation: \"True\"\nuser-invocable: maybe\n---\n",
        )
        .unwrap();
        assert!(stringy.disable_model_invocation && stringy.user_invocable);
    }

    #[test]
    fn agent_skill_manifest_still_validates_known_fields_with_unknown_keys_present() {
        let long = "d".repeat(1025);
        let markdown = format!("---\nname: ok\ndescription: {long}\nmodel: x\n---\n");
        assert!(matches!(
            parse_skill_markdown(markdown.as_bytes()),
            Err(ManifestError::InvalidDescription)
        ));
    }

    #[test]
    fn agent_skill_manifest_accepts_crlf_frontmatter() {
        let manifest = parse_skill_markdown(
            b"---\r\nname: portable\r\ndescription: Works with CRLF files.\r\n---\r\nBody\r\n",
        )
        .unwrap();
        assert_eq!(manifest.name, "portable");
    }

    #[test]
    fn agent_skill_manifest_accepts_bounded_exact_learned_js_identities() {
        let first = "a".repeat(64);
        let second = "0123456789abcdef".repeat(4);
        let markdown = format!(
            "---\nname: portable\ndescription: Associates verified capabilities.\nlearned-js:\n  - {first}\n  - {second}\n---\nBody\n"
        );
        let manifest = parse_skill_markdown(markdown.as_bytes()).unwrap();
        assert_eq!(manifest.learned_js, vec![first, second]);
    }

    #[test]
    fn agent_skill_manifest_rejects_invalid_or_duplicate_learned_js_identities() {
        for learned_js in [
            "learned-js: [short]".to_string(),
            format!("learned-js: [{}]", "A".repeat(64)),
            format!("learned-js: [{0}, {0}]", "a".repeat(64)),
        ] {
            let markdown = format!(
                "---\nname: portable\ndescription: Associates verified capabilities.\n{learned_js}\n---\nBody\n"
            );
            assert!(matches!(
                parse_skill_markdown(markdown.as_bytes()),
                Err(ManifestError::InvalidLearnedJs)
            ));
        }
    }
}
