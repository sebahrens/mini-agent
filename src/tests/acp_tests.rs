#[cfg(test)]
mod tests {
    use crate::cli::Cli;
    use crate::config::Config;
    use crate::extras::acp::config::AcpServerConfig;
    use crate::permission::SecurityMode;

    #[test]
    fn test_acp_config_tcp_deserialization() {
        let json = r#"{"type":"tcp","host":"0.0.0.0","port":7243}"#;
        let cfg: AcpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.transport_type(), "tcp");
    }

    #[test]
    fn test_acp_config_tcp_with_api_key() {
        let json = r#"{"type":"tcp","host":"127.0.0.1","port":9999,"api_key":"secret"}"#;
        let cfg: AcpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.transport_type(), "tcp");
    }

    #[test]
    fn test_acp_config_stdio() {
        let json = r#"{"type":"stdio"}"#;
        let cfg: AcpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.transport_type(), "stdio");
    }

    #[test]
    fn test_acp_config_unknown_type_errors() {
        let json = r#"{"type":"http","url":"https://example.com"}"#;
        let result: Result<AcpServerConfig, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_acp_cli_defaults() {
        let cli = Cli::default();
        assert!(!cli.acp_enabled);
        assert!(cli.acp_host.is_none());
        assert!(cli.acp_port.is_none());
    }

    #[test]
    fn test_acp_cli_tcp_config() {
        let cli = Cli {
            acp_enabled: true,
            acp_host: Some("0.0.0.0".into()),
            acp_port: Some(7243),
            ..Default::default()
        };
        assert!(cli.acp_enabled);
        assert_eq!(cli.acp_host.as_deref(), Some("0.0.0.0"));
        assert_eq!(cli.acp_port, Some(7243));
    }

    #[test]
    fn test_acp_config_default_fields() {
        let cfg = Config::default();
        assert!(cfg.acp_servers.is_none());
        assert!(cfg.acp_host.is_none());
        assert!(cfg.acp_port.is_none());
    }

    #[test]
    fn test_security_mode_discriminants() {
        use SecurityMode::*;
        let modes = [Yolo, Standard, Guarded, ReadOnly, Restrictive];
        assert_eq!(modes.len(), 5);
        assert!(matches!(SecurityMode::Yolo, SecurityMode::Yolo));
    }
}
