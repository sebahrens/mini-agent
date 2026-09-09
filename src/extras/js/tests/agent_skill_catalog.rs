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
    for damage in [
        "invalid",
        #[cfg(unix)]
        "unreadable",
        "directory",
        "oversized",
        #[cfg(unix)]
        "symlink",
        #[cfg(unix)]
        "dangling-symlink",
    ] {
        let temp = TempPaths::new();
        let source = write_skill(&temp, "v1");
        import_agent_skill(&source, &temp.paths).unwrap();
        let other_source = temp.root.join("corrupt-skill");
        fs::create_dir_all(&other_source).unwrap();
        fs::write(
            other_source.join("SKILL.md"),
            "---\nname: corrupt-skill\ndescription: review code\n---\n# Other skill\n",
        )
        .unwrap();
        let other = import_agent_skill(&other_source, &temp.paths).unwrap();
        let pointer = other.install_path.parent().unwrap().join("ACTIVE");
        let embedder = Embedder::new().unwrap();
        let mut catalog = AgentSkillCatalog::new(&temp.paths);
        let names = |index: &crate::extras::skills::index::AgentSkillIndex| {
            let mut names = index
                .search_lexical("review", &AgentSkillSearchPolicy::default())
                .unwrap()
                .into_iter()
                .map(|skill| skill.record.name.clone())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        assert_eq!(
            names(&catalog.refresh(&embedder).unwrap()),
            ["corrupt-skill", "review-code"]
        );
        fs::remove_file(&pointer).unwrap();
        match damage {
            "invalid" => fs::write(&pointer, "not-a-digest\n").unwrap(),
            "directory" => fs::create_dir(&pointer).unwrap(),
            "oversized" => fs::write(
                &pointer,
                format!("{}{}", other.identity.digest, " ".repeat(1024)),
            )
            .unwrap(),
            #[cfg(unix)]
            "unreadable" => {
                use std::os::unix::fs::PermissionsExt;
                fs::write(&pointer, format!("{}\n", other.identity.digest)).unwrap();
                fs::set_permissions(&pointer, fs::Permissions::from_mode(0o000)).unwrap();
                if fs::read(&pointer).is_ok() {
                    eprintln!(
                        "skipping unreadable-pointer case: this account bypasses file permissions"
                    );
                    continue;
                }
            }
            #[cfg(unix)]
            "symlink" | "dangling-symlink" => {
                let target = temp.root.join("external-active");
                if damage == "symlink" {
                    fs::write(&target, format!("{}\n", other.identity.digest)).unwrap();
                }
                std::os::unix::fs::symlink(&target, &pointer).unwrap();
            }
            _ => unreachable!(),
        }
        let changed = catalog
            .refresh_if_changed(&embedder)
            .unwrap_or_else(|error| panic!("{damage}: {error}"))
            .expect("pointer damage must refresh the catalog");
        assert_eq!(names(&changed), ["review-code"], "{damage}");
        assert!(
            catalog.refresh_if_changed(&embedder).unwrap().is_none(),
            "{damage}"
        );

        // A fresh catalog must isolate the same failure, and a repaired package must reappear.
        let mut fresh_catalog = AgentSkillCatalog::new(&temp.paths);
        assert_eq!(
            names(&fresh_catalog.refresh(&embedder).unwrap()),
            ["review-code"],
            "{damage}"
        );
        if damage == "directory" {
            fs::remove_dir(&pointer).unwrap();
        } else {
            fs::remove_file(&pointer).unwrap();
        }
        catalog
            .activate("corrupt-skill", &other.identity.digest)
            .unwrap();
        let repaired = catalog
            .refresh_if_changed(&embedder)
            .unwrap()
            .expect("repair must refresh");
        assert_eq!(
            names(&repaired),
            ["corrupt-skill", "review-code"],
            "{damage}"
        );
        assert!(catalog.refresh_if_changed(&embedder).unwrap().is_none());
    }
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
    let name_root = temp.paths.data_dir.join("agent-skills").join("review-code");
    fs::remove_file(name_root.join("ACTIVE")).unwrap();
    let embedder = Embedder::new().unwrap();
    let mut catalog = AgentSkillCatalog::new(&temp.paths);
    let legacy = catalog
        .refresh(&embedder)
        .unwrap()
        .search_lexical("review", &AgentSkillSearchPolicy::default())
        .unwrap();
    assert_eq!(
        legacy.len(),
        1,
        "one digest without a pointer remains loadable"
    );
    assert_eq!(legacy[0].record.digest, first.identity.digest);
    fs::write(
        source.join("SKILL.md"),
        b"---\nname: review-code\ndescription: Reviews Rust code for correctness v2.\n---\n\n# V2\n",
    )
    .unwrap();
    let second = import_agent_skill(&source, &temp.paths).unwrap();
    assert_ne!(first.identity.digest, second.identity.digest);

    fs::remove_file(name_root.join("ACTIVE")).unwrap();
    assert!(name_root.join(&first.identity.digest).is_dir());
    assert!(name_root.join(&second.identity.digest).is_dir());

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

/// Catalog limits must apply before ranking, while retaining each exact-limit tree.
#[test]
fn agent_skill_catalog_enforces_instruction_and_resource_read_limits() {
    for (markdown, limit) in [
        (true, crate::extras::skills::MAX_SKILL_INSTRUCTION_BYTES),
        (false, 16 * 1024 * 1024),
    ] {
        let temp = TempPaths::new();
        let source = write_skill(&temp, "v1");
        let small = import_agent_skill(&source, &temp.paths).unwrap();
        let digest = "f".repeat(64);
        let root = temp
            .paths
            .data_dir
            .join("agent-skills")
            .join("verbose-code")
            .join(&digest);
        fs::create_dir_all(&root).unwrap();
        let header = "---\nname: verbose-code\ndescription: Reviews Rust code at length.\n---\n\n";
        fs::write(root.join("SKILL.md"), header).unwrap();
        let path = if markdown {
            root.join("SKILL.md")
        } else {
            root.join("asset.bin")
        };
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        file.set_len(limit).unwrap();
        fs::write(root.parent().unwrap().join("ACTIVE"), format!("{digest}\n")).unwrap();
        let embedder = Embedder::new().unwrap();
        let mut catalog = AgentSkillCatalog::new(&temp.paths);
        let query = embedder
            .embed_documents(&["review rust".to_string()])
            .unwrap()
            .remove(0);
        // This generous selection budget cannot itself hide an over-limit instruction tree.
        let policy = AgentSkillSearchPolicy {
            score_floor: -1.0,
            instruction_byte_budget: 8 * 1024 * 1024,
            ..AgentSkillSearchPolicy::default()
        };
        let at_limit = catalog
            .refresh(&embedder)
            .unwrap()
            .search(&query, &policy)
            .unwrap();
        assert_eq!(
            at_limit.len(),
            2,
            "markdown={markdown}: exact limit must be accepted"
        );
        if !markdown {
            let record = &at_limit
                .iter()
                .find(|skill| skill.record.digest == digest)
                .unwrap()
                .record;
            assert!(
                matches!(
                    load_resource(record, "asset.bin"),
                    Err(crate::extras::skills::loader::LoadError::TooLarge)
                ),
                "catalog import limit must not enlarge progressive loading's 1 MiB budget"
            );
        }
        file.set_len(limit + 1).unwrap();
        let selected = catalog
            .refresh(&embedder)
            .unwrap()
            .search(&query, &policy)
            .unwrap();
        assert_eq!(
            selected.len(),
            1,
            "markdown={markdown}: oversized tree must be omitted"
        );
        assert_eq!(selected[0].record.digest, small.identity.digest);
    }
}

#[test]
fn agent_skill_catalog_scan_limits_include_empty_directories_and_instruction_bytes() {
    for limit in ["depth", "entries", "bytes"] {
        let temp = TempPaths::new();
        let source = write_skill(&temp, "v1");
        let small = import_agent_skill(&source, &temp.paths).unwrap();
        let digest = "e".repeat(64);
        let root = temp
            .paths
            .data_dir
            .join("agent-skills")
            .join("bounded-code")
            .join(&digest);
        fs::create_dir_all(&root).unwrap();
        let markdown = "---\nname: bounded-code\ndescription: review code\n---\n# Bounded\n";
        fs::write(root.join("SKILL.md"), markdown).unwrap();
        fs::write(root.parent().unwrap().join("ACTIVE"), format!("{digest}\n")).unwrap();
        let overflow_path = match limit {
            "depth" => {
                let mut deepest = root.clone();
                for _ in 0..16 {
                    deepest.push("d");
                    fs::create_dir(&deepest).unwrap();
                }
                deepest.join("one-too-deep")
            }
            "entries" => {
                // SKILL.md is also an entry: 4095 empty directories reach the limit.
                for entry in 0..4095 {
                    fs::create_dir(root.join(format!("d-{entry}"))).unwrap();
                }
                root.join("one-too-many")
            }
            "bytes" => {
                let chunk = 16 * 1024 * 1024;
                for entry in 0..8 {
                    let bytes = if entry == 7 {
                        chunk - markdown.len() as u64
                    } else {
                        chunk
                    };
                    fs::File::create(root.join(format!("asset-{entry}")))
                        .unwrap()
                        .set_len(bytes)
                        .unwrap();
                }
                root.join("asset-7")
            }
            _ => unreachable!(),
        };
        let embedder = Embedder::new().unwrap();
        let mut catalog = AgentSkillCatalog::new(&temp.paths);
        let policy = AgentSkillSearchPolicy::default();
        let at_limit = catalog
            .refresh(&embedder)
            .unwrap()
            .search_lexical("review", &policy)
            .unwrap();
        assert_eq!(
            at_limit.len(),
            2,
            "{limit}: exact boundary must be accepted"
        );
        if limit == "bytes" {
            let file = fs::OpenOptions::new()
                .write(true)
                .open(&overflow_path)
                .unwrap();
            file.set_len(file.metadata().unwrap().len() + 1).unwrap();
        } else {
            fs::create_dir(&overflow_path).unwrap();
        }
        let over_limit = catalog
            .refresh(&embedder)
            .unwrap()
            .search_lexical("review", &policy)
            .unwrap();
        assert_eq!(
            over_limit.len(),
            1,
            "{limit}: over-limit package must be omitted"
        );
        assert_eq!(over_limit[0].record.digest, small.identity.digest);
    }
}
