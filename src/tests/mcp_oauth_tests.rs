use crate::extras::mcp::config::{
    DEFAULT_REDIRECT_PORT, McpServerConfig, OAuthConfig, OAuthSettings,
};
use crate::extras::mcp::oauth;
use crate::paths::AppPaths;
use rmcp::transport::auth::StoredCredentials;
use std::path::{Path, PathBuf};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "mini-agent-mcp-oauth-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_paths(root: &Path) -> AppPaths {
    AppPaths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        local_data_dir: root.join("local"),
        state_dir: root.join("state"),
        cache_dir: root.join("cache"),
        credentials_dir: root.join("local").join("credentials"),
        project_dir: None,
    }
}

fn test_credentials(client_id: &str) -> StoredCredentials {
    StoredCredentials::new(client_id.to_string(), None, Vec::new(), None)
}

#[test]
fn url_server_without_oauth_parses() {
    let json = r#"{ "url": "https://example.com/mcp" }"#;
    let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
    match cfg {
        McpServerConfig::Url { url, oauth, .. } => {
            assert_eq!(url, "https://example.com/mcp");
            assert!(oauth.is_none());
        }
        _ => panic!("expected Url variant"),
    }
}

#[test]
fn oauth_true_enables_with_defaults() {
    let json = r#"{ "url": "https://example.com/mcp", "oauth": true }"#;
    let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
    let McpServerConfig::Url { oauth, .. } = cfg else {
        panic!("expected Url variant");
    };
    let settings = oauth.unwrap().settings().expect("oauth enabled");
    assert!(settings.scopes.is_empty());
    assert!(settings.client_id.is_none());
    assert_eq!(settings.redirect_port(), DEFAULT_REDIRECT_PORT);
}

#[test]
fn oauth_false_disables() {
    let json = r#"{ "url": "https://example.com/mcp", "oauth": false }"#;
    let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
    let McpServerConfig::Url { oauth, .. } = cfg else {
        panic!("expected Url variant");
    };
    assert!(oauth.unwrap().settings().is_none());
}

#[test]
fn oauth_object_parses_fields() {
    let json = r#"{
        "url": "https://example.com/mcp",
        "oauth": { "scopes": ["read", "write"], "client_id": "abc", "redirect_port": 9123 }
    }"#;
    let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
    let McpServerConfig::Url { oauth, .. } = cfg else {
        panic!("expected Url variant");
    };
    let settings = oauth.unwrap().settings().unwrap();
    assert_eq!(
        settings.scopes,
        vec!["read".to_string(), "write".to_string()]
    );
    assert_eq!(settings.client_id.as_deref(), Some("abc"));
    assert_eq!(settings.redirect_port(), 9123);
    assert_eq!(settings.redirect_uri(), "http://127.0.0.1:9123/callback");
}

#[test]
fn default_redirect_uri_uses_loopback() {
    let settings = OAuthSettings::default();
    assert_eq!(
        settings.redirect_uri(),
        format!("http://127.0.0.1:{DEFAULT_REDIRECT_PORT}/callback")
    );
}

#[test]
fn token_filename_uses_complete_canonical_server_identity() {
    let first = oauth::token_filename(
        "Exa Web Search",
        "HTTPS://EXAMPLE.COM:443/mcp#ignored",
        Some("client"),
    )
    .unwrap();
    let equivalent =
        oauth::token_filename("Exa Web Search", "https://example.com/mcp", Some("client")).unwrap();
    assert_eq!(first, equivalent);
    assert_eq!(first.len(), 69);
    assert!(first.ends_with(".json"));
    assert!(first[..64].bytes().all(|byte| byte.is_ascii_hexdigit()));

    assert_ne!(
        first,
        oauth::token_filename("Exa/Web Search", "https://example.com/mcp", Some("client")).unwrap()
    );
    assert_ne!(
        first,
        oauth::token_filename(
            "Exa Web Search",
            "https://example.com/other",
            Some("client")
        )
        .unwrap()
    );
    assert_ne!(
        first,
        oauth::token_filename("Exa Web Search", "https://example.com/mcp", Some("other")).unwrap()
    );
}

#[test]
fn mcp_oauth_identity_reserved_colliding_and_unicode_names_are_opaque() {
    let names = [
        "CON", "server.", "server ", "a/b", "a\\b", "Straße", "STRASSE",
    ];
    let mut filenames = std::collections::HashSet::new();
    for name in names {
        let filename =
            oauth::token_filename(name, "https://例え.テスト:443/mcp?x=%2F", None).unwrap();
        assert_eq!(filename.len(), 69);
        assert!(filename.ends_with(".json"));
        assert!(filename[..64].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(filenames.insert(filename), "identity alias for {name:?}");
    }
}

#[test]
fn mcp_oauth_identity_maps_only_to_the_credentials_root() {
    let root = TempDir::new("root-map");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let path = oauth::token_path(
        &paths,
        "../display-name",
        "https://example.com/mcp",
        &settings,
    )
    .unwrap();
    let expected_parent = paths.credentials_dir.join("mcp-oauth");
    assert_eq!(path.parent(), Some(expected_parent.as_path()));
    assert!(!path.to_string_lossy().contains("display-name"));
}

#[cfg(unix)]
#[test]
fn mcp_oauth_storage_security_creates_private_root_final_and_lock() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new("unix-modes");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let store =
        oauth::FileCredentialStore::for_paths(&paths, "server", "https://example.com", &settings)
            .unwrap();
    store.write_blocking(&test_credentials("client")).unwrap();

    let directory = paths.credentials_dir.join("mcp-oauth");
    let token = oauth::token_path(&paths, "server", "https://example.com", &settings).unwrap();
    let lock = token.with_extension("json.lock");
    assert_eq!(
        std::fs::metadata(&paths.credentials_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(token).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(lock).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(windows)]
#[test]
fn mcp_oauth_storage_security_windows_dacls_exclude_broad_principals() {
    let root = TempDir::new("windows-dacl");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let store =
        oauth::FileCredentialStore::for_paths(&paths, "server", "https://example.com", &settings)
            .unwrap();
    store.write_blocking(&test_credentials("client")).unwrap();

    let directory = paths.credentials_dir.join("mcp-oauth");
    let token = oauth::token_path(&paths, "server", "https://example.com", &settings).unwrap();
    let lock = token.with_extension("json.lock");
    for (path, is_directory) in [
        (&paths.credentials_dir, true),
        (&directory, true),
        (&token, false),
        (&lock, false),
    ] {
        let dacl = oauth::windows_private_dacl_sddl(path, is_directory).unwrap();
        assert!(
            dacl.starts_with("D:P"),
            "DACL inherits broad grants: {dacl}"
        );
        assert!(
            !dacl.contains(";;;WD)") && !dacl.contains("S-1-1-0"),
            "Everyone can access credential content: {dacl}"
        );
        assert!(
            !dacl.contains(";;;BU)") && !dacl.contains("S-1-5-32-545"),
            "ordinary Users can access credential content: {dacl}"
        );
    }
}

#[cfg(unix)]
#[test]
fn mcp_oauth_storage_security_rejects_symlink_and_repairs_owned_mode() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = TempDir::new("symlink");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let store =
        oauth::FileCredentialStore::for_paths(&paths, "server", "https://example.com", &settings)
            .unwrap();
    store.write_blocking(&test_credentials("client")).unwrap();
    let token = oauth::token_path(&paths, "server", "https://example.com", &settings).unwrap();

    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o666)).unwrap();
    store.read_blocking().unwrap();
    assert_eq!(
        std::fs::metadata(&token).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let outside = root.path().join("outside.json");
    std::fs::write(&outside, b"do not read").unwrap();
    std::fs::remove_file(&token).unwrap();
    symlink(&outside, &token).unwrap();
    assert!(store.read_blocking().is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"do not read");
}

#[test]
fn mcp_oauth_storage_security_bounds_records_and_redacts_diagnostics() {
    let root = TempDir::new("bounded");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let store =
        oauth::FileCredentialStore::for_paths(&paths, "server", "https://example.com", &settings)
            .unwrap();
    store.write_blocking(&test_credentials("client")).unwrap();
    let token = oauth::token_path(&paths, "server", "https://example.com", &settings).unwrap();

    std::fs::write(&token, vec![b'x'; 1024 * 1024 + 1]).unwrap();
    assert!(store.read_blocking().is_err());

    let secret = "refresh-secret-must-not-appear";
    std::fs::write(&token, format!(r#"{{"refresh_token":"{secret}","broken":"#)).unwrap();
    let diagnostic = store.read_blocking().unwrap_err().to_string();
    assert!(!diagnostic.contains(secret));
}

#[test]
fn mcp_oauth_storage_security_concurrent_writes_publish_complete_json() {
    let root = TempDir::new("concurrent");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let mut threads = Vec::new();
    for index in 0..8 {
        let paths = paths.clone();
        let settings = settings.clone();
        threads.push(std::thread::spawn(move || {
            let store = oauth::FileCredentialStore::for_paths(
                &paths,
                "server",
                "https://example.com",
                &settings,
            )
            .unwrap();
            store
                .write_blocking(&test_credentials(&format!("client-{index}")))
                .unwrap();
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }
    let store =
        oauth::FileCredentialStore::for_paths(&paths, "server", "https://example.com", &settings)
            .unwrap();
    assert!(store.read_blocking().unwrap().is_some());
}

#[test]
fn mcp_oauth_storage_security_refresh_logout_race_is_restart_safe() {
    let root = TempDir::new("refresh-logout");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let mut threads = Vec::new();
    for index in 0..16 {
        let paths = paths.clone();
        let settings = settings.clone();
        threads.push(std::thread::spawn(move || {
            let store = oauth::FileCredentialStore::for_paths(
                &paths,
                "server",
                "https://example.com",
                &settings,
            )
            .unwrap();
            if index % 2 == 0 {
                store
                    .write_blocking(&test_credentials(&format!("client-{index}")))
                    .unwrap();
            } else {
                store.clear_blocking().unwrap();
            }
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }

    let store =
        oauth::FileCredentialStore::for_paths(&paths, "server", "https://example.com", &settings)
            .unwrap();
    let _complete_or_absent_record = store.read_blocking().unwrap();
    let directory = paths.credentials_dir.join("mcp-oauth");
    assert!(std::fs::read_dir(directory).unwrap().all(|entry| {
        let name = entry.unwrap().file_name();
        let name = name.to_string_lossy();
        !name.starts_with(".oauth-") && !name.starts_with(".zswrite.")
    }));
}

#[test]
fn mcp_oauth_migration_is_idempotent_and_retains_legacy_source() {
    let root = TempDir::new("migration");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let legacy_dir = paths.data_dir.join("mcp-oauth");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    let legacy = legacy_dir.join("server.json");
    std::fs::write(
        &legacy,
        serde_json::to_vec_pretty(&test_credentials("legacy-client")).unwrap(),
    )
    .unwrap();
    let store =
        oauth::FileCredentialStore::for_paths(&paths, "server", "https://example.com", &settings)
            .unwrap();

    assert!(store.read_blocking().unwrap().is_some());
    let canonical = oauth::token_path(&paths, "server", "https://example.com", &settings).unwrap();
    assert!(canonical.is_file());
    assert!(legacy.is_file());
    assert!(store.read_blocking().unwrap().is_some());
    assert!(legacy.is_file());
}

#[test]
fn mcp_oauth_migration_logout_does_not_reimport_retained_source() {
    let root = TempDir::new("migration-logout");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let legacy_dir = paths.data_dir.join("mcp-oauth");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    let legacy = legacy_dir.join("server.json");
    std::fs::write(
        &legacy,
        serde_json::to_vec_pretty(&test_credentials("legacy-client")).unwrap(),
    )
    .unwrap();
    let store =
        oauth::FileCredentialStore::for_paths(&paths, "server", "https://example.com", &settings)
            .unwrap();

    assert!(store.read_blocking().unwrap().is_some());
    assert!(store.clear_blocking().unwrap());
    assert!(store.read_blocking().unwrap().is_none());
    assert!(legacy.exists());
    let canonical = oauth::token_path(&paths, "server", "https://example.com", &settings).unwrap();
    assert!(!canonical.exists());
    assert!(canonical.with_extension("migration.json").is_file());
}

#[test]
fn mcp_oauth_migration_fails_closed_on_sanitized_collision() {
    let root = TempDir::new("migration-collision");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let legacy_dir = paths.data_dir.join("mcp-oauth");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    let bytes = serde_json::to_vec_pretty(&test_credentials("legacy-client")).unwrap();
    std::fs::write(legacy_dir.join("alpha beta.json"), &bytes).unwrap();
    std::fs::write(legacy_dir.join("alpha_beta.json"), &bytes).unwrap();
    let store = oauth::FileCredentialStore::for_paths(
        &paths,
        "alpha beta",
        "https://example.com",
        &settings,
    )
    .unwrap();

    assert!(store.read_blocking().is_err());
    let canonical =
        oauth::token_path(&paths, "alpha beta", "https://example.com", &settings).unwrap();
    assert!(!canonical.exists());
}

#[test]
fn mcp_oauth_migration_rejects_a_different_explicit_client_identity() {
    let root = TempDir::new("migration-client");
    let paths = test_paths(root.path());
    let settings = OAuthSettings {
        client_id: Some("configured-client".to_string()),
        ..OAuthSettings::default()
    };
    let legacy_dir = paths.data_dir.join("mcp-oauth");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    std::fs::write(
        legacy_dir.join("server.json"),
        serde_json::to_vec_pretty(&test_credentials("different-client")).unwrap(),
    )
    .unwrap();
    let store =
        oauth::FileCredentialStore::for_paths(&paths, "server", "https://example.com", &settings)
            .unwrap();

    assert!(store.read_blocking().is_err());
    let canonical = oauth::token_path(&paths, "server", "https://example.com", &settings).unwrap();
    assert!(!canonical.exists());
}

#[test]
fn mcp_oauth_migration_malformed_source_never_publishes_canonical_content() {
    let root = TempDir::new("migration-malformed");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let legacy_dir = paths.data_dir.join("mcp-oauth");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    let legacy = legacy_dir.join("server.json");
    std::fs::write(&legacy, br#"{"refresh_token":"do-not-log","broken":"#).unwrap();
    let store =
        oauth::FileCredentialStore::for_paths(&paths, "server", "https://example.com", &settings)
            .unwrap();

    let diagnostic = store.read_blocking().unwrap_err().to_string();
    assert!(!diagnostic.contains("do-not-log"));
    let canonical = oauth::token_path(&paths, "server", "https://example.com", &settings).unwrap();
    assert!(!canonical.exists());
    assert!(legacy.exists());
}

#[cfg(unix)]
#[test]
fn mcp_oauth_migration_fails_closed_on_nonportable_legacy_name() {
    let root = TempDir::new("migration-reserved");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let legacy_dir = paths.data_dir.join("mcp-oauth");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    std::fs::write(
        legacy_dir.join("CON.json"),
        serde_json::to_vec_pretty(&test_credentials("legacy-client")).unwrap(),
    )
    .unwrap();
    let store =
        oauth::FileCredentialStore::for_paths(&paths, "CON", "https://example.com", &settings)
            .unwrap();

    assert!(store.read_blocking().is_err());
    let canonical = oauth::token_path(&paths, "CON", "https://example.com", &settings).unwrap();
    assert!(!canonical.exists());
}

#[cfg(unix)]
#[test]
fn mcp_oauth_migration_never_follows_a_legacy_root_symlink() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new("migration-root-symlink");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(
        outside.join("server.json"),
        serde_json::to_vec_pretty(&test_credentials("outside-client")).unwrap(),
    )
    .unwrap();
    std::fs::create_dir_all(&paths.data_dir).unwrap();
    symlink(&outside, paths.data_dir.join("mcp-oauth")).unwrap();
    let store =
        oauth::FileCredentialStore::for_paths(&paths, "server", "https://example.com", &settings)
            .unwrap();

    assert!(store.read_blocking().is_err());
    let canonical = oauth::token_path(&paths, "server", "https://example.com", &settings).unwrap();
    assert!(!canonical.exists());
}

#[test]
fn mcp_oauth_storage_security_logout_removes_only_canonical_record() {
    let root = TempDir::new("logout");
    let paths = test_paths(root.path());
    let settings = OAuthSettings::default();
    let first =
        oauth::FileCredentialStore::for_paths(&paths, "first", "https://example.com", &settings)
            .unwrap();
    let second =
        oauth::FileCredentialStore::for_paths(&paths, "second", "https://example.com", &settings)
            .unwrap();
    first.write_blocking(&test_credentials("first")).unwrap();
    second.write_blocking(&test_credentials("second")).unwrap();

    assert!(first.clear_blocking().unwrap());
    assert!(!first.clear_blocking().unwrap());
    assert!(second.read_blocking().unwrap().is_some());
}

#[test]
fn parse_callback_extracts_code_and_state() {
    let line = "GET /callback?code=abc123&state=xyz789 HTTP/1.1";
    let (code, state) = oauth::parse_callback(line).unwrap();
    assert_eq!(code, "abc123");
    assert_eq!(state, "xyz789");
}

#[test]
fn parse_callback_decodes_percent_escapes() {
    let line = "GET /callback?code=a%2Fb%2Bc&state=s%20t HTTP/1.1";
    let (code, state) = oauth::parse_callback(line).unwrap();
    assert_eq!(code, "a/b+c");
    assert_eq!(state, "s t");
}

#[test]
fn parse_callback_reports_server_error() {
    let line = "GET /callback?error=access_denied HTTP/1.1";
    assert!(oauth::parse_callback(line).is_err());
}

#[test]
fn parse_callback_missing_params_errors() {
    let line = "GET /callback?code=only HTTP/1.1";
    assert!(oauth::parse_callback(line).is_err());
}

#[test]
fn percent_decode_handles_plus_and_hex() {
    assert_eq!(oauth::percent_decode("a+b"), "a b");
    assert_eq!(oauth::percent_decode("%41%42"), "AB");
    assert_eq!(oauth::percent_decode("nochange"), "nochange");
    // Malformed escape is left as-is.
    assert_eq!(oauth::percent_decode("%zz"), "%zz");
}

#[test]
fn oauth_config_round_trips_through_serde() {
    let cfg = McpServerConfig::Url {
        url: "https://example.com/mcp".to_string(),
        headers: Default::default(),
        oauth: Some(OAuthConfig::Enabled(true)),
    };
    let json = serde_json::to_string(&cfg).unwrap();
    let back: McpServerConfig = serde_json::from_str(&json).unwrap();
    let McpServerConfig::Url { oauth, .. } = back else {
        panic!("expected Url variant");
    };
    assert!(oauth.unwrap().settings().is_some());
}
