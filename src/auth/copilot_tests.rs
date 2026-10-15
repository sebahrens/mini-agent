use super::*;
use serde_json::json;
pub(super) fn profile() -> Profile {
    Profile {
        host: "github.com".into(),
        client_id: Some("owned-mini-agent-app".into()),
        endpoints: None,
    }
}
pub(super) fn store(profile: Profile) -> Store {
    Store {
        version: 1,
        revision: 0,
        generation: uuid::Uuid::new_v4().to_string(),
        profile,
        account: 7,
        login: "owned-user".into(),
        tokens: Some(GitHubTokens {
            access: "PRIVATE_GITHUB_ACCESS".into(),
            refresh: None,
            expires: None,
            refresh_expires: None,
            verified: true,
        }),
        model: None,
    }
}
#[test]
fn copilot81_profile_and_model_destinations_are_bound_to_selected_github_host() {
    let github = profile();
    github.validate().unwrap();
    assert_eq!(
        github.model_origin(None).unwrap(),
        "https://api.githubcopilot.com"
    );
    assert_eq!(
        github
            .model_origin(Some("https://api.enterprise.githubcopilot.com"))
            .unwrap(),
        "https://api.enterprise.githubcopilot.com"
    );
    for bad in [
        "http://api.githubcopilot.com",
        "https://api.githubcopilot.com.evil.example",
        "https://user:PRIVATE@api.githubcopilot.com",
        "https://api.githubcopilot.com/v1",
        "https://api.githubcopilot.com?PRIVATE",
    ] {
        let error = github.model_origin(Some(bad)).unwrap_err().to_string();
        assert!(!error.contains("PRIVATE"));
    }
    let enterprise = Profile {
        host: "owned.ghe.com".into(),
        client_id: github.client_id.clone(),
        endpoints: None,
    };
    assert_eq!(
        enterprise.model_origin(None).unwrap(),
        "https://copilot-api.owned.ghe.com"
    );
    assert!(
        enterprise
            .model_origin(Some("https://api.githubcopilot.com"))
            .is_err()
    );
    assert!(
        enterprise
            .model_origin(Some("https://copilot-api.other.ghe.com"))
            .is_err()
    );
    for host in [
        "../other",
        "github.com/evil",
        "github.com.evil.example",
        "owned.ghe.com.evil.example",
        "evil.owned.ghe.com",
    ] {
        assert!(Profile::for_host(host).is_err());
    }
}
#[test]
fn copilot81_private_store_preserves_generation_and_never_serializes_debug_secrets() {
    let temp = crate::tests::OwnedTestDirectory::new("copilot81-store");
    let directory = temp.path().join("private");
    let mut locked = LockedStore::open(directory.clone(), true).unwrap().unwrap();
    locked.data = Some(store(profile()));
    locked.save().unwrap();
    assert!(
        LockedStore::open(directory.clone(), false).is_err(),
        "concurrent writer must not replace selected credentials"
    );
    let data = locked.data.as_ref().unwrap();
    let registration = Registration {
        directory: directory.clone(),
        generation: data.generation.clone(),
        profile: data.profile.clone(),
    };
    assert!(!format!("{registration:?}").contains("PRIVATE_GITHUB_ACCESS"));
    drop(locked);
    let locked = LockedStore::open(directory.clone(), false)
        .unwrap()
        .unwrap();
    assert_eq!(locked.data.as_ref().unwrap().revision, 1);
    drop(locked);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(directory.join("session.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::set_permissions(
            directory.join("session.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(LockedStore::open(directory, false).is_err());
    }
}
#[cfg(unix)]
#[test]
fn copilot81_private_store_rejects_a_symlinked_token_record() {
    let temp = crate::tests::OwnedTestDirectory::new("copilot81-link");
    let directory = temp.path().join("private");
    crate::fs::ensure_private_directory(&directory).unwrap();
    let target = temp.path().join("target.json");
    crate::fs::private_atomic_write_sync(&target, &serde_json::to_vec(&store(profile())).unwrap())
        .unwrap();
    std::os::unix::fs::symlink(&target, directory.join("session.json")).unwrap();
    assert!(LockedStore::open(directory, false).is_err());
}
#[test]
fn copilot81_oauth_tokens_are_typed_bounded_and_expiring_without_echoing_values() {
    for value in [
        json!({"access_token":"PRIVATE","token_type":"basic"}),
        json!({"access_token":"PRIVATE","token_type":"bearer","expires_in":0}),
        json!({"access_token":"PRIVATE","token_type":"bearer","expires_in":u64::MAX}),
        json!({"access_token":"x".repeat(MAX_TOKEN+1),"token_type":"bearer"}),
    ] {
        let error = tokens(value).err().unwrap().to_string();
        assert!(!error.contains("PRIVATE"));
    }
    let token=tokens(json!({"access_token":"OWNED","token_type":"bearer","expires_in":3600,"refresh_token":"OWNED_REFRESH","refresh_token_expires_in":7200})).unwrap();
    assert!(token.expires.unwrap() > now().unwrap());
    assert_eq!(token.refresh.as_deref(), Some("OWNED_REFRESH"));
}

#[test]
fn copilot81_cli_and_slash_commands_share_provider_login_surfaces() {
    use clap::Parser;
    for command in ["--login", "--logout"] {
        let cli = crate::cli::Cli::try_parse_from(["mini-agent", command, "copilot"]).unwrap();
        assert!(cli.api_key.is_none());
        assert_eq!(
            if command == "--login" {
                cli.login
            } else {
                cli.logout
            }
            .as_deref(),
            Some("copilot")
        );
    }
    for command in ["/login", "/logout"] {
        assert!(crate::ui::slash::routes_to_providers(command));
        assert!(crate::ui::pickers::list::available_commands().contains(&command));
    }
}

#[test]
fn login_labels47_copilot_marker_requires_a_private_record_for_the_captured_host() {
    use crate::auth::{CredentialSource, LoginRecordState as S};
    use crate::diagnostics::{CredentialReport as R, provider_credential_with_env as inspect};
    use std::env::VarError;
    let root = crate::tests::OwnedTestDirectory::new("labels47-copilot");
    let credentials = root.path().join("credentials");
    let env = |_: &str| Err(VarError::NotPresent);
    let cfg = crate::config::Config::default();
    assert_eq!(diagnostic_state_at(&credentials, "github.com"), S::Absent);
    assert_eq!(
        inspect(&cfg, "copilot", None, &credentials, env),
        R::Copilot(S::Absent)
    );
    assert_eq!(
        inspect(&cfg, "copilot", None, &credentials, env).setup_label(),
        "auth: Copilot sign-in required"
    );
    assert!(!credentials.exists());
    let directory = credentials.join("copilot/github.com");
    crate::fs::ensure_private_directory(&directory).unwrap();
    let path = directory.join("session.json");
    let mut data = store(profile());
    let bytes = serde_json::to_vec(&data).unwrap();
    crate::fs::private_atomic_write_sync(&path, &bytes).unwrap();
    assert_eq!(
        inspect(&cfg, "copilot", None, &credentials, env),
        R::Copilot(S::Stored)
    );
    assert_eq!(
        inspect(&cfg, "copilot", None, &credentials, env).setup_label(),
        "auth: saved Copilot login"
    );
    assert_eq!(
        inspect(&cfg, "copilot", Some("PRIVATE_CLI"), &credentials, env),
        R::Source(CredentialSource::Cli)
    );
    assert_eq!(
        inspect(&cfg, "copilot", None, &credentials, |name| {
            if name == "COPILOT_GITHUB_HOST" {
                Ok("other.ghe.com".into())
            } else {
                env(name)
            }
        }),
        R::Copilot(S::Absent)
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(!directory.join("session.lock").exists());
    data.tokens = None;
    crate::fs::private_atomic_write_sync(&path, &serde_json::to_vec(&data).unwrap()).unwrap();
    assert_eq!(diagnostic_state_at(&credentials, "github.com"), S::Absent);
    crate::fs::private_atomic_write_sync(&path, b"bad PRIVATE_RECORD").unwrap();
    assert_eq!(
        inspect(&cfg, "copilot", None, &credentials, env),
        R::Copilot(S::Unavailable)
    );
    let custom: crate::config::Config = serde_json::from_value(serde_json::json!({"custom_providers":{"gateway":{"provider_type":"copilot","base_url":"https://gateway.example"}}})).unwrap();
    assert_eq!(
        inspect(&custom, "gateway", None, &credentials, env),
        R::Missing
    );
    assert_eq!(
        inspect(&cfg, "copilot", Some("PRIVATE_CLI"), &credentials, env),
        R::Source(CredentialSource::Cli)
    );
    assert!(!directory.join("session.lock").exists());
}
