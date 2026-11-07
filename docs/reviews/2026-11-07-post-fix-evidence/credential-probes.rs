
    #[tokio::test]
    async fn rereview_initial_gemini_plus_token_is_encoded() {
        let credential = CommandCredential::new("fixture-gateway", ApiKeyCommandSpec {
            command: vec!["unused".into()], ttl_secs: Some(3600), timeout_secs: Some(1),
        });
        credential.store("fixture+token".into());
        // Exact URI shape built by GeminiExt::build_uri on its initial call.
        let mut uri: http::Uri = "https://fixture.invalid/v1/models?key=fixture+token".parse().unwrap();
        assert!(credential.apply(&mut HeaderMap::new(), &mut uri).await.is_some());
        assert_eq!(uri.query(), Some("key=fixture%2Btoken"), "initial key must preserve its query value");
    }

    #[tokio::test]
    async fn rereview_initial_gemini_ampersand_keeps_credential_ownership() {
        let credential = CommandCredential::new("fixture-gateway", ApiKeyCommandSpec {
            command: vec!["unused".into()], ttl_secs: Some(3600), timeout_secs: Some(1),
        });
        credential.store("fixture&token".into());
        let mut uri: http::Uri = "https://fixture.invalid/v1/models?key=fixture&token".parse().unwrap();
        assert!(credential.apply(&mut HeaderMap::new(), &mut uri).await.is_some(), "SDK seed lost its renewal binding");
    }
