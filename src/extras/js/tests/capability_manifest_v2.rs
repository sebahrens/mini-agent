use crate::extras::js::skills::{
    CapabilityManifest, CapabilityScope, CapabilityTier, HttpMethod, IDENTITY_VERSION,
    SKILL_ABI_VERSION, SkillArtifact, SkillExport,
};

fn export() -> SkillExport {
    SkillExport {
        name: "run".to_string(),
        signature: "run(value: string): string".to_string(),
    }
}

fn artifact(manifest: CapabilityManifest) -> SkillArtifact {
    SkillArtifact::new(
        "function run(_cap, value) { return value; }".to_string(),
        "Return a value.".to_string(),
        vec!["identity".to_string()],
        vec![export()],
        vec!["run(null, 'ok') === 'ok'".to_string()],
        manifest,
    )
    .expect("valid identity-v2 artifact")
}

#[test]
fn capability_manifest_v2_binds_identity_and_abi_versions() {
    let original = artifact(CapabilityManifest::pure());
    assert_eq!(IDENTITY_VERSION, 2);
    assert_eq!(SKILL_ABI_VERSION, 2);
    assert_eq!(original.identity_version, IDENTITY_VERSION);
    assert_eq!(original.abi_version, SKILL_ABI_VERSION);

    let mut wrong_abi = original.clone();
    wrong_abi.abi_version += 1;
    wrong_abi.id = wrong_abi.compute_identity();
    assert!(wrong_abi.verify_identity().is_err());

    let scoped = artifact(
        CapabilityManifest::new(
            CapabilityTier::ReadOnly,
            vec![CapabilityScope::ReadFile {
                workspace_prefixes: vec!["src".to_string()],
            }],
        )
        .unwrap(),
    );
    assert_ne!(original.id, scoped.id);
}

#[test]
fn capability_manifest_v2_canonicalizes_order_origins_ports_and_unicode() {
    let decomposed = "fixtures/cafe\u{301}";
    let forward = CapabilityManifest::new(
        CapabilityTier::SideEffecting,
        vec![
            CapabilityScope::Spawn {
                programs: vec!["zeta".to_string(), "alpha".to_string()],
            },
            CapabilityScope::Fetch {
                origins: vec![
                    "https://EXAMPLE.com:443".to_string(),
                    "http://example.net:80".to_string(),
                ],
                methods: vec![HttpMethod::Post, HttpMethod::Get],
            },
            CapabilityScope::ReadFile {
                workspace_prefixes: vec![decomposed.to_string(), "src/lib".to_string()],
            },
        ],
    )
    .unwrap();
    let reverse = CapabilityManifest::new(
        CapabilityTier::SideEffecting,
        vec![
            CapabilityScope::ReadFile {
                workspace_prefixes: vec!["src/lib".to_string(), "fixtures/café".to_string()],
            },
            CapabilityScope::Fetch {
                origins: vec![
                    "http://example.net".to_string(),
                    "https://example.com".to_string(),
                ],
                methods: vec![HttpMethod::Get, HttpMethod::Post],
            },
            CapabilityScope::Spawn {
                programs: vec!["alpha".to_string(), "zeta".to_string()],
            },
        ],
    )
    .unwrap();

    assert_eq!(forward, reverse);
    assert_eq!(artifact(forward).id, artifact(reverse).id);
}

#[test]
fn capability_manifest_v2_rejects_non_portable_paths_and_programs() {
    for prefix in [
        "",
        ".",
        "..",
        "src/./lib",
        "src/../secret",
        "/absolute",
        "C:/absolute",
        "src\\windows",
        "src//double",
        "src/",
    ] {
        let result = CapabilityManifest::new(
            CapabilityTier::ReadOnly,
            vec![CapabilityScope::ReadFile {
                workspace_prefixes: vec![prefix.to_string()],
            }],
        );
        assert!(
            result.is_err(),
            "accepted invalid workspace prefix {prefix:?}"
        );
    }

    for program in ["", ".", "..", "bin/tool", "bin\\tool", "C:tool"] {
        let result = CapabilityManifest::new(
            CapabilityTier::SideEffecting,
            vec![CapabilityScope::Spawn {
                programs: vec![program.to_string()],
            }],
        );
        assert!(result.is_err(), "accepted invalid program {program:?}");
    }
}

#[test]
fn capability_manifest_v2_rejects_duplicate_and_malformed_scopes() {
    let duplicate_program = CapabilityManifest::new(
        CapabilityTier::SideEffecting,
        vec![CapabilityScope::Spawn {
            programs: vec!["tool".to_string(), "tool".to_string()],
        }],
    );
    assert!(duplicate_program.is_err());

    let duplicate_origin_after_normalization = CapabilityManifest::new(
        CapabilityTier::SideEffecting,
        vec![CapabilityScope::Fetch {
            origins: vec![
                "https://example.com".to_string(),
                "https://EXAMPLE.com:443".to_string(),
            ],
            methods: vec![HttpMethod::Get],
        }],
    );
    assert!(duplicate_origin_after_normalization.is_err());

    let duplicate_method = CapabilityManifest::new(
        CapabilityTier::SideEffecting,
        vec![CapabilityScope::Fetch {
            origins: vec!["https://example.com".to_string()],
            methods: vec![HttpMethod::Get, HttpMethod::Get],
        }],
    );
    assert!(duplicate_method.is_err());

    for origin in [
        "ftp://example.com",
        "https://user@example.com",
        "https://example.com/path",
        "https://example.com?query=1",
        "https://example.com#fragment",
        "https://example.com./",
    ] {
        let result = CapabilityManifest::new(
            CapabilityTier::SideEffecting,
            vec![CapabilityScope::Fetch {
                origins: vec![origin.to_string()],
                methods: vec![HttpMethod::Get],
            }],
        );
        assert!(result.is_err(), "accepted invalid origin {origin:?}");
    }

    assert!(
        CapabilityManifest::new(
            CapabilityTier::Pure,
            vec![CapabilityScope::ReadFile {
                workspace_prefixes: vec!["src".to_string()],
            }],
        )
        .is_err()
    );
    assert!(
        CapabilityManifest::new(
            CapabilityTier::ReadOnly,
            vec![CapabilityScope::Fetch {
                origins: vec!["https://example.com".to_string()],
                methods: vec![HttpMethod::Get],
            }],
        )
        .is_err()
    );
}

#[test]
fn capability_manifest_v2_deserialization_rejects_unknowns_and_non_exact_methods() {
    let unknown_manifest = serde_json::json!({
        "tier": "pure",
        "grants": [],
        "ambient_admin": true
    });
    assert!(serde_json::from_value::<CapabilityManifest>(unknown_manifest).is_err());

    let unknown_scope = serde_json::json!({
        "tier": "side_effecting",
        "grants": [{
            "kind": "spawn",
            "programs": ["tool"],
            "shell": true
        }]
    });
    assert!(serde_json::from_value::<CapabilityManifest>(unknown_scope).is_err());

    let lowercase_method = serde_json::json!({
        "tier": "side_effecting",
        "grants": [{
            "kind": "fetch",
            "origins": ["https://example.com"],
            "methods": ["get"]
        }]
    });
    assert!(serde_json::from_value::<CapabilityManifest>(lowercase_method).is_err());
}
