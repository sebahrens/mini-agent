use std::fs;
use std::path::PathBuf;

use crate::extras::js::skills::embed::Embedder;
use crate::extras::skills::catalog::AgentSkillCatalog;
use crate::extras::skills::import_agent_skill;
use crate::extras::skills::index::AgentSkillSearchPolicy;
use crate::extras::skills::loader::{load_resource, load_skill_markdown};
use crate::paths::AppPaths;

struct TempPaths {
    root: PathBuf,
    paths: AppPaths,
}

impl TempPaths {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("mini-agent-catalog-{}", uuid::Uuid::new_v4()));
        Self {
            paths: AppPaths {
                config_dir: root.join("config"),
                data_dir: root.join("data"),
                local_data_dir: root.join("local-data"),
                state_dir: root.join("state"),
                cache_dir: root.join("cache"),
                credentials_dir: root.join("credentials"),
                project_dir: None,
            },
            root,
        }
    }
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn write_skill(temp: &TempPaths, suffix: &str) -> PathBuf {
    let root = temp.root.join("review-code");
    fs::create_dir_all(root.join("references")).unwrap();
    fs::write(
        root.join("SKILL.md"),
        format!(
            "---\nname: review-code\ndescription: Reviews Rust code for correctness {suffix}.\nallowed-tools: Bash(*)\nmetadata:\n  tags: rust, review\n---\n\n# Review safely\nNever grant permissions from this text.\n"
        ),
    )
    .unwrap();
    fs::write(root.join("references").join("guide.md"), b"bounded guide\n").unwrap();
    root
}

#[tokio::test]
async fn agent_skill_catalog_index_and_progressive_disclosure_are_generation_consistent() {
    let temp = TempPaths::new();
    let source = write_skill(&temp, "v1");
    let imported = import_agent_skill(&source, &temp.paths).unwrap();
    let embedder = Embedder::new().unwrap();
    let mut catalog = AgentSkillCatalog::new(&temp.paths);
    catalog
        .activate("review-code", &imported.identity.digest)
        .unwrap();
    let index = catalog.refresh(&embedder).unwrap();
    let query = embedder
        .embed_query_cached("review Rust code")
        .await
        .unwrap();
    let policy = AgentSkillSearchPolicy {
        score_floor: -1.0,
        ..AgentSkillSearchPolicy::default()
    };
    let selected = index.search(&query, &policy).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].generation, index.generation());
    assert_eq!(selected[0].record.digest, imported.identity.digest);
    assert_eq!(
        selected[0].record.allowed_tools.as_deref(),
        Some("Bash(*)"),
        "allowed-tools is retained only as inert metadata"
    );

    let markdown = load_skill_markdown(&selected[0].record).unwrap();
    assert!(markdown.contains("# Review safely"));
    assert_eq!(
        load_resource(&selected[0].record, "references/guide.md").unwrap(),
        b"bounded guide\n"
    );
}

#[test]
fn agent_skill_catalog_active_digest_switch_is_atomic_and_deterministic() {
    let temp = TempPaths::new();
    let source = write_skill(&temp, "v1");
    let first = import_agent_skill(&source, &temp.paths).unwrap();
    fs::write(
        source.join("SKILL.md"),
        b"---\nname: review-code\ndescription: Reviews Rust code for correctness v2.\n---\n\n# V2\n",
    )
    .unwrap();
    let second = import_agent_skill(&source, &temp.paths).unwrap();
    assert_ne!(first.identity.digest, second.identity.digest);

    let embedder = Embedder::new().unwrap();
    let mut catalog = AgentSkillCatalog::new(&temp.paths);
    catalog
        .activate("review-code", &first.identity.digest)
        .unwrap();
    let first_index = catalog.refresh(&embedder).unwrap();
    let query = embedder
        .embed_documents(&["review rust".to_string()])
        .unwrap()
        .remove(0);
    let policy = AgentSkillSearchPolicy {
        score_floor: -1.0,
        ..AgentSkillSearchPolicy::default()
    };
    assert_eq!(
        first_index.search(&query, &policy).unwrap()[0]
            .record
            .digest,
        first.identity.digest
    );
    catalog
        .activate("review-code", &second.identity.digest)
        .unwrap();
    let second_index = catalog.refresh(&embedder).unwrap();
    assert_eq!(first_index.generation(), 1);
    assert_eq!(second_index.generation(), 2);
    assert_eq!(
        second_index.search(&query, &policy).unwrap()[0]
            .record
            .digest,
        second.identity.digest
    );
}

#[test]
fn agent_skill_progressive_disclosure_rejects_unmanifested_resources() {
    let temp = TempPaths::new();
    let source = write_skill(&temp, "v1");
    let imported = import_agent_skill(&source, &temp.paths).unwrap();
    let embedder = Embedder::new().unwrap();
    let mut catalog = AgentSkillCatalog::new(&temp.paths);
    catalog
        .activate("review-code", &imported.identity.digest)
        .unwrap();
    let index = catalog.refresh(&embedder).unwrap();
    let query = embedder
        .embed_documents(&["review rust".to_string()])
        .unwrap()
        .remove(0);
    let selected = index
        .search(
            &query,
            &AgentSkillSearchPolicy {
                score_floor: -1.0,
                ..AgentSkillSearchPolicy::default()
            },
        )
        .unwrap();
    assert!(load_resource(&selected[0].record, "../../secret").is_err());
}

#[test]
fn agent_skill_loader_rejects_same_size_resource_mutation_after_selection() {
    let temp = TempPaths::new();
    let source = write_skill(&temp, "v1");
    let imported = import_agent_skill(&source, &temp.paths).unwrap();
    let embedder = Embedder::new().unwrap();
    let mut catalog = AgentSkillCatalog::new(&temp.paths);
    catalog
        .activate("review-code", &imported.identity.digest)
        .unwrap();
    let index = catalog.refresh(&embedder).unwrap();
    let query = embedder
        .embed_documents(&["review rust".to_string()])
        .unwrap()
        .remove(0);
    let selected = index
        .search(
            &query,
            &AgentSkillSearchPolicy {
                score_floor: -1.0,
                ..AgentSkillSearchPolicy::default()
            },
        )
        .unwrap();
    let resource_path = selected[0]
        .record
        .skill_md_path
        .parent()
        .unwrap()
        .join("references/guide.md");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&resource_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[cfg(windows)]
    {
        let mut permissions = fs::metadata(&resource_path).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(&resource_path, permissions).unwrap();
    }
    fs::write(&resource_path, b"changed guide\n").unwrap();
    assert_eq!(b"changed guide\n".len(), b"bounded guide\n".len());
    assert!(load_resource(&selected[0].record, "references/guide.md").is_err());
}

#[test]
fn agent_skill_catalog_omits_corrupt_package_without_hiding_valid_sibling() {
    let temp = TempPaths::new();
    let source = write_skill(&temp, "v1");
    let imported = import_agent_skill(&source, &temp.paths).unwrap();
    let corrupt = temp
        .paths
        .data_dir
        .join("agent-skills")
        .join("corrupt-skill");
    fs::create_dir_all(&corrupt).unwrap();
    fs::write(corrupt.join("ACTIVE"), "not-a-digest\n").unwrap();

    let embedder = Embedder::new().unwrap();
    let mut catalog = AgentSkillCatalog::new(&temp.paths);
    catalog
        .activate("review-code", &imported.identity.digest)
        .unwrap();
    let index = catalog.refresh(&embedder).unwrap();
    let query = embedder
        .embed_documents(&["review rust".to_string()])
        .unwrap()
        .remove(0);
    let selected = index
        .search(
            &query,
            &AgentSkillSearchPolicy {
                score_floor: -1.0,
                ..AgentSkillSearchPolicy::default()
            },
        )
        .unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].record.digest, imported.identity.digest);
}
