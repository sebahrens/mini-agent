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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: String,
    description: String,
    license: Option<String>,
    compatibility: Option<String>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(rename = "allowed-tools")]
    allowed_tools: Option<String>,
    #[serde(default, rename = "learned-js")]
    learned_js: Vec<String>,
}

pub(super) fn parse_skill_markdown(bytes: &[u8]) -> Result<AgentSkillManifest, ManifestError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ManifestError::NonUtf8)?;
    let yaml = frontmatter(text)?;
    let raw: RawManifest = serde_yaml_ng::from_str(yaml)?;
    validate_name(&raw.name)?;
    if !(1..=1024).contains(&raw.description.chars().count()) {
        return Err(ManifestError::InvalidDescription);
    }
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
        name: raw.name,
        description: raw.description,
        license: raw.license,
        compatibility: raw.compatibility,
        metadata: raw.metadata,
        allowed_tools: raw.allowed_tools,
        learned_js: raw.learned_js,
    })
}

fn is_full_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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
    fn agent_skill_manifest_rejects_invalid_names_and_unknown_fields() {
        let bad_name = b"---\nname: Bad--Name\ndescription: Invalid name.\n---\nInstructions\n";
        assert!(parse_skill_markdown(bad_name).is_err());

        let unknown =
            b"---\nname: valid\ndescription: Valid description.\ncapabilities: all\n---\n";
        assert!(parse_skill_markdown(unknown).is_err());
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
