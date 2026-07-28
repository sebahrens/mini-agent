use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use compact_str::CompactString;
use rmcp::service::{RoleClient, RunningService, serve_client};
use rmcp::transport::{child_process::TokioChildProcess, which_command};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::task::JoinHandle;

use super::config::{McpServerConfig, TrustedMcpServer};

const MCP_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
const MCP_STDERR_LIMIT: usize = 8 * 1024;
const MCP_STDERR_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

pub struct McpClientHandle {
    pub server_name: CompactString,
    pub trusted_identity: Option<TrustedMcpServer>,
    pub running_service: RunningService<RoleClient, ()>,
}

impl McpClientHandle {
    pub async fn connect(
        server_name: CompactString,
        config: &McpServerConfig,
    ) -> anyhow::Result<Self> {
        Self::connect_with_timeout(server_name, config, MCP_INITIALIZE_TIMEOUT).await
    }

    pub(crate) async fn connect_with_timeout(
        server_name: CompactString,
        config: &McpServerConfig,
        initialize_timeout: Duration,
    ) -> anyhow::Result<Self> {
        match config {
            McpServerConfig::Command { command, args, env } => {
                tracing::debug!(
                    "MCP command transport: {} {:?} ({} env vars)",
                    command,
                    args,
                    env.len(),
                );
                let cmd = stdio_command(command, args, env).map_err(|error| {
                    anyhow::anyhow!(
                        "MCP command resolution failed for '{server_name}' ({command}): {error}"
                    )
                })?;
                let (transport, stderr) = TokioChildProcess::builder(cmd)
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "MCP command spawn failed for '{server_name}' ({command}): {error}"
                        )
                    })?;
                let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
                let stderr_task =
                    stderr.map(|stderr| capture_stderr(stderr, Arc::clone(&stderr_buffer)));
                let running_service =
                    match tokio::time::timeout(initialize_timeout, serve_client((), transport))
                        .await
                    {
                        Ok(Ok(service)) => service,
                        Ok(Err(error)) => {
                            return Err(stdio_connect_error(
                                &server_name,
                                format!("initialization failed: {error}"),
                                stderr_task,
                                stderr_buffer,
                            )
                            .await);
                        }
                        Err(_) => {
                            return Err(stdio_connect_error(
                                &server_name,
                                format!(
                                    "initialization timed out after {} ms",
                                    initialize_timeout.as_millis()
                                ),
                                stderr_task,
                                stderr_buffer,
                            )
                            .await);
                        }
                    };
                Ok(Self {
                    server_name,
                    trusted_identity: config.trusted_identity(),
                    running_service,
                })
            }
            McpServerConfig::Url {
                url,
                headers,
                oauth,
            } => {
                validate_mcp_server_url(url).await?;
                tracing::debug!(
                    "MCP HTTP transport: {} ({} headers, OAuth: {})",
                    url,
                    headers.len(),
                    oauth.is_some(),
                );
                let custom_headers = parse_headers(headers)?;
                let cfg = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.as_str())
                    .custom_headers(custom_headers);

                let oauth_settings = oauth.as_ref().and_then(|o| o.settings());
                let running_service = if let Some(settings) = oauth_settings {
                    let auth_client =
                        super::oauth::build_auth_client(&server_name, url, &settings).await?;
                    type AuthHttpClient = rmcp::transport::StreamableHttpClientTransport<
                        rmcp::transport::auth::AuthClient<reqwest::Client>,
                    >;
                    let transport = AuthHttpClient::with_client(auth_client, cfg);
                    serve_client((), transport).await.map_err(|e| {
                        anyhow::anyhow!("MCP HTTP connection failed for '{server_name}': {e}")
                    })?
                } else {
                    type HttpClient =
                        rmcp::transport::StreamableHttpClientTransport<reqwest::Client>;
                    let transport = HttpClient::from_config(cfg);
                    serve_client((), transport).await.map_err(|e| {
                        anyhow::anyhow!("MCP HTTP connection failed for '{server_name}': {e}")
                    })?
                };
                Ok(Self {
                    server_name,
                    trusted_identity: config.trusted_identity(),
                    running_service,
                })
            }
            McpServerConfig::BuiltIn { identity, headers } => {
                let url = identity.endpoint();
                tracing::debug!(
                    "MCP built-in HTTP transport: {} ({} headers)",
                    url,
                    headers.len(),
                );
                let custom_headers = parse_headers(headers)?;
                let cfg = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url)
                    .custom_headers(custom_headers);
                type HttpClient = rmcp::transport::StreamableHttpClientTransport<reqwest::Client>;
                let transport = HttpClient::from_config(cfg);
                let running_service = serve_client((), transport).await.map_err(|e| {
                    anyhow::anyhow!("MCP HTTP connection failed for '{server_name}': {e}")
                })?;
                Ok(Self {
                    server_name,
                    trusted_identity: config.trusted_identity(),
                    running_service,
                })
            }
        }
    }

    pub fn peer(&self) -> rmcp::service::Peer<RoleClient> {
        self.running_service.peer().clone()
    }

    pub async fn list_tools(&self) -> Result<Vec<rmcp::model::Tool>, rmcp::ServiceError> {
        self.running_service.peer().list_all_tools().await
    }
}

fn capture_stderr(
    mut stderr: tokio::process::ChildStderr,
    buffer: Arc<Mutex<Vec<u8>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut chunk = [0_u8; 1024];
        loop {
            let read = match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let mut captured = buffer.lock().unwrap_or_else(|error| error.into_inner());
            let remaining = MCP_STDERR_LIMIT.saturating_sub(captured.len());
            captured.extend_from_slice(&chunk[..read.min(remaining)]);
        }
    })
}

async fn stdio_connect_error(
    server_name: &str,
    reason: String,
    stderr_task: Option<JoinHandle<()>>,
    stderr_buffer: Arc<Mutex<Vec<u8>>>,
) -> anyhow::Error {
    if let Some(task) = stderr_task {
        let _ = tokio::time::timeout(MCP_STDERR_DRAIN_TIMEOUT, task).await;
    }
    let diagnostic = {
        let captured = stderr_buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        String::from_utf8_lossy(&captured).trim().to_owned()
    };
    if diagnostic.is_empty() {
        anyhow::anyhow!("MCP connection failed for '{server_name}': {reason}")
    } else {
        anyhow::anyhow!("MCP connection failed for '{server_name}': {reason}; stderr: {diagnostic}")
    }
}

fn stdio_command(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> std::io::Result<Command> {
    let mut command = which_command(command)?;
    command.args(args).envs(env);
    Ok(command)
}

fn parse_headers(
    headers: &HashMap<String, String>,
) -> anyhow::Result<HashMap<http::HeaderName, http::HeaderValue>> {
    let mut result = HashMap::new();
    for (name, value) in headers {
        let h_name: http::HeaderName = name
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid header name '{name}': {e}"))?;
        let h_value: http::HeaderValue = value
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid header value for '{name}': {e}"))?;
        result.insert(h_name, h_value);
    }
    Ok(result)
}

async fn validate_mcp_server_url(value: &str) -> anyhow::Result<()> {
    let (host, port, literal_ip) = parse_mcp_server_url(value)?;
    let addresses = if let Some(address) = literal_ip {
        vec![address]
    } else {
        let resolver_host = host.clone();
        tokio::task::spawn_blocking(move || {
            (resolver_host.as_str(), port)
                .to_socket_addrs()
                .map(|addresses| addresses.map(|address| address.ip()).collect::<Vec<_>>())
        })
        .await
        .map_err(|error| anyhow::anyhow!("could not resolve MCP server host '{host}': {error}"))?
        .map_err(|error| anyhow::anyhow!("could not resolve MCP server host '{host}': {error}"))?
    };

    validate_resolved_addresses(&addresses)
}

fn parse_mcp_server_url(value: &str) -> anyhow::Result<(String, u16, Option<IpAddr>)> {
    let url = reqwest::Url::parse(value)
        .map_err(|error| anyhow::anyhow!("invalid MCP server URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("MCP server URL must use the http or https scheme");
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("MCP server URL must not include a username or password");
    }

    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("MCP server URL must include a host"))?;
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .to_owned();
    if host.eq_ignore_ascii_case("localhost")
        || host
            .to_ascii_lowercase()
            .strip_suffix(".localhost")
            .is_some()
    {
        anyhow::bail!("MCP server URL host is local; use a publicly routable HTTP(S) API endpoint");
    }

    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("MCP server URL must include a valid port"))?;
    let literal_ip = host.parse().ok();
    Ok((host, port, literal_ip))
}

fn validate_resolved_addresses(addresses: &[IpAddr]) -> anyhow::Result<()> {
    if addresses.is_empty() {
        anyhow::bail!("MCP server host did not resolve to an IP address");
    }

    for address in addresses {
        if is_restricted_ip(*address) {
            anyhow::bail!(
                "MCP server URL resolves to non-public address {address}; use a publicly routable HTTP(S) API endpoint"
            );
        }
    }
    Ok(())
}

fn is_restricted_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_restricted_ipv4(address),
        IpAddr::V6(address) => {
            let octets = address.octets();
            let is_ipv4_compatible = octets[..12].iter().all(|octet| *octet == 0);
            let is_ipv4_mapped = octets[..10].iter().all(|octet| *octet == 0)
                && octets[10] == 0xff
                && octets[11] == 0xff;
            if is_ipv4_compatible || is_ipv4_mapped {
                return is_restricted_ipv4(Ipv4Addr::new(
                    octets[12], octets[13], octets[14], octets[15],
                ));
            }

            let segments = address.segments();
            let is_global_unicast = (0x2000..=0x3fff).contains(&segments[0]);
            let is_teredo = segments[0] == 0x2001 && segments[1] == 0;
            let is_benchmark = segments[0] == 0x2001 && segments[1] == 2 && segments[2] == 0;
            let is_orchid =
                segments[0] == 0x2001 && matches!(segments[1] & 0xfff0, 0x0010 | 0x0020);
            let is_documentation = (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || (segments[0] == 0x3fff && segments[1] & 0xf000 == 0);
            let is_6to4 = segments[0] == 0x2002;

            !is_global_unicast
                || is_teredo
                || is_benchmark
                || is_orchid
                || is_documentation
                || is_6to4
        }
    }
}

fn is_restricted_ipv4(address: Ipv4Addr) -> bool {
    let [first, second, third, _] = address.octets();
    first == 0
        || first == 10
        || (first == 100 && (64..=127).contains(&second))
        || first == 127
        || (first == 169 && second == 254)
        || (first == 172 && (16..=31).contains(&second))
        || (first == 192 && second == 0 && third == 0)
        || (first == 192 && second == 0 && third == 2)
        || (first == 192 && second == 88 && third == 99)
        || (first == 192 && second == 168)
        || (first == 198 && (18..=19).contains(&second))
        || (first == 198 && second == 51 && third == 100)
        || (first == 203 && second == 0 && third == 113)
        || first >= 224
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr};

    use super::{
        parse_mcp_server_url, stdio_command, validate_mcp_server_url, validate_resolved_addresses,
    };

    #[test]
    fn stdio_command_resolves_path_lookup_before_spawn() {
        let command = stdio_command("rustc", &[], &HashMap::new()).unwrap();

        assert!(std::path::Path::new(command.as_std().get_program()).is_absolute());
    }

    #[tokio::test]
    async fn rejects_local_and_private_mcp_server_urls() {
        for url in [
            "http://127.0.0.1:8080",
            "http://localhost:8080",
            "http://192.168.0.1",
            "http://10.0.0.1",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]",
        ] {
            let error = validate_mcp_server_url(url).await.unwrap_err();
            assert!(
                error.to_string().contains("publicly routable"),
                "unexpected error for {url}: {error}"
            );
        }
    }

    #[test]
    fn accepts_public_https_url_with_public_resolution() {
        let (host, port, literal_ip) = parse_mcp_server_url("https://api.example.com/mcp").unwrap();

        assert_eq!(host, "api.example.com");
        assert_eq!(port, 443);
        assert_eq!(literal_ip, None);
        assert!(
            validate_resolved_addresses(&[IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]).is_ok()
        );
    }

    #[test]
    fn rejects_non_http_scheme_and_embedded_credentials() {
        let scheme_error = parse_mcp_server_url("ftp://api.example.com").unwrap_err();
        assert!(scheme_error.to_string().contains("http or https"));

        let credentials_error =
            parse_mcp_server_url("https://user:password@api.example.com").unwrap_err();
        assert!(
            credentials_error
                .to_string()
                .contains("username or password")
        );
    }
}
