//! ACP `authMethods`, `authenticate` and the `auth_required` gate
//! (mini-agent-nvnkx.109).
//!
//! `initialize` advertises how a user can sign in:
//!
//! * `credentials` (an agent method): the provider credential comes from the
//!   environment or config; `authenticate` re-checks it, so a client can
//!   offer a "retry" after the user set it;
//! * one terminal method per login-capable provider (`login-openai`,
//!   `login-copilot`), only for a client that advertised
//!   `auth.terminal`: the client runs `mini-agent <args>` in its own
//!   terminal, where the existing device/PKCE login flow can print its URL
//!   and read the browser redirect. The flows write to stdout, which is the
//!   protocol channel here, so they are never run inside the server.
//!
//! A client that advertised `auth.terminal` can show a sign-in UI, so only it
//! gets `auth_required` from `session/new` when the default provider has no
//! credential; every other client keeps the failure at the first prompt.

use agent_client_protocol::schema::v1::{AuthMethod, AuthMethodAgent, AuthMethodTerminal};

use crate::cli::Cli;
use crate::config::Config;

pub(super) const CREDENTIALS_METHOD: &str = "credentials";
/// Providers `--login` can sign in to.
const LOGIN_PROVIDERS: [&str; 2] = ["openai", "copilot"];

fn login_id(provider: &str) -> String {
    format!("login-{provider}")
}

/// The methods `initialize` lists. Terminal methods need the client's
/// `auth.terminal` capability.
pub(super) fn auth_methods(terminal: bool) -> Vec<AuthMethod> {
    let mut methods = vec![AuthMethod::Agent(
        AuthMethodAgent::new(CREDENTIALS_METHOD, "Provider credentials").description(
            "Set the provider API key in the environment or [api_keys] of the config, then retry; mini-agent checks it again",
        ),
    )];
    if terminal {
        for provider in LOGIN_PROVIDERS {
            let name = if provider == "openai" {
                "Sign in with ChatGPT"
            } else {
                "Sign in with GitHub Copilot"
            };
            methods.push(AuthMethod::Terminal(
                AuthMethodTerminal::new(login_id(provider), name)
                    .description(format!(
                        "Runs `mini-agent --login {provider}` in a terminal"
                    ))
                    .args(vec!["--login".to_owned(), provider.to_owned()]),
            ));
        }
    }
    methods
}

/// Whether `method` is one `initialize` can list.
pub(super) fn is_known_method(method: &str) -> bool {
    method == CREDENTIALS_METHOD || LOGIN_PROVIDERS.iter().any(|p| login_id(p) == method)
}

/// `Ok` when the default provider's credential resolves now; otherwise the
/// bounded reason, which never carries a credential value.
pub(super) fn check_default_credential(cli: &Cli, cfg: &Config) -> Result<(), String> {
    let (provider, _) = super::acp_provider_and_model(cli, cfg);
    crate::provider::check_client_credentials(
        &provider,
        cli.api_key_for(cfg, &provider),
        cfg.custom_providers_map(),
        cfg.api_keys.as_ref(),
    )
    .map_err(|error| super::persona::bounded_message(&error.to_string()))
}

/// The `auth_required` error with the reason and the sign-in options.
pub(super) fn auth_required(reason: &str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::auth_required().data(serde_json::json!({
        "reason": reason,
        "methods": auth_methods(true)
            .iter()
            .map(|method| method.id().0.to_string())
            .collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_methods_need_the_client_capability_and_name_the_command() {
        let plain = auth_methods(false);
        assert_eq!(plain.len(), 1);
        assert!(matches!(plain[0], AuthMethod::Agent(_)));
        let with_terminal = auth_methods(true);
        let terminals: Vec<_> = with_terminal
            .iter()
            .filter_map(|method| match method {
                AuthMethod::Terminal(terminal) => Some(terminal),
                _ => None,
            })
            .collect();
        assert_eq!(terminals.len(), LOGIN_PROVIDERS.len());
        let openai = terminals
            .iter()
            .find(|method| &*method.id.0 == "login-openai")
            .unwrap();
        assert_eq!(openai.args, ["--login", "openai"]);
        assert!(
            openai
                .description
                .as_deref()
                .unwrap()
                .contains("mini-agent --login openai")
        );
    }

    #[test]
    fn only_listed_methods_are_known() {
        assert!(is_known_method("credentials"));
        assert!(is_known_method("login-copilot"));
        assert!(!is_known_method("login-anthropic"));
        assert!(!is_known_method(""));
    }

    #[test]
    fn auth_required_lists_the_method_ids() {
        let error = auth_required("no key");
        let data = error.data.unwrap();
        assert_eq!(data["reason"], "no key");
        assert!(
            data["methods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == "login-openai")
        );
    }
}
