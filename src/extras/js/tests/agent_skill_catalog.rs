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
async fn agent_skill_catalog_preserves_imported_metadata_and_progressive_disclosure() {
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
    let retained = first_index.search(&query, &policy).unwrap().remove(0);
    assert_eq!(retained.record.digest, first.identity.digest);
    let first_markdown = load_skill_markdown(&retained.record).unwrap();
    assert!(first_markdown.contains("# Review safely"));
    catalog
        .activate("review-code", &second.identity.digest)
        .unwrap();
    let second_index = catalog.refresh(&embedder).unwrap();
    assert_eq!(first_index.generation(), 1);
    assert_eq!(second_index.generation(), 2);
    let current = second_index.search(&query, &policy).unwrap().remove(0);
    assert_eq!(current.record.digest, second.identity.digest);
    assert!(
        load_skill_markdown(&current.record)
            .unwrap()
            .contains("# V2")
    );
    assert_eq!(
        load_skill_markdown(&retained.record).unwrap(),
        first_markdown
    );
    assert_eq!(
        first_index.search(&query, &policy).unwrap()[0]
            .record
            .digest,
        first.identity.digest,
        "publishing a new catalog must not change a retained index"
    );
}

#[test]
fn catalog_refresh_if_changed_skips_stable_trees_and_detects_imports() {
    let temp = TempPaths::new();
    let source = write_skill(&temp, "v1");
    let first = import_agent_skill(&source, &temp.paths).unwrap();
    let embedder = Embedder::new().unwrap();
    let mut catalog = AgentSkillCatalog::new(&temp.paths);
    let initial = catalog.refresh(&embedder).unwrap();
    assert_eq!(initial.generation(), 1);
    assert!(catalog.refresh_if_changed(&embedder).unwrap().is_none());

    fs::write(
        source.join("SKILL.md"),
        b"---\nname: review-code\ndescription: Reviews imported Rust changes v2.\n---\n\n# V2\n",
    )
    .unwrap();
    let second = import_agent_skill(&source, &temp.paths).unwrap();
    assert_ne!(first.identity.digest, second.identity.digest);
    let refreshed = catalog
        .refresh_if_changed(&embedder)
        .unwrap()
        .expect("new import must refresh the catalog");
    assert_eq!(refreshed.generation(), 2);
    assert!(catalog.refresh_if_changed(&embedder).unwrap().is_none());
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

/// The version an import installs is the version the catalog serves.
///
/// The catalog used to sort the installed digest directories and take the
/// last, so the winner was an artefact of SHA-256 ordering: this test keeps
/// importing new versions until one of the older digests sorts after the
/// newest, which is exactly the case the old fallback got wrong.
#[test]
fn reimporting_an_updated_agent_skill_activates_the_version_just_installed() {
    let temp = TempPaths::new();
    let source = write_skill(&temp, "v1");
    let mut latest = import_agent_skill(&source, &temp.paths).unwrap();
    let mut digests = vec![latest.identity.digest.clone()];
    for version in 2..12 {
        fs::write(
            source.join("SKILL.md"),
            format!(
                "---\nname: review-code\ndescription: Reviews Rust code for correctness v{version}.\n---\n\n# V{version}\n"
            ),
        )
        .unwrap();
        latest = import_agent_skill(&source, &temp.paths).unwrap();
        digests.push(latest.identity.digest.clone());
        if digests
            .iter()
            .any(|digest| digest.as_str() > latest.identity.digest.as_str())
        {
            break;
        }
    }
    assert!(
        digests
            .iter()
            .any(|digest| digest.as_str() > latest.identity.digest.as_str()),
        "no installed digest sorted after the newest one, so this test proves nothing"
    );

    let embedder = Embedder::new().unwrap();
    let mut catalog = AgentSkillCatalog::new(&temp.paths);
    let index = catalog.refresh(&embedder).unwrap();
    let query = embedder
        .embed_documents(&["review rust".to_string()])
        .unwrap()
        .remove(0);
    let policy = AgentSkillSearchPolicy {
        score_floor: -1.0,
        ..AgentSkillSearchPolicy::default()
    };
    let selected = index.search(&query, &policy).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0].record.digest, latest.identity.digest,
        "the newest import must be active, not the digest that sorts last"
    );
}

#[test]
fn agent_skill_catalog_refuses_to_guess_between_digests_without_a_pointer() {
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

    let name_root = temp.paths.data_dir.join("agent-skills").join("review-code");
    fs::remove_file(name_root.join("ACTIVE")).unwrap();
    assert!(name_root.join(&first.identity.digest).is_dir());
    assert!(name_root.join(&second.identity.digest).is_dir());

    let embedder = Embedder::new().unwrap();
    let mut catalog = AgentSkillCatalog::new(&temp.paths);
    let query = embedder
        .embed_documents(&["review rust".to_string()])
        .unwrap()
        .remove(0);
    let policy = AgentSkillSearchPolicy {
        score_floor: -1.0,
        ..AgentSkillSearchPolicy::default()
    };
    assert!(
        catalog
            .refresh(&embedder)
            .unwrap()
            .search(&query, &policy)
            .unwrap()
            .is_empty(),
        "two installed digests and no pointer must be omitted, not guessed"
    );

    // Naming one restores it, and the pointer decides which.
    catalog
        .activate("review-code", &first.identity.digest)
        .unwrap();
    let selected = catalog
        .refresh(&embedder)
        .unwrap()
        .search(&query, &policy)
        .unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].record.digest, first.identity.digest);
}

/// A tree already installed above the turn instruction budget is omitted
/// rather than ranked first and then dropped on every turn.
#[test]
fn agent_skill_catalog_omits_a_tree_over_the_turn_instruction_budget() {
    let temp = TempPaths::new();
    let source = write_skill(&temp, "v1");
    let small = import_agent_skill(&source, &temp.paths).unwrap();

    let oversized_digest = "f".repeat(64);
    let oversized_root = temp
        .paths
        .data_dir
        .join("agent-skills")
        .join("verbose-code")
        .join(&oversized_digest);
    fs::create_dir_all(&oversized_root).unwrap();
    let header = "---\nname: verbose-code\ndescription: Reviews Rust code at length.\n---\n\n";
    let budget = usize::try_from(crate::extras::skills::MAX_SKILL_INSTRUCTION_BYTES).unwrap();
    fs::write(
        oversized_root.join("SKILL.md"),
        format!("{header}{}", "x".repeat(budget)),
    )
    .unwrap();
    fs::write(
        oversized_root.parent().unwrap().join("ACTIVE"),
        format!("{oversized_digest}\n"),
    )
    .unwrap();

    let embedder = Embedder::new().unwrap();
    let mut catalog = AgentSkillCatalog::new(&temp.paths);
    let index = catalog.refresh(&embedder).unwrap();
    let query = embedder
        .embed_documents(&["review rust".to_string()])
        .unwrap()
        .remove(0);
    // A budget wide enough to select the oversized tree if the catalog had
    // admitted it: the omission must come from the catalog, not from the
    // per-turn budget that silently dropped it afterwards.
    let selected = index
        .search(
            &query,
            &AgentSkillSearchPolicy {
                score_floor: -1.0,
                instruction_byte_budget: 8 * 1024 * 1024,
                ..AgentSkillSearchPolicy::default()
            },
        )
        .unwrap();
    assert_eq!(selected.len(), 1, "the oversized tree must not be indexed");
    assert_eq!(selected[0].record.digest, small.identity.digest);
}
