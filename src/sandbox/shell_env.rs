//! `shell-inherit-env` / `shell-env`: the opt-in environment layered over the
//! fixed shell allowlist (`essential_env`).
//!
//! Every shell, background, pty, `!` and validation command starts from an
//! empty environment plus a short allowlist. This policy lets the operator add
//! named variables inherited from the parent process (`shell-inherit-env`) and
//! literal values (`shell-env`). It is default closed, and variables whose
//! names look like credentials stay refused unless the global-only
//! `shell-env-allow-secrets` is set. Loader-injection variables are always
//! refused.

use std::collections::BTreeMap;

const MAX_ENTRIES: usize = 64;
const MAX_VALUE_BYTES: usize = 8 * 1024;

/// Name fragments that mark a variable as a credential.
const SECRET_FRAGMENTS: &[&str] = &[
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "TOKEN",
    "CREDENTIAL",
    "API_KEY",
    "APIKEY",
    "PRIVATE_KEY",
    "ACCESS_KEY",
];

/// The effective shell environment policy. `Default` is closed (no additions).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellEnvPolicy {
    /// Names (or `*` globs) copied from the parent process when set there.
    pub inherit: Vec<String>,
    /// Literal values; they win over inherited ones and the allowlist.
    pub set: BTreeMap<String, String>,
    /// Permit credential-looking names (global config only).
    pub allow_secrets: bool,
}

/// Whether `name` looks like a credential (`*_API_KEY`, `GH_TOKEN`, ...).
pub fn looks_like_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.ends_with("_KEY")
        || upper.ends_with("_PAT")
        || SECRET_FRAGMENTS.iter().any(|part| upper.contains(part))
}

/// Variables that make a shell or the dynamic loader run attacker-chosen code
/// before the command, which no setting may add.
pub fn is_injection_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("LD_")
        || upper.starts_with("DYLD_")
        || upper.starts_with("BASH_FUNC_")
        || matches!(
            upper.as_str(),
            "BASH_ENV" | "ENV" | "PROMPT_COMMAND" | "SHELLOPTS" | "BASHOPTS" | "PS4" | "IFS"
        )
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['=', '\0']) && !name.chars().any(char::is_whitespace)
}

fn is_glob(pattern: &str) -> bool {
    pattern.contains('*')
}

/// `*`-only glob match over the whole name.
fn glob_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let [first, rest @ ..] = parts.as_slice() else {
        return false;
    };
    let Some(mut remaining) = name.strip_prefix(first) else {
        return false;
    };
    let Some((last, middle)) = rest.split_last() else {
        return remaining.is_empty();
    };
    for part in middle {
        match remaining.find(part) {
            Some(at) => remaining = &remaining[at + part.len()..],
            None => return false,
        }
    }
    remaining.ends_with(last)
}

impl ShellEnvPolicy {
    pub fn is_empty(&self) -> bool {
        self.inherit.is_empty() && self.set.is_empty()
    }

    /// Reject malformed entries, injection names and (unless allowed)
    /// credential-looking exact names. Globs are filtered at resolution
    /// instead, so `NVM_*` never drags a token along.
    pub fn validate(&self) -> Result<(), String> {
        if self.inherit.len() > MAX_ENTRIES || self.set.len() > MAX_ENTRIES {
            return Err(format!(
                "shell-inherit-env and shell-env accept at most {MAX_ENTRIES} entries each"
            ));
        }
        let names = self
            .inherit
            .iter()
            .map(|name| (name, "shell-inherit-env"))
            .chain(self.set.keys().map(|name| (name, "shell-env")));
        for (name, key) in names {
            if !valid_name(name) {
                return Err(format!("{key}: invalid variable name {name:?}"));
            }
            if is_injection_name(name) {
                return Err(format!(
                    "{key}: {name} is refused because it loads or runs code before the command"
                ));
            }
            if !is_glob(name) && !self.allow_secrets && looks_like_secret(name) {
                return Err(format!(
                    "{key}: {name} looks like a credential and is refused; set the global-only shell-env-allow-secrets = true to pass it"
                ));
            }
        }
        if let Some(value) = self
            .set
            .iter()
            .find(|(_, value)| value.len() > MAX_VALUE_BYTES || value.contains('\0'))
        {
            return Err(format!(
                "shell-env: the value of {} is too long or contains NUL",
                value.0
            ));
        }
        Ok(())
    }

    /// The variables to add, resolved against `parent` (the process
    /// environment). Credential-looking and injection names never come from a
    /// glob, and a literal wins over an inherited value.
    pub fn resolve_from<I>(&self, parent: I) -> Vec<(String, String)>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        if self.is_empty() {
            return Vec::new();
        }
        let parent: BTreeMap<String, String> = parent.into_iter().collect();
        let permitted = |name: &str| {
            !is_injection_name(name) && (self.allow_secrets || !looks_like_secret(name))
        };
        let mut out = BTreeMap::new();
        for pattern in &self.inherit {
            if is_glob(pattern) {
                for (name, value) in &parent {
                    if glob_match(pattern, name) && permitted(name) {
                        out.insert(name.clone(), value.clone());
                    }
                }
            } else if let Some(value) = parent.get(pattern)
                && permitted(pattern)
            {
                out.insert(pattern.clone(), value.clone());
            }
        }
        for (name, value) in &self.set {
            if permitted(name) {
                out.insert(name.clone(), value.clone());
            }
        }
        out.into_iter().collect()
    }

    /// [`Self::resolve_from`] over the live process environment.
    pub fn resolve(&self) -> Vec<(String, String)> {
        if self.is_empty() {
            return Vec::new();
        }
        self.resolve_from(std::env::vars_os().filter_map(|(name, value)| {
            Some((name.into_string().ok()?, value.into_string().ok()?))
        }))
    }

    /// One-line summary for `/sandbox`. Names only, never values.
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return "allowlist only".to_string();
        }
        let mut names: Vec<String> = self.inherit.clone();
        names.extend(self.set.keys().map(|name| format!("{name}=")));
        format!(
            "allowlist + {}{}",
            names.join(", "),
            if self.allow_secrets {
                " (secrets allowed)"
            } else {
                ""
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn policy(inherit: &[&str], set: &[(&str, &str)]) -> ShellEnvPolicy {
        ShellEnvPolicy {
            inherit: inherit.iter().map(|s| s.to_string()).collect(),
            set: set
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            allow_secrets: false,
        }
    }

    #[test]
    fn default_policy_is_closed() {
        assert!(
            ShellEnvPolicy::default()
                .resolve_from(env(&[("JAVA_HOME", "/jdk")]))
                .is_empty()
        );
        assert_eq!(ShellEnvPolicy::default().summary(), "allowlist only");
    }

    #[test]
    fn inherits_named_and_globbed_variables_and_literals_win() {
        let p = policy(
            &["JAVA_HOME", "NVM_*", "MISSING"],
            &[("JAVA_HOME", "/override"), ("X", "1")],
        );
        let got = p.resolve_from(env(&[
            ("JAVA_HOME", "/jdk"),
            ("NVM_DIR", "/nvm"),
            ("NVM_BIN", "/nvm/bin"),
            ("OTHER", "no"),
        ]));
        assert_eq!(
            got,
            env(&[
                ("JAVA_HOME", "/override"),
                ("NVM_BIN", "/nvm/bin"),
                ("NVM_DIR", "/nvm"),
                ("X", "1")
            ])
        );
    }

    #[test]
    fn credential_names_are_refused_by_default_and_never_come_from_globs() {
        for name in [
            "OPENAI_API_KEY",
            "GH_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "DB_PASSWORD",
            "SSH_PRIVATE_KEY",
            "NPM_CONFIG_TOKEN",
            "MY_PAT",
        ] {
            assert!(looks_like_secret(name), "{name}");
            assert!(policy(&[name], &[]).validate().is_err(), "{name}");
            assert!(policy(&[], &[(name, "v")]).validate().is_err(), "{name}");
        }
        assert!(!looks_like_secret("HTTPS_PROXY") && !looks_like_secret("SSL_CERT_FILE"));
        let globbed = policy(&["*"], &[]).resolve_from(env(&[
            ("GH_TOKEN", "t"),
            ("HTTPS_PROXY", "p"),
            ("LD_PRELOAD", "x"),
            ("OPENAI_API_KEY", "k"),
        ]));
        assert_eq!(globbed, env(&[("HTTPS_PROXY", "p")]));
        let mut open = policy(&["GH_TOKEN"], &[]);
        open.allow_secrets = true;
        open.validate().unwrap();
        assert_eq!(
            open.resolve_from(env(&[("GH_TOKEN", "t")])),
            env(&[("GH_TOKEN", "t")])
        );
        assert!(open.summary().contains("secrets allowed"));
    }

    #[test]
    fn injection_names_and_malformed_entries_are_always_refused() {
        for name in [
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "BASH_ENV",
            "ENV",
            "PROMPT_COMMAND",
            "BASH_FUNC_x%%",
            "LD_*",
        ] {
            let mut p = policy(&[name], &[(name, "v")]);
            p.allow_secrets = true;
            assert!(p.validate().is_err(), "{name}");
        }
        for bad in ["", "A=B", "A B", "A\0"] {
            assert!(policy(&[bad], &[]).validate().is_err(), "{bad:?}");
        }
        assert!(policy(&[], &[("A", "x\0y")]).validate().is_err());
        assert!(
            policy(&[], &[("A", &"x".repeat(MAX_VALUE_BYTES + 1))])
                .validate()
                .is_err()
        );
        let many: Vec<String> = (0..=MAX_ENTRIES).map(|i| format!("V{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(policy(&refs, &[]).validate().is_err());
    }

    #[test]
    fn glob_matching_is_anchored() {
        assert!(glob_match("NVM_*", "NVM_DIR"));
        assert!(!glob_match("NVM_*", "XNVM_DIR"));
        assert!(glob_match("*_PROXY", "HTTPS_PROXY"));
        assert!(!glob_match("*_PROXY", "HTTPS_PROXY_X"));
        assert!(glob_match("A*B*C", "A1B2C"));
        assert!(!glob_match("A*B*C", "A1C2B"));
        assert!(glob_match("EXACT", "EXACT") && !glob_match("EXACT", "EXACTx"));
    }
}
