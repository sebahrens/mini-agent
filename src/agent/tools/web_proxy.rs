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

/// Proxy settings read from the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProxyEnv {
    http: Option<String>,
    https: Option<String>,
    all: Option<String>,
    no_proxy: Vec<String>,
}

fn clean_proxy(value: Option<String>) -> Option<String> {
    let value = value?.trim().to_owned();
    if value.is_empty() {
        return None;
    }
    let candidate = if value.contains("://") {
        value
    } else {
        format!("http://{value}")
    };
    let url = Url::parse(&candidate).ok()?;
    // Only schemes the HTTP client is built to speak.
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some()).then_some(candidate)
}

impl ProxyEnv {
    pub(crate) fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let first = |names: &[&str]| names.iter().find_map(|name| clean_proxy(get(name)));
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

    /// The proxy URL `url` must be sent through, if any.
    pub(crate) fn proxy_for(&self, url: &Url) -> Option<String> {
        let host = url
            .host_str()?
            .trim_matches(['[', ']'])
            .to_ascii_lowercase();
        if is_loopback_name(&host) || self.excluded(&host, url.port_or_known_default()) {
            return None;
        }
        let specific = match url.scheme() {
            "https" => &self.https,
            "http" => &self.http,
            _ => return None,
        };
        specific.clone().or_else(|| self.all.clone())
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
            let name = name.trim_start_matches("*.").trim_start_matches('.');
            host == name || host.ends_with(&format!(".{name}"))
        })
    }
}

fn is_loopback_name(host: &str) -> bool {
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
pub(crate) fn proxy_for_url(url: &Url) -> Option<String> {
    current_env().proxy_for(url)
}

/// Whether `url` goes through a proxy that must resolve its (DNS) name, so no
/// local resolution or pinning is possible.
pub(crate) fn proxied_dns_name(url: &Url) -> bool {
    proxy_for_url(url).is_some()
        && url
            .host_str()
            .is_some_and(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().is_err())
}

/// Name-level screening for a host the proxy will resolve.
pub(crate) fn validate_proxied_host_name(host: &str) -> Result<(), FetchError> {
    let host = host.to_ascii_lowercase();
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
            e.proxy_for(&url("https://example.com/")).as_deref(),
            Some("http://proxy.corp:3128")
        );
        assert_eq!(
            e.proxy_for(&url("http://example.com/")).as_deref(),
            Some("http://all.corp:1")
        );
        assert_eq!(env(&[]).proxy_for(&url("https://example.com/")), None);
        // Unsupported schemes and empty values are ignored.
        let bad = env(&[("HTTPS_PROXY", "ftp://x"), ("HTTP_PROXY", "  ")]);
        assert_eq!(bad.proxy_for(&url("https://example.com/")), None);
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
        assert!(e.proxy_for(&url("https://example.com/")).is_some());
        assert!(e.proxy_for(&url("https://internal.corp/")).is_none());
        assert!(e.proxy_for(&url("https://a.internal.corp/")).is_none());
        assert!(e.proxy_for(&url("https://x.skip.test/")).is_none());
        assert!(e.proxy_for(&url("https://10.1.2.3/")).is_none());
        assert!(e.proxy_for(&url("https://host.test:8443/")).is_none());
        assert!(e.proxy_for(&url("https://host.test/")).is_some());
        assert!(e.proxy_for(&url("https://notinternal.corp/")).is_some());
        // Loopback never proxies, even with no NO_PROXY.
        let e = env(&[("HTTPS_PROXY", "http://p:1")]);
        assert!(e.proxy_for(&url("https://localhost/")).is_none());
        assert!(e.proxy_for(&url("https://127.0.0.1/")).is_none());
        assert!(e.proxy_for(&url("https://[::1]/")).is_none());
        let all = env(&[("HTTPS_PROXY", "http://p:1"), ("NO_PROXY", "*")]);
        assert!(all.proxy_for(&url("https://example.com/")).is_none());
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
        ] {
            assert_eq!(
                validate_proxied_host_name(denied),
                Err(FetchError::DestinationDenied),
                "{denied}"
            );
        }
        assert!(validate_proxied_host_name("docs.rs").is_ok());
    }
}
