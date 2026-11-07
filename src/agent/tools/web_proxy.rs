//! Environment proxy support for the native web tools and the JS `fetch()`
//! broker (mini-agent-788z5).
//!
//! `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` (and lowercase forms) are honoured
//! with `NO_PROXY` exclusions, like the provider and MCP clients. Loopback and
//! `localhost` destinations never use a proxy.
//!
//! Trade-off: a proxied request is resolved by the proxy, so local DNS
//! resolution and address pinning are impossible for DNS names. Those hosts are
//! instead screened by name ([`validate_proxied_host_name`]: no `localhost`,
//! no single-label or well-known internal suffixes), and IP literals still go
//! through the normal public-address check. The proxy is then the boundary for
//! what the resolved name may reach; origin policy, approval, redirect
//! re-validation and body/time bounds are unchanged.

use super::web_http::FetchError;
use reqwest::Url;
use std::net::IpAddr;

/// A proxy variable's outcome: a usable URL, or the name of a variable that
/// is set but unusable (unsupported scheme such as `socks5://`, or unparsable).
/// An unusable value must fail closed, never fall back to a direct connection.
type ProxySlot = Result<String, String>;

/// Proxy settings read from the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProxyEnv {
    http: Option<ProxySlot>,
    https: Option<ProxySlot>,
    all: Option<ProxySlot>,
    no_proxy: Vec<String>,
}

/// `None` when unset/empty; `Some(Err(name))` when set but unusable.
fn clean_proxy(name: &str, value: Option<String>) -> Option<ProxySlot> {
    let value = value?.trim().to_owned();
    if value.is_empty() {
        return None;
    }
    let candidate = if value.contains("://") {
        value
    } else {
        format!("http://{value}")
    };
    // Only schemes the HTTP client is built to speak.
    let usable = Url::parse(&candidate)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some());
    Some(if usable {
        Ok(candidate)
    } else {
        Err(name.to_owned())
    })
}

fn unusable_proxy(variable: &str) -> FetchError {
    FetchError::ProxyUnusable(variable.to_owned())
}

impl ProxyEnv {
    pub(crate) fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let first = |names: &[&str]| names.iter().find_map(|name| clean_proxy(name, get(name)));
        let no_proxy = ["NO_PROXY", "no_proxy"]
            .iter()
            .find_map(|name| get(name).filter(|v| !v.trim().is_empty()))
            .map(|list| {
                list.split(',')
                    .map(|entry| entry.trim().to_ascii_lowercase())
                    .filter(|entry| !entry.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            http: first(&["http_proxy", "HTTP_PROXY"]),
            https: first(&["https_proxy", "HTTPS_PROXY"]),
            all: first(&["all_proxy", "ALL_PROXY"]),
            no_proxy,
        }
    }

    /// The proxy URL `url` must be sent through, if any. `Err` when the
    /// variable that applies is set but unusable: the request must not go direct.
    pub(crate) fn proxy_for(&self, url: &Url) -> Result<Option<String>, FetchError> {
        let Some(host) = url.host_str() else {
            return Ok(None);
        };
        let host = normalize_host(host);
        if is_loopback_name(&host) || self.excluded(&host, url.port_or_known_default()) {
            return Ok(None);
        }
        let specific = match url.scheme() {
            "https" => &self.https,
            "http" => &self.http,
            _ => return Ok(None),
        };
        match specific.as_ref().or(self.all.as_ref()) {
            None => Ok(None),
            Some(Ok(proxy)) => Ok(Some(proxy.clone())),
            Some(Err(variable)) => Err(unusable_proxy(variable)),
        }
    }

    fn excluded(&self, host: &str, port: Option<u16>) -> bool {
        let ip = host.parse::<IpAddr>().ok();
        self.no_proxy.iter().any(|entry| {
            if entry == "*" {
                return true;
            }
            if let Some((network, bits)) = entry.split_once('/')
                && let (Some(IpAddr::V4(host)), Ok(IpAddr::V4(network)), Ok(bits)) =
                    (ip, network.parse::<IpAddr>(), bits.parse::<u8>())
                && bits <= 32
            {
                return super::web_http::ipv4_in_prefix(u32::from(host), u32::from(network), bits);
            }
            // `host:port` entries only match that port; bare IPv6 keeps colons.
            let (name, entry_port) = match entry.rsplit_once(':') {
                Some((name, p)) if !name.contains(':') => match p.parse::<u16>() {
                    Ok(p) => (name, Some(p)),
                    Err(_) => (entry.as_str(), None),
                },
                _ => (entry.as_str(), None),
            };
            if entry_port.is_some() && entry_port != port {
                return false;
            }
            let name = name
                .trim_start_matches("*.")
                .trim_start_matches('.')
                .trim_end_matches('.');
            host == name || host.ends_with(&format!(".{name}"))
        })
    }
}

/// Lowercase, strip IPv6 brackets and the root-label trailing dot(s), so
/// `localhost.` and `printer.local.` compare like their dotless spellings.
fn normalize_host(host: &str) -> String {
    host.trim_matches(['[', ']'])
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_owned()
}

fn is_loopback_name(host: &str) -> bool {
    let host = normalize_host(host);
    let host = host.as_str();
    host == "localhost"
        || host.ends_with(".localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
thread_local! {
    static TEST_ENV: std::cell::RefCell<Option<ProxyEnv>> = const { std::cell::RefCell::new(None) };
}

/// Makes `env` stand in for the process environment on this thread until the
/// guard drops (tests only; production reads never see it, and tests otherwise
/// see no proxy). Use on a current-thread runtime.
#[cfg(test)]
pub(crate) fn set_test_proxy_env(env: ProxyEnv) -> TestProxyGuard {
    TEST_ENV.with(|cell| *cell.borrow_mut() = Some(env));
    TestProxyGuard
}

#[cfg(test)]
pub(crate) struct TestProxyGuard;

#[cfg(test)]
impl Drop for TestProxyGuard {
    fn drop(&mut self) {
        TEST_ENV.with(|cell| *cell.borrow_mut() = None);
    }
}

fn current_env() -> ProxyEnv {
    #[cfg(test)]
    {
        TEST_ENV.with(|cell| cell.borrow().clone().unwrap_or_default())
    }
    #[cfg(not(test))]
    {
        ProxyEnv::from_lookup(|name| std::env::var(name).ok())
    }
}

/// The environment proxy for `url`, if one applies.
pub(crate) fn proxy_for_url(url: &Url) -> Result<Option<String>, FetchError> {
    current_env().proxy_for(url)
}

/// Whether `url` goes through a proxy that must resolve its (DNS) name, so no
/// local resolution or pinning is possible.
pub(crate) fn proxied_dns_name(url: &Url) -> Result<bool, FetchError> {
    Ok(proxy_for_url(url)?.is_some()
        && url
            .host_str()
            .is_some_and(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().is_err()))
}

/// Name-level screening for a host the proxy will resolve.
pub(crate) fn validate_proxied_host_name(host: &str) -> Result<(), FetchError> {
    let host = normalize_host(host);
    let internal_suffix = [
        ".localhost",
        ".local",
        ".localdomain",
        ".internal",
        ".lan",
        ".home.arpa",
        ".intranet",
    ]
    .iter()
    .any(|suffix| host.ends_with(suffix));
    if host == "localhost" || internal_suffix || !host.contains('.') {
        return Err(FetchError::DestinationDenied);
    }
    Ok(())
}

/// Proxy for a pinned HTTP client builder; a bad value yields a redacted error.
pub(crate) fn proxy_config(proxy: &str) -> Result<reqwest::Proxy, FetchError> {
    reqwest::Proxy::all(proxy)
        .map_err(|_| FetchError::ClientBuild("invalid HTTP(S)_PROXY value".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> ProxyEnv {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        ProxyEnv::from_lookup(|name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        })
    }

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn scheme_specific_then_all_proxy_and_none_without_env() {
        let e = env(&[
            ("HTTPS_PROXY", "proxy.corp:3128"),
            ("ALL_PROXY", "http://all.corp:1"),
        ]);
        assert_eq!(
            e.proxy_for(&url("https://example.com/"))
                .unwrap()
                .as_deref(),
            Some("http://proxy.corp:3128")
        );
        assert_eq!(
            e.proxy_for(&url("http://example.com/")).unwrap().as_deref(),
            Some("http://all.corp:1")
        );
        assert_eq!(env(&[]).proxy_for(&url("https://example.com/")), Ok(None));
        // Empty values are unset.
        let blank = env(&[("HTTP_PROXY", "  ")]);
        assert_eq!(blank.proxy_for(&url("http://example.com/")), Ok(None));
    }

    #[test]
    fn no_proxy_and_loopback_bypass() {
        let e = env(&[
            ("HTTPS_PROXY", "http://p:1"),
            (
                "NO_PROXY",
                "internal.corp, .skip.test,10.0.0.0/8,host.test:8443",
            ),
        ]);
        assert!(e.proxy_for(&url("https://example.com/")).unwrap().is_some());
        assert!(
            e.proxy_for(&url("https://internal.corp/"))
                .unwrap()
                .is_none()
        );
        assert!(
            e.proxy_for(&url("https://a.internal.corp/"))
                .unwrap()
                .is_none()
        );
        assert!(e.proxy_for(&url("https://x.skip.test/")).unwrap().is_none());
        assert!(e.proxy_for(&url("https://10.1.2.3/")).unwrap().is_none());
        assert!(
            e.proxy_for(&url("https://host.test:8443/"))
                .unwrap()
                .is_none()
        );
        assert!(e.proxy_for(&url("https://host.test/")).unwrap().is_some());
        assert!(
            e.proxy_for(&url("https://notinternal.corp/"))
                .unwrap()
                .is_some()
        );
        // Loopback never proxies, even with no NO_PROXY.
        let e = env(&[("HTTPS_PROXY", "http://p:1")]);
        assert!(e.proxy_for(&url("https://localhost/")).unwrap().is_none());
        assert!(e.proxy_for(&url("https://127.0.0.1/")).unwrap().is_none());
        assert!(e.proxy_for(&url("https://[::1]/")).unwrap().is_none());
        let all = env(&[("HTTPS_PROXY", "http://p:1"), ("NO_PROXY", "*")]);
        assert!(
            all.proxy_for(&url("https://example.com/"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn proxied_names_are_screened_for_local_and_internal_targets() {
        for denied in [
            "localhost",
            "app.localhost",
            "printer.local",
            "db.internal",
            "router.lan",
            "intranet",
            "nas.home.arpa",
            "localhost.",
            "a.localhost.",
            "printer.local.",
            "db.internal.",
            "intranet.",
            "nas.home.arpa.",
            "LOCALHOST.",
            "localhost..",
        ] {
            assert_eq!(
                validate_proxied_host_name(denied),
                Err(FetchError::DestinationDenied),
                "{denied}"
            );
        }
        assert!(validate_proxied_host_name("docs.rs").is_ok());
    }

    #[test]
    fn trailing_dot_loopback_never_uses_the_proxy() {
        let e = env(&[("HTTPS_PROXY", "http://p:1")]);
        assert_eq!(e.proxy_for(&url("https://localhost./")), Ok(None));
        assert_eq!(e.proxy_for(&url("https://a.localhost./")), Ok(None));
        assert_eq!(proxied_dns_name(&url("https://localhost./")), Ok(false));
    }

    #[test]
    fn unusable_proxy_values_fail_closed_naming_the_variable() {
        for (var, value) in [
            ("ALL_PROXY", "socks5://127.0.0.1:1080"),
            ("ALL_PROXY", "socks5h://h:1"),
            ("HTTPS_PROXY", "ftp://x"),
            ("HTTPS_PROXY", "http://"),
            ("HTTP_PROXY", "http://[::1"),
            ("https_proxy", "socks5://h:1"),
        ] {
            let e = env(&[(var, value)]);
            let https = e.proxy_for(&url("https://example.com/"));
            let http = e.proxy_for(&url("http://example.com/"));
            let applies = if var.eq_ignore_ascii_case("HTTP_PROXY") {
                http.clone()
            } else if var.eq_ignore_ascii_case("HTTPS_PROXY") {
                https.clone()
            } else {
                https.clone().and(http.clone())
            };
            let Err(FetchError::ProxyUnusable(message)) = applies else {
                panic!("{var}={value} must fail closed, got {applies:?}");
            };
            assert!(message.contains(var), "{message}");
            assert!(!message.contains("127.0.0.1"), "{message}");
        }
        // The unusable variable only matters where it would apply.
        let e = env(&[
            ("HTTP_PROXY", "socks5://h:1"),
            ("HTTPS_PROXY", "http://p:1"),
        ]);
        assert_eq!(
            e.proxy_for(&url("https://example.com/")),
            Ok(Some("http://p:1".to_owned()))
        );
        assert!(e.proxy_for(&url("http://example.com/")).is_err());
        // Bypasses still go direct without error.
        let e = env(&[("ALL_PROXY", "socks5://h:1"), ("NO_PROXY", "skip.test")]);
        assert_eq!(e.proxy_for(&url("https://skip.test/")), Ok(None));
        assert_eq!(e.proxy_for(&url("https://localhost/")), Ok(None));
    }
}
