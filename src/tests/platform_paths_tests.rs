use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::paths::{
    AppPathError, AppPathRoot, AppPaths, LegacyArtifactKind, LegacyArtifactRequirement,
    LegacyMigrationError, LegacyMigrationRequest, LegacyMigrationStatus, PathEnvironment,
    PathOverrides, PathPlatform, PortablePathError, collision_key, ensure_no_link_traversal,
    migrate_legacy_path, prepare_storage_roots, validate_portable_component,
};

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        #[cfg(unix)]
        let temp = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        #[cfg(not(unix))]
        let temp = std::env::temp_dir();
        let path = temp.join(format!(
            "zerostack-platform-paths-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn linux_environment() -> PathEnvironment {
    PathEnvironment {
        platform: PathPlatform::Linux,
        home_dir: Some(PathBuf::from("/home/alice")),
        config_base: Some(PathBuf::from("/home/alice/.config")),
        data_base: Some(PathBuf::from("/home/alice/.local/share")),
        local_data_base: Some(PathBuf::from("/home/alice/.local/share")),
        state_base: Some(PathBuf::from("/home/alice/.local/state")),
        cache_base: Some(PathBuf::from("/home/alice/.cache")),
        workspace_root: Some(PathBuf::from("/work/project")),
        overrides: PathOverrides::default(),
    }
}

fn macos_environment() -> PathEnvironment {
    PathEnvironment {
        platform: PathPlatform::MacOs,
        home_dir: Some(PathBuf::from("/Users/alice")),
        config_base: Some(PathBuf::from("/Users/alice/Library/Application Support")),
        data_base: Some(PathBuf::from("/Users/alice/Library/Application Support")),
        local_data_base: Some(PathBuf::from("/Users/alice/Library/Application Support")),
        state_base: None,
        cache_base: Some(PathBuf::from("/Users/alice/Library/Caches")),
        workspace_root: Some(PathBuf::from("/Users/alice/work/project")),
        overrides: PathOverrides::default(),
    }
}

fn windows_environment() -> PathEnvironment {
    PathEnvironment {
        platform: PathPlatform::Windows,
        home_dir: Some(PathBuf::from(r"C:\Users\Alice")),
        config_base: Some(PathBuf::from(r"C:\Users\Alice\AppData\Roaming")),
        data_base: Some(PathBuf::from(r"C:\Users\Alice\AppData\Roaming")),
        local_data_base: Some(PathBuf::from(r"C:\Users\Alice\AppData\Local")),
        state_base: None,
        cache_base: Some(PathBuf::from(r"C:\Users\Alice\AppData\Local")),
        workspace_root: Some(PathBuf::from(r"C:\work\project")),
        overrides: PathOverrides::default(),
    }
}

fn set_override(overrides: &mut PathOverrides, variable: &str, value: OsString) {
    match variable {
        "ZS_CONFIG_DIR" => overrides.config_dir = Some(value),
        "ZS_DATA_DIR" => overrides.data_dir = Some(value),
        "ZS_LOCAL_DATA_DIR" => overrides.local_data_dir = Some(value),
        "ZS_STATE_DIR" => overrides.state_dir = Some(value),
        "ZS_CACHE_DIR" => overrides.cache_dir = Some(value),
        "ZS_CREDENTIALS_DIR" => overrides.credentials_dir = Some(value),
        _ => panic!("unknown override {variable}"),
    }
}

fn assert_default_root_contract(platform: PathPlatform, paths: &AppPaths) -> Result<(), String> {
    match platform {
        PathPlatform::Linux => {
            if paths.config_dir == paths.data_dir {
                return Err("Linux configuration mapped to portable data".to_string());
            }
            if paths.state_dir == paths.local_data_dir {
                return Err("Linux state mapped to local data".to_string());
            }
        }
        PathPlatform::MacOs => {
            if paths.config_dir != paths.data_dir || paths.data_dir != paths.local_data_dir {
                return Err("macOS Application Support roots diverged".to_string());
            }
            if paths.state_dir != paths.local_data_dir.join("state") {
                return Err("macOS state is not derived from Application Support".to_string());
            }
        }
        PathPlatform::Windows => {
            if paths.config_dir != paths.data_dir {
                return Err("Windows Roaming config/data roots diverged".to_string());
            }
            if paths.local_data_dir == paths.data_dir {
                return Err("Windows Local data mapped to Roaming".to_string());
            }
            let local = paths.local_data_dir.to_string_lossy();
            if paths.state_dir != PathBuf::from(format!(r"{local}\state"))
                || paths.cache_dir != PathBuf::from(format!(r"{local}\cache"))
                || paths.credentials_dir != PathBuf::from(format!(r"{local}\credentials"))
            {
                return Err("Windows Local child-root mapping is incorrect".to_string());
            }
        }
    }
    Ok(())
}

#[test]
fn app_paths_matrix_acceptance_defaults_on_all_platforms() {
    let linux = AppPaths::resolve(&linux_environment()).unwrap();
    assert_eq!(
        linux,
        AppPaths {
            config_dir: PathBuf::from("/home/alice/.config/zerostack"),
            data_dir: PathBuf::from("/home/alice/.local/share/zerostack"),
            local_data_dir: PathBuf::from("/home/alice/.local/share/zerostack"),
            state_dir: PathBuf::from("/home/alice/.local/state/zerostack"),
            cache_dir: PathBuf::from("/home/alice/.cache/zerostack"),
            credentials_dir: PathBuf::from(
                "/home/alice/.local/share/zerostack/credentials"
            ),
            project_dir: Some(PathBuf::from("/work/project/.zerostack")),
        }
    );
    assert_default_root_contract(PathPlatform::Linux, &linux).unwrap();

    let macos = AppPaths::resolve(&macos_environment()).unwrap();
    let application_support =
        PathBuf::from("/Users/alice/Library/Application Support/zerostack");
    assert_eq!(macos.config_dir, application_support);
    assert_eq!(macos.data_dir, application_support);
    assert_eq!(macos.local_data_dir, application_support);
    assert_eq!(macos.state_dir, application_support.join("state"));
    assert_eq!(
        macos.cache_dir,
        PathBuf::from("/Users/alice/Library/Caches/zerostack")
    );
    assert_eq!(
        macos.credentials_dir,
        application_support.join("credentials")
    );
    assert_default_root_contract(PathPlatform::MacOs, &macos).unwrap();

    let windows = AppPaths::resolve(&windows_environment()).unwrap();
    assert_eq!(
        windows.config_dir,
        PathBuf::from(r"C:\Users\Alice\AppData\Roaming\zerostack")
    );
    assert_eq!(windows.data_dir, windows.config_dir);
    assert_eq!(
        windows.local_data_dir,
        PathBuf::from(r"C:\Users\Alice\AppData\Local\zerostack")
    );
    assert_eq!(
        windows.state_dir,
        PathBuf::from(r"C:\Users\Alice\AppData\Local\zerostack\state")
    );
    assert_eq!(
        windows.cache_dir,
        PathBuf::from(r"C:\Users\Alice\AppData\Local\zerostack\cache")
    );
    assert_eq!(
        windows.credentials_dir,
        PathBuf::from(r"C:\Users\Alice\AppData\Local\zerostack\credentials")
    );
    assert_default_root_contract(PathPlatform::Windows, &windows).unwrap();
}

#[test]
fn app_paths_matrix_acceptance_all_overrides_and_precedence() {
    for mut environment in [
        linux_environment(),
        macos_environment(),
        windows_environment(),
    ] {
        let windows = environment.platform == PathPlatform::Windows;
        let absolute = |name: &str| {
            if windows {
                OsString::from(format!(r"C:\overrides\{name}"))
            } else {
                OsString::from(format!("/overrides/{name}"))
            }
        };
        environment.overrides = PathOverrides {
            config_dir: Some(absolute("config")),
            data_dir: Some(absolute("data")),
            local_data_dir: Some(absolute("local")),
            state_dir: Some(absolute("state")),
            cache_dir: Some(absolute("cache")),
            credentials_dir: Some(absolute("credentials")),
        };
        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(paths.config_dir, PathBuf::from(absolute("config")));
        assert_eq!(paths.data_dir, PathBuf::from(absolute("data")));
        assert_eq!(paths.local_data_dir, PathBuf::from(absolute("local")));
        assert_eq!(paths.state_dir, PathBuf::from(absolute("state")));
        assert_eq!(paths.cache_dir, PathBuf::from(absolute("cache")));
        assert_eq!(
            paths.credentials_dir,
            PathBuf::from(absolute("credentials"))
        );

        environment.overrides.state_dir = None;
        assert_eq!(
            AppPaths::resolve(&environment).unwrap().state_dir,
            PathBuf::from(absolute("local"))
        );
        environment.overrides.local_data_dir = None;
        assert_eq!(
            AppPaths::resolve(&environment).unwrap().state_dir,
            PathBuf::from(absolute("data"))
        );
        environment.overrides.data_dir = None;
        assert_ne!(
            AppPaths::resolve(&environment).unwrap().state_dir,
            PathBuf::from(absolute("config")),
            "configuration must never become a fallback for local/state data"
        );
    }

    const OVERRIDES: [&str; 6] = [
        "ZS_CONFIG_DIR",
        "ZS_DATA_DIR",
        "ZS_LOCAL_DATA_DIR",
        "ZS_STATE_DIR",
        "ZS_CACHE_DIR",
        "ZS_CREDENTIALS_DIR",
    ];
    for base_environment in [
        linux_environment(),
        macos_environment(),
        windows_environment(),
    ] {
        for variable in OVERRIDES {
            let mut environment = base_environment.clone();
            set_override(&mut environment.overrides, variable, OsString::new());
            assert_eq!(
                AppPaths::resolve(&environment),
                Err(AppPathError::EmptyOverride { variable })
            );

            let mut environment = base_environment.clone();
            set_override(
                &mut environment.overrides,
                variable,
                OsString::from("relative/path"),
            );
            assert_eq!(
                AppPaths::resolve(&environment),
                Err(AppPathError::RelativeOverride {
                    variable,
                    value: PathBuf::from("relative/path"),
                })
            );

            let mut environment = base_environment.clone();
            environment.home_dir = None;
            set_override(
                &mut environment.overrides,
                variable,
                OsString::from("~/private"),
            );
            assert_eq!(
                AppPaths::resolve(&environment),
                Err(AppPathError::MissingHomeForTilde { variable })
            );
        }
    }
}

#[test]
fn app_paths_matrix_acceptance_missing_bases_fail_closed() {
    let cases = [
        (PathPlatform::Linux, AppPathRoot::Config),
        (PathPlatform::Linux, AppPathRoot::Data),
        (PathPlatform::Linux, AppPathRoot::LocalData),
        (PathPlatform::Linux, AppPathRoot::State),
        (PathPlatform::Linux, AppPathRoot::Cache),
        (PathPlatform::MacOs, AppPathRoot::Config),
        (PathPlatform::MacOs, AppPathRoot::Data),
        (PathPlatform::MacOs, AppPathRoot::LocalData),
        (PathPlatform::MacOs, AppPathRoot::Cache),
        (PathPlatform::Windows, AppPathRoot::Config),
        (PathPlatform::Windows, AppPathRoot::Data),
        (PathPlatform::Windows, AppPathRoot::LocalData),
        (PathPlatform::Windows, AppPathRoot::Cache),
    ];
    for (platform, missing) in cases {
        let mut environment = match platform {
            PathPlatform::Linux => linux_environment(),
            PathPlatform::MacOs => macos_environment(),
            PathPlatform::Windows => windows_environment(),
        };
        match missing {
            AppPathRoot::Config => environment.config_base = None,
            AppPathRoot::Data => environment.data_base = None,
            AppPathRoot::LocalData => environment.local_data_base = None,
            AppPathRoot::State => environment.state_base = None,
            AppPathRoot::Cache => environment.cache_base = None,
            AppPathRoot::Workspace => unreachable!(),
        }
        assert_eq!(
            AppPaths::resolve(&environment),
            Err(AppPathError::MissingBase {
                root: missing,
                platform,
            })
        );
    }
}

fn acceptance_paths(root: &Path) -> AppPaths {
    AppPaths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        local_data_dir: root.join("local-data"),
        state_dir: root.join("state"),
        cache_dir: root.join("cache"),
        credentials_dir: root.join("credentials"),
        project_dir: Some(root.join("workspace").join(".zerostack")),
    }
}

fn assert_owned_by(path: PathBuf, root: &Path, artifact: &str) {
    assert!(
        path.starts_with(root),
        "{artifact} escaped its typed owner: {path:?} is not below {root:?}"
    );
}

#[test]
fn persistent_artifact_ownership_acceptance_covers_every_typed_owner() {
    let root = TempRoot::new("owners");
    let paths = acceptance_paths(root.path());
    let project = paths.project_dir.as_deref().unwrap();

    for (path, artifact) in [
        (paths.config_file(), "config"),
        (paths.suffix_file(), "suffix"),
        (paths.global_agents_file(), "global AGENTS"),
        (paths.global_hook_settings_file(), "global hook settings"),
    ] {
        assert_owned_by(path, &paths.config_dir, artifact);
    }
    #[cfg(feature = "archmd")]
    assert_owned_by(
        paths.global_architecture_file(),
        &paths.config_dir,
        "global architecture",
    );
    for (path, artifact) in [
        (paths.project_config_file().unwrap(), "project config"),
        (paths.project_prompts_dir().unwrap(), "project prompts"),
        (
            paths.project_agent_skills_dir().unwrap(),
            "project Agent Skills",
        ),
        (
            paths.project_hook_settings_file().unwrap(),
            "project hook settings",
        ),
    ] {
        assert_owned_by(path, project, artifact);
    }
    for (path, artifact) in [
        (paths.prompts_dir(), "global prompts"),
        (paths.themes_dir(), "themes"),
        (paths.docs_dir(), "docs"),
        (paths.memory_dir(), "memory"),
        (paths.portable_agent_skills_dir(), "portable Agent Skills"),
        (paths.theme_selection_file(), "theme selection"),
    ] {
        assert_owned_by(path, &paths.data_dir, artifact);
    }
    assert_owned_by(
        paths.learned_skills_db(),
        &paths.local_data_dir,
        "learned skills database",
    );
    for (path, artifact) in [
        (paths.embedding_models_dir(), "embedding models"),
        (paths.learned_skills_cache_dir(), "learned skills cache"),
        (paths.import_staging_dir(), "import staging"),
    ] {
        assert_owned_by(path, &paths.cache_dir, artifact);
    }
    for (path, artifact) in [
        (paths.sessions_dir(), "sessions"),
        (paths.tool_outputs_dir(), "tool outputs"),
        (paths.transcripts_dir(), "transcripts"),
        (paths.turn_telemetry_dir(), "turn telemetry"),
        (paths.logs_dir(), "logs"),
        (paths.crash_logs_dir(), "crash logs"),
        (paths.chat_history_file(), "chat history"),
        (paths.welcome_marker_file(), "welcome marker"),
        (paths.hook_trust_file(), "hook trust"),
        (paths.archmd_state_dir(), "architecture state"),
        (paths.migration_markers_dir(), "migration markers"),
    ] {
        assert_owned_by(path, &paths.state_dir, artifact);
    }
    assert_owned_by(
        paths.mcp_oauth_dir(),
        &paths.credentials_dir,
        "MCP OAuth",
    );
}

#[test]
fn legacy_path_migration_acceptance_proves_restart_retention_and_conflicts() {
    let root = TempRoot::new("migration");
    let paths = acceptance_paths(root.path());
    let legacy = root.path().join("legacy-sessions");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("session.json"), b"retained").unwrap();
    let request = LegacyMigrationRequest {
        artifact: "acceptance-sessions",
        canonical: paths.sessions_dir(),
        candidates: vec![legacy.clone()],
        marker: paths.migration_markers_dir().join("acceptance-sessions.json"),
        requirement: LegacyArtifactRequirement::Required,
        kind: LegacyArtifactKind::Directory,
        selected: None,
    };
    assert_eq!(
        migrate_legacy_path(&request).unwrap(),
        LegacyMigrationStatus::Migrated {
            source: legacy.clone()
        }
    );
    assert_eq!(
        std::fs::read(paths.sessions_dir().join("session.json")).unwrap(),
        b"retained"
    );
    assert!(legacy.join("session.json").is_file());
    assert!(request.marker.is_file());
    assert_eq!(
        migrate_legacy_path(&request).unwrap(),
        LegacyMigrationStatus::CanonicalPresent
    );

    let first = root.path().join("legacy-first");
    let second = root.path().join("legacy-second");
    std::fs::write(&first, b"first").unwrap();
    std::fs::write(&second, b"second").unwrap();
    let conflict = LegacyMigrationRequest {
        artifact: "acceptance-conflict",
        canonical: root.path().join("canonical-conflict"),
        candidates: vec![first.clone(), second.clone()],
        marker: root.path().join("conflict-marker.json"),
        requirement: LegacyArtifactRequirement::Required,
        kind: LegacyArtifactKind::File,
        selected: None,
    };
    assert!(matches!(
        migrate_legacy_path(&conflict),
        Err(LegacyMigrationError::Conflict { .. })
    ));
    assert!(!conflict.canonical.exists());

    let optional = LegacyMigrationRequest {
        requirement: LegacyArtifactRequirement::Optional,
        ..conflict
    };
    assert!(matches!(
        migrate_legacy_path(&optional).unwrap(),
        LegacyMigrationStatus::DisabledByConflict(_)
    ));
    assert!(!optional.canonical.exists());
}

#[test]
fn portable_filename_policy_acceptance_proves_reserved_and_collision_contract() {
    for reserved in [
        "CON",
        "con.txt",
        "PRN",
        "AUX.json",
        "NUL",
        "COM1",
        "com9.log",
        "LPT1",
        "lpt9.txt",
    ] {
        assert!(matches!(
            validate_portable_component(reserved),
            Err(PortablePathError::ReservedWindowsDevice { .. })
        ));
    }
    for invalid in ["trailing.", "trailing ", "name:stream", r"a\b", "a/b"] {
        assert!(validate_portable_component(invalid).is_err());
    }
    assert_eq!(
        collision_key("Résumé").unwrap(),
        collision_key("RE\u{301}SUME\u{301}").unwrap()
    );
    assert_eq!(
        collision_key("Straße").unwrap(),
        collision_key("STRASSE").unwrap()
    );
}

fn private_dacl_contract(sddl: &str) -> Result<(), String> {
    if !sddl.starts_with("D:P") {
        return Err("DACL inheritance is not protected".to_string());
    }
    if sddl.contains(";;;WD)") || sddl.contains("S-1-1-0") {
        return Err("Everyone has access".to_string());
    }
    if sddl.contains(";;;BU)") || sddl.contains("S-1-5-32-545") {
        return Err("ordinary Users have access".to_string());
    }
    Ok(())
}

#[test]
fn platform_paths_acceptance_real_platform_private_roots() {
    let root = TempRoot::new("private-roots");
    let paths = acceptance_paths(root.path());
    prepare_storage_roots(&paths).unwrap();

    for path in [
        &paths.config_dir,
        &paths.data_dir,
        &paths.local_data_dir,
        &paths.state_dir,
        &paths.cache_dir,
        &paths.credentials_dir,
    ] {
        assert!(path.is_dir(), "private root was not created: {path:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700,
                "private root has broad Unix permissions: {path:?}"
            );
        }
        #[cfg(windows)]
        {
            let dacl = crate::fs::private_dacl_sddl(path, true).unwrap();
            private_dacl_contract(&dacl).unwrap();
        }
        #[cfg(not(any(unix, windows)))]
        panic!("the platform security acceptance gate has no implementation");
    }
}

#[test]
fn platform_paths_acceptance_real_host_uses_native_base_classes() {
    let environment = PathEnvironment::from_process(None).unwrap();
    let paths = AppPaths::resolve(&environment).unwrap();
    assert!(paths.config_dir.is_absolute());
    assert!(paths.data_dir.is_absolute());
    assert!(paths.local_data_dir.is_absolute());
    assert!(paths.state_dir.is_absolute());
    assert!(paths.cache_dir.is_absolute());
    assert!(paths.credentials_dir.is_absolute());
    assert_eq!(paths.project_dir, None);

    #[cfg(target_os = "linux")]
    {
        assert_eq!(environment.platform, PathPlatform::Linux);
        assert_ne!(paths.config_dir, paths.data_dir);
        assert_ne!(paths.state_dir, paths.local_data_dir);
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(environment.platform, PathPlatform::MacOs);
        assert_eq!(paths.config_dir, paths.data_dir);
        assert_eq!(paths.data_dir, paths.local_data_dir);
        assert_eq!(paths.state_dir, paths.local_data_dir.join("state"));
        assert_ne!(paths.cache_dir, paths.local_data_dir);
    }
    #[cfg(windows)]
    {
        assert_eq!(environment.platform, PathPlatform::Windows);
        assert_eq!(paths.config_dir, paths.data_dir);
        assert_ne!(paths.local_data_dir, paths.data_dir);
        assert_default_root_contract(PathPlatform::Windows, &paths).unwrap();
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    panic!("the native application-path acceptance gate has no implementation");
}

#[test]
fn platform_paths_acceptance_real_platform_rejects_links_and_reparse_points() {
    let root = TempRoot::new("link-gate");
    let contained = root.path().join("contained");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&contained).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let link = contained.join("escape");

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(matches!(
            ensure_no_link_traversal(&contained, &link.join("secret")),
            Err(PortablePathError::LinkTraversal { .. })
        ));
        let migration = LegacyMigrationRequest {
            artifact: "linked-directory",
            canonical: root.path().join("canonical"),
            candidates: vec![link.clone()],
            marker: root.path().join("linked-directory-marker.json"),
            requirement: LegacyArtifactRequirement::Required,
            kind: LegacyArtifactKind::Directory,
            selected: None,
        };
        assert!(migrate_legacy_path(&migration).is_err());
        assert!(!migration.canonical.exists());
        std::fs::remove_file(&link).unwrap();
    }
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                link.to_str().unwrap(),
                outside.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "fixture must create a real junction");
        assert!(matches!(
            ensure_no_link_traversal(&contained, &link.join("secret")),
            Err(PortablePathError::LinkTraversal { .. })
        ));
        let migration = LegacyMigrationRequest {
            artifact: "linked-directory",
            canonical: root.path().join("canonical"),
            candidates: vec![link.clone()],
            marker: root.path().join("linked-directory-marker.json"),
            requirement: LegacyArtifactRequirement::Required,
            kind: LegacyArtifactKind::Directory,
            selected: None,
        };
        assert!(migrate_legacy_path(&migration).is_err());
        assert!(!migration.canonical.exists());
        std::fs::remove_dir(&link).unwrap();
    }
    #[cfg(not(any(unix, windows)))]
    panic!("the link/reparse acceptance gate has no implementation");
}

fn migration_observation_contract(
    differing_candidates: bool,
    explicit_selection: bool,
    migrated: bool,
) -> Result<(), String> {
    if differing_candidates && !explicit_selection && migrated {
        return Err("migration silently selected a conflicting candidate".to_string());
    }
    Ok(())
}

fn reserved_name_observation_contract(
    name: &str,
    result: Result<(), PortablePathError>,
) -> Result<(), String> {
    let uppercase = name.to_ascii_uppercase();
    let reserved = matches!(
        uppercase.split('.').next(),
        Some("CON" | "PRN" | "AUX" | "NUL")
    );
    if reserved && result.is_ok() {
        return Err("generated reserved name was accepted".to_string());
    }
    Ok(())
}

fn rejected_negative_control(control: &str) -> Result<(), String> {
    match control {
        "config-to-data" => {
            let mut paths = AppPaths::resolve(&linux_environment()).unwrap();
            paths.config_dir = paths.data_dir.clone();
            assert_default_root_contract(PathPlatform::Linux, &paths)
        }
        "windows-local-to-roaming" => {
            let mut paths = AppPaths::resolve(&windows_environment()).unwrap();
            paths.local_data_dir = paths.data_dir.clone();
            assert_default_root_contract(PathPlatform::Windows, &paths)
        }
        "credential-acl-broad-read" | "session-acl-broad-read" => {
            private_dacl_contract("D:P(A;;GR;;;WD)")
        }
        "migration-silent-conflict" => migration_observation_contract(true, false, true),
        "reserved-name-accepted" => reserved_name_observation_contract("CON", Ok(())),
        _ => panic!("unknown platform-path negative control {control:?}"),
    }
}

#[test]
fn platform_paths_acceptance_intentional_negative_controls() {
    const CONTROLS: [&str; 6] = [
        "config-to-data",
        "windows-local-to-roaming",
        "credential-acl-broad-read",
        "session-acl-broad-read",
        "migration-silent-conflict",
        "reserved-name-accepted",
    ];
    for control in CONTROLS {
        assert!(
            rejected_negative_control(control).is_err(),
            "acceptance oracle did not reject {control}"
        );
    }

    if let Ok(control) = std::env::var("ZS_PLATFORM_PATH_NEGATIVE_CONTROL") {
        rejected_negative_control(&control)
            .expect("intentional negative control must make the acceptance test fail");
    }
}
