use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AcpServerConfig {
    #[serde(rename = "tcp")]
    Tcp {
        host: String,
        port: u16,
        #[serde(default)]
        api_key: Option<String>,
    },
    #[serde(rename = "stdio")]
    Stdio,
}

impl AcpServerConfig {
    #[allow(dead_code)]
    pub fn transport_type(&self) -> &str {
        match self {
            AcpServerConfig::Tcp { .. } => "tcp",
            AcpServerConfig::Stdio => "stdio",
        }
    }

    pub(crate) fn tcp_endpoint(&self) -> Option<(&str, u16, Option<&str>)> {
        match self {
            AcpServerConfig::Tcp {
                host,
                port,
                api_key,
            } => Some((host, *port, api_key.as_deref())),
            AcpServerConfig::Stdio => None,
        }
    }
}

impl fmt::Debug for AcpServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AcpServerConfig::Tcp {
                host,
                port,
                api_key,
            } => f
                .debug_struct("Tcp")
                .field("host", host)
                .field("port", port)
                .field("api_key", &api_key.as_ref().map(|_| "[REDACTED]"))
                .finish(),
            AcpServerConfig::Stdio => f.write_str("Stdio"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_redacts_tcp_api_key() {
        let config = AcpServerConfig::Tcp {
            host: "127.0.0.1".to_owned(),
            port: 7243,
            api_key: Some("must-never-appear".to_owned()),
        };

        let debug = format!("{config:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("must-never-appear"));
    }

    #[test]
    fn tcp_endpoint_exposes_config_without_changing_serialization() {
        let config: AcpServerConfig = serde_json::from_str(
            r#"{"type":"tcp","host":"127.0.0.1","port":7243,"api_key":"secret"}"#,
        )
        .unwrap();

        assert_eq!(
            config.tcp_endpoint(),
            Some(("127.0.0.1", 7243, Some("secret")))
        );
    }
}
