use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

// These shared path-policy primitives are exercised by the acceptance suite
// before all of their production consumers land.
#[allow(dead_code)]
pub mod portable;

#[allow(unused_imports)]
pub use portable::{
    MAX_PORTABLE_COMPONENT_BYTES, MAX_PORTABLE_COMPONENT_UTF16_UNITS, MAX_PORTABLE_PATH_BYTES,
    MAX_PORTABLE_PATH_UTF16_UNITS, PortablePathError, collision_key, contained_join,
    ensure_contained, ensure_no_link_traversal, validate_portable_relative_path,
};
pub use portable::{digest_filename, opaque_name, validate_portable_component};

const APP_COMPONENT: &str = "zerostack";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPlatform {
    Linux,
    MacOs,
    Windows,
}

impl PathPlatform {
    fn current() -> Result<Self, AppPathError> {
        if cfg!(target_os = "linux") {
            Ok(Self::Linux)
        } else if cfg!(target_os = "macos") {
            Ok(Self::MacOs)
        } else if cfg!(target_os = "windows") {
            Ok(Self::Windows)
        } else {
            Err(AppPathError::UnsupportedPlatform)
        }
    }
}

impl std::fmt::Display for PathPlatform {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Linux => formatter.write_str("Linux"),
            Self::MacOs => formatter.write_str("macOS"),
            Self::Windows => formatter.write_str("Windows"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPathRoot {
    Config,
    Data,
    LocalData,
    State,
    Cache,
    Workspace,
}

impl std::fmt::Display for AppPathRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config => formatter.write_str("configuration"),
            Self::Data => formatter.write_str("portable data"),
            Self::LocalData => formatter.write_str("local data"),
            Self::State => formatter.write_str("state"),
            Self::Cache => formatter.write_str("cache"),
            Self::Workspace => formatter.write_str("workspace"),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AppPathError {
    #[error("this operating system is not supported")]
    UnsupportedPlatform,
    #[error("required {root} base directory is unavailable on {platform}")]
    MissingBase {
        root: AppPathRoot,
        platform: PathPlatform,
    },
    #[error("{variable} is set but empty")]
    EmptyOverride { variable: &'static str },
    #[error("{variable} uses '~', but the home directory is unavailable")]
    MissingHomeForTilde { variable: &'static str },
    #[error("{variable} must be an absolute path, got {value:?}")]
    RelativeOverride {
        variable: &'static str,
        value: PathBuf,
    },
    #[error("the {root} base directory must be absolute, got {value:?}")]
    RelativeBase { root: AppPathRoot, value: PathBuf },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathOverrides {
    pub config_dir: Option<OsString>,
    pub data_dir: Option<OsString>,
    pub local_data_dir: Option<OsString>,
    pub state_dir: Option<OsString>,
    pub cache_dir: Option<OsString>,
    pub credentials_dir: Option<OsString>,
}

impl PathOverrides {
    fn from_process() -> Self {
        Self {
            config_dir: std::env::var_os("ZS_CONFIG_DIR"),
            data_dir: std::env::var_os("ZS_DATA_DIR"),
            local_data_dir: std::env::var_os("ZS_LOCAL_DATA_DIR"),
            state_dir: std::env::var_os("ZS_STATE_DIR"),
            cache_dir: std::env::var_os("ZS_CACHE_DIR"),
            credentials_dir: std::env::var_os("ZS_CREDENTIALS_DIR"),
        }
    }
}

/// All host inputs used to resolve application paths.
///
/// Production captures this value once. Tests construct it directly, avoiding
/// process-global environment mutation and host-dependent expectations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEnvironment {
    pub platform: PathPlatform,
    pub home_dir: Option<PathBuf>,
    pub config_base: Option<PathBuf>,
    pub data_base: Option<PathBuf>,
    pub local_data_base: Option<PathBuf>,
    pub state_base: Option<PathBuf>,
    pub cache_base: Option<PathBuf>,
    pub workspace_root: Option<PathBuf>,
    pub overrides: PathOverrides,
}

impl PathEnvironment {
    pub fn from_process(workspace_root: Option<PathBuf>) -> Result<Self, AppPathError> {
        Ok(Self {
            platform: PathPlatform::current()?,
            home_dir: dirs::home_dir(),
            config_base: dirs::config_dir(),
            data_base: dirs::data_dir(),
            local_data_base: dirs::data_local_dir(),
            state_base: dirs::state_dir(),
            cache_base: dirs::cache_dir(),
            workspace_root,
            overrides: PathOverrides::from_process(),
        })
    }
}

/// Immutable, fully resolved roots for all persistent application storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub local_data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub credentials_dir: PathBuf,
    pub project_dir: Option<PathBuf>,
}

impl AppPaths {
    pub fn from_process(workspace_root: Option<PathBuf>) -> Result<Self, AppPathError> {
        Self::resolve(&PathEnvironment::from_process(workspace_root)?)
    }

    pub fn resolve(environment: &PathEnvironment) -> Result<Self, AppPathError> {
        let platform = environment.platform;
        let config_override = resolve_override(
            environment,
            "ZS_CONFIG_DIR",
            environment.overrides.config_dir.as_deref(),
        )?;
        let data_override = resolve_override(
            environment,
            "ZS_DATA_DIR",
            environment.overrides.data_dir.as_deref(),
        )?;
        let local_data_override = resolve_override(
            environment,
            "ZS_LOCAL_DATA_DIR",
            environment.overrides.local_data_dir.as_deref(),
        )?;
        let state_override = resolve_override(
            environment,
            "ZS_STATE_DIR",
            environment.overrides.state_dir.as_deref(),
        )?;

        let config_dir = match config_override {
            Some(path) => path,
            None => default_root(environment, AppPathRoot::Config, &environment.config_base)?,
        };
        let data_dir = match &data_override {
            Some(path) => path.clone(),
            None => default_root(environment, AppPathRoot::Data, &environment.data_base)?,
        };
        let local_data_dir = match (&local_data_override, &data_override) {
            (Some(path), _) | (None, Some(path)) => path.clone(),
            (None, None) => default_root(
                environment,
                AppPathRoot::LocalData,
                &environment.local_data_base,
            )?,
        };
        let state_dir = match (&state_override, &local_data_override, &data_override) {
            (Some(path), _, _) | (None, Some(path), _) | (None, None, Some(path)) => path.clone(),
            (None, None, None) => match platform {
                PathPlatform::Linux => {
                    default_root(environment, AppPathRoot::State, &environment.state_base)?
                }
                PathPlatform::MacOs | PathPlatform::Windows => {
                    join_component(platform, &local_data_dir, "state")
                }
            },
        };
        let cache_dir = match resolve_override(
            environment,
            "ZS_CACHE_DIR",
            environment.overrides.cache_dir.as_deref(),
        )? {
            Some(path) => path,
            None => {
                let base = required_base(
                    platform,
                    AppPathRoot::Cache,
                    environment.cache_base.as_deref(),
                )?;
                let application = join_component(platform, base, APP_COMPONENT);
                match platform {
                    PathPlatform::Windows => join_component(platform, &application, "cache"),
                    PathPlatform::Linux | PathPlatform::MacOs => application,
                }
            }
        };
        let credentials_dir = match resolve_override(
            environment,
            "ZS_CREDENTIALS_DIR",
            environment.overrides.credentials_dir.as_deref(),
        )? {
            Some(path) => path,
            None => join_component(platform, &local_data_dir, "credentials"),
        };
        let project_dir = environment
            .workspace_root
            .as_deref()
            .map(|root| {
                ensure_absolute(platform, AppPathRoot::Workspace, root)?;
                Ok(join_component(platform, root, ".zerostack"))
            })
            .transpose()?;

        Ok(Self {
            config_dir,
            data_dir,
            local_data_dir,
            state_dir,
            cache_dir,
            credentials_dir,
            project_dir,
        })
    }
}

fn default_root(
    environment: &PathEnvironment,
    root: AppPathRoot,
    base: &Option<PathBuf>,
) -> Result<PathBuf, AppPathError> {
    let base = required_base(environment.platform, root, base.as_deref())?;
    Ok(join_component(environment.platform, base, APP_COMPONENT))
}

fn required_base(
    platform: PathPlatform,
    root: AppPathRoot,
    base: Option<&Path>,
) -> Result<&Path, AppPathError> {
    let base = base.ok_or(AppPathError::MissingBase { root, platform })?;
    ensure_absolute(platform, root, base)?;
    Ok(base)
}

fn resolve_override(
    environment: &PathEnvironment,
    variable: &'static str,
    value: Option<&OsStr>,
) -> Result<Option<PathBuf>, AppPathError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(AppPathError::EmptyOverride { variable });
    }

    let path = expand_tilde(
        environment.platform,
        environment.home_dir.as_deref(),
        variable,
        value,
    )?;
    if !is_absolute(environment.platform, &path) {
        return Err(AppPathError::RelativeOverride {
            variable,
            value: path,
        });
    }
    Ok(Some(path))
}

fn expand_tilde(
    platform: PathPlatform,
    home_dir: Option<&Path>,
    variable: &'static str,
    value: &OsStr,
) -> Result<PathBuf, AppPathError> {
    let text = value.to_string_lossy();
    let suffix = if text == "~" {
        Some("")
    } else if let Some(suffix) = text.strip_prefix("~/") {
        Some(suffix)
    } else {
        text.strip_prefix("~\\")
    };
    let Some(suffix) = suffix else {
        return Ok(PathBuf::from(value));
    };
    let home_dir = home_dir.ok_or(AppPathError::MissingHomeForTilde { variable })?;
    if suffix.is_empty() {
        return Ok(home_dir.to_path_buf());
    }
    match platform {
        PathPlatform::Windows => Ok(join_component(
            platform,
            home_dir,
            &suffix.replace('/', "\\"),
        )),
        PathPlatform::Linux | PathPlatform::MacOs => Ok(join_component(platform, home_dir, suffix)),
    }
}

fn ensure_absolute(
    platform: PathPlatform,
    root: AppPathRoot,
    value: &Path,
) -> Result<(), AppPathError> {
    if is_absolute(platform, value) {
        Ok(())
    } else {
        Err(AppPathError::RelativeBase {
            root,
            value: value.to_path_buf(),
        })
    }
}

fn is_absolute(platform: PathPlatform, path: &Path) -> bool {
    match platform {
        PathPlatform::Linux | PathPlatform::MacOs => {
            path.as_os_str().to_string_lossy().starts_with('/')
        }
        PathPlatform::Windows => {
            let value = path.as_os_str().to_string_lossy();
            let value = value.as_bytes();
            let has_drive_root = value.len() >= 3
                && value[0].is_ascii_alphabetic()
                && value[1] == b':'
                && matches!(value[2], b'\\' | b'/');
            let has_unc_root =
                value.len() >= 2 && matches!(value[0], b'\\' | b'/') && value[1] == value[0];
            has_drive_root || has_unc_root
        }
    }
}

fn join_component(platform: PathPlatform, base: &Path, component: &str) -> PathBuf {
    let separator = match platform {
        PathPlatform::Linux | PathPlatform::MacOs => "/",
        PathPlatform::Windows => "\\",
    };
    let mut value = base.as_os_str().to_os_string();
    let base_text = base.as_os_str().to_string_lossy();
    let has_separator = match platform {
        PathPlatform::Linux | PathPlatform::MacOs => base_text.ends_with('/'),
        PathPlatform::Windows => base_text.ends_with('/') || base_text.ends_with('\\'),
    };
    if !has_separator {
        value.push(separator);
    }
    value.push(component);
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_override(
        overrides: &mut PathOverrides,
        variable: &'static str,
        value: Option<OsString>,
    ) {
        match variable {
            "ZS_CONFIG_DIR" => overrides.config_dir = value,
            "ZS_DATA_DIR" => overrides.data_dir = value,
            "ZS_LOCAL_DATA_DIR" => overrides.local_data_dir = value,
            "ZS_STATE_DIR" => overrides.state_dir = value,
            "ZS_CACHE_DIR" => overrides.cache_dir = value,
            "ZS_CREDENTIALS_DIR" => overrides.credentials_dir = value,
            _ => unreachable!("unknown path override"),
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

    #[test]
    fn app_paths_matrix_linux_defaults_and_project_root() {
        let paths = AppPaths::resolve(&linux_environment()).unwrap();

        assert_eq!(
            paths.config_dir,
            PathBuf::from("/home/alice/.config/zerostack")
        );
        assert_eq!(
            paths.data_dir,
            PathBuf::from("/home/alice/.local/share/zerostack")
        );
        assert_eq!(paths.local_data_dir, paths.data_dir);
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/home/alice/.local/state/zerostack")
        );
        assert_eq!(
            paths.cache_dir,
            PathBuf::from("/home/alice/.cache/zerostack")
        );
        assert_eq!(
            paths.credentials_dir,
            PathBuf::from("/home/alice/.local/share/zerostack/credentials")
        );
        assert_eq!(
            paths.project_dir,
            Some(PathBuf::from("/work/project/.zerostack"))
        );
    }

    #[test]
    fn app_paths_matrix_macos_defaults() {
        let environment = PathEnvironment {
            platform: PathPlatform::MacOs,
            home_dir: Some(PathBuf::from("/Users/alice")),
            config_base: Some(PathBuf::from("/Users/alice/Library/Application Support")),
            data_base: Some(PathBuf::from("/Users/alice/Library/Application Support")),
            local_data_base: Some(PathBuf::from("/Users/alice/Library/Application Support")),
            state_base: None,
            cache_base: Some(PathBuf::from("/Users/alice/Library/Caches")),
            workspace_root: None,
            overrides: PathOverrides::default(),
        };

        let paths = AppPaths::resolve(&environment).unwrap();
        let application_support =
            PathBuf::from("/Users/alice/Library/Application Support/zerostack");
        assert_eq!(paths.config_dir, application_support);
        assert_eq!(paths.data_dir, application_support);
        assert_eq!(paths.local_data_dir, application_support);
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/Users/alice/Library/Application Support/zerostack/state")
        );
        assert_eq!(
            paths.cache_dir,
            PathBuf::from("/Users/alice/Library/Caches/zerostack")
        );
        assert_eq!(
            paths.credentials_dir,
            PathBuf::from("/Users/alice/Library/Application Support/zerostack/credentials")
        );
        assert_eq!(paths.project_dir, None);
    }

    #[test]
    fn app_paths_matrix_windows_defaults_drive_unc_and_long_overrides() {
        let mut environment = PathEnvironment {
            platform: PathPlatform::Windows,
            home_dir: Some(PathBuf::from(r"C:\Users\Alice")),
            config_base: Some(PathBuf::from(r"C:\Users\Alice\AppData\Roaming")),
            data_base: Some(PathBuf::from(r"C:\Users\Alice\AppData\Roaming")),
            local_data_base: Some(PathBuf::from(r"C:\Users\Alice\AppData\Local")),
            state_base: None,
            cache_base: Some(PathBuf::from(r"C:\Users\Alice\AppData\Local")),
            workspace_root: Some(PathBuf::from(r"C:\work\project")),
            overrides: PathOverrides::default(),
        };

        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(
            paths.config_dir,
            PathBuf::from(r"C:\Users\Alice\AppData\Roaming\zerostack")
        );
        assert_eq!(paths.config_dir, paths.data_dir);
        assert_eq!(
            paths.local_data_dir,
            PathBuf::from(r"C:\Users\Alice\AppData\Local\zerostack")
        );
        assert_eq!(
            paths.state_dir,
            PathBuf::from(r"C:\Users\Alice\AppData\Local\zerostack\state")
        );
        assert_eq!(
            paths.cache_dir,
            PathBuf::from(r"C:\Users\Alice\AppData\Local\zerostack\cache")
        );
        assert_eq!(
            paths.credentials_dir,
            PathBuf::from(r"C:\Users\Alice\AppData\Local\zerostack\credentials")
        );
        assert_eq!(
            paths.project_dir,
            Some(PathBuf::from(r"C:\work\project\.zerostack"))
        );

        environment.overrides.data_dir = Some(OsString::from(r"\\server\share\portable"));
        environment.overrides.cache_dir = Some(OsString::from(format!(
            r"C:\{}",
            "long-segment\\".repeat(30)
        )));
        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(paths.data_dir, PathBuf::from(r"\\server\share\portable"));
        assert_eq!(paths.local_data_dir, paths.data_dir);
        assert_eq!(paths.state_dir, paths.data_dir);
        assert!(paths.cache_dir.to_string_lossy().len() > 260);

        environment.overrides.config_dir = Some(OsString::from("~/nested/config"));
        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(
            paths.config_dir,
            PathBuf::from(r"C:\Users\Alice\nested\config")
        );
    }

    #[test]
    fn app_paths_matrix_override_precedence_and_tilde_expansion() {
        let mut environment = linux_environment();
        environment.overrides = PathOverrides {
            config_dir: Some(OsString::from("~/config")),
            data_dir: Some(OsString::from("/legacy")),
            local_data_dir: Some(OsString::from("/local")),
            state_dir: Some(OsString::from("/state")),
            cache_dir: Some(OsString::from("/cache")),
            credentials_dir: Some(OsString::from("/secrets")),
        };

        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(paths.config_dir, PathBuf::from("/home/alice/config"));
        assert_eq!(paths.data_dir, PathBuf::from("/legacy"));
        assert_eq!(paths.local_data_dir, PathBuf::from("/local"));
        assert_eq!(paths.state_dir, PathBuf::from("/state"));
        assert_eq!(paths.cache_dir, PathBuf::from("/cache"));
        assert_eq!(paths.credentials_dir, PathBuf::from("/secrets"));

        environment.overrides.state_dir = None;
        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(paths.state_dir, PathBuf::from("/local"));

        environment.overrides.local_data_dir = None;
        environment.overrides.credentials_dir = None;
        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(paths.local_data_dir, PathBuf::from("/legacy"));
        assert_eq!(paths.state_dir, PathBuf::from("/legacy"));
        assert_eq!(paths.credentials_dir, PathBuf::from("/legacy/credentials"));
        assert_eq!(
            paths.config_dir,
            PathBuf::from("/home/alice/config"),
            "ZS_DATA_DIR must not select the configuration root"
        );

        environment.overrides.config_dir = None;
        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(
            paths.config_dir,
            PathBuf::from("/home/alice/.config/zerostack"),
            "ZS_DATA_DIR must fall through to the platform configuration base"
        );
    }

    #[test]
    fn app_paths_matrix_rejects_invalid_overrides_and_missing_bases() {
        const VARIABLES: [&str; 6] = [
            "ZS_CONFIG_DIR",
            "ZS_DATA_DIR",
            "ZS_LOCAL_DATA_DIR",
            "ZS_STATE_DIR",
            "ZS_CACHE_DIR",
            "ZS_CREDENTIALS_DIR",
        ];
        for variable in VARIABLES {
            let mut environment = linux_environment();
            set_override(&mut environment.overrides, variable, Some(OsString::new()));
            assert_eq!(
                AppPaths::resolve(&environment),
                Err(AppPathError::EmptyOverride { variable })
            );

            let mut environment = linux_environment();
            set_override(
                &mut environment.overrides,
                variable,
                Some(OsString::from("relative/path")),
            );
            assert_eq!(
                AppPaths::resolve(&environment),
                Err(AppPathError::RelativeOverride {
                    variable,
                    value: PathBuf::from("relative/path"),
                })
            );

            let mut environment = linux_environment();
            environment.home_dir = None;
            set_override(
                &mut environment.overrides,
                variable,
                Some(OsString::from("~/path")),
            );
            assert_eq!(
                AppPaths::resolve(&environment),
                Err(AppPathError::MissingHomeForTilde { variable })
            );
        }

        let mut environment = linux_environment();
        environment.config_base = None;
        assert_eq!(
            AppPaths::resolve(&environment),
            Err(AppPathError::MissingBase {
                root: AppPathRoot::Config,
                platform: PathPlatform::Linux,
            })
        );

        environment = linux_environment();
        environment.data_base = None;
        assert_eq!(
            AppPaths::resolve(&environment),
            Err(AppPathError::MissingBase {
                root: AppPathRoot::Data,
                platform: PathPlatform::Linux,
            })
        );

        environment = linux_environment();
        environment.local_data_base = None;
        assert_eq!(
            AppPaths::resolve(&environment),
            Err(AppPathError::MissingBase {
                root: AppPathRoot::LocalData,
                platform: PathPlatform::Linux,
            })
        );

        environment = linux_environment();
        environment.state_base = None;
        assert_eq!(
            AppPaths::resolve(&environment),
            Err(AppPathError::MissingBase {
                root: AppPathRoot::State,
                platform: PathPlatform::Linux,
            })
        );

        environment = linux_environment();
        environment.cache_base = None;
        assert_eq!(
            AppPaths::resolve(&environment),
            Err(AppPathError::MissingBase {
                root: AppPathRoot::Cache,
                platform: PathPlatform::Linux,
            })
        );

        environment = linux_environment();
        environment.cache_base = Some(PathBuf::from("relative/cache"));
        assert_eq!(
            AppPaths::resolve(&environment),
            Err(AppPathError::RelativeBase {
                root: AppPathRoot::Cache,
                value: PathBuf::from("relative/cache"),
            })
        );
    }

    #[test]
    fn app_paths_matrix_routes_startup_config_to_resolved_root() {
        let root =
            std::env::temp_dir().join(format!("zerostack-app-paths-{}", uuid::Uuid::new_v4()));
        let environment = PathEnvironment {
            platform: PathPlatform::current().unwrap(),
            home_dir: Some(root.join("home")),
            config_base: Some(root.join("config-base")),
            data_base: Some(root.join("data-base")),
            local_data_base: Some(root.join("local-data-base")),
            state_base: Some(root.join("state-base")),
            cache_base: Some(root.join("cache-base")),
            workspace_root: Some(root.join("workspace")),
            overrides: PathOverrides {
                config_dir: Some(root.as_os_str().to_os_string()),
                ..PathOverrides::default()
            },
        };
        let paths = AppPaths::resolve(&environment).unwrap();

        let (_, is_first_startup) = crate::config::load_with_paths(&paths);

        assert!(is_first_startup);
        assert!(root.join("config.toml").is_file());
        assert!(!root.join("workspace/.zerostack/config.toml").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
