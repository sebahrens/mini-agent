    #[cfg(feature = "hooks")]
    #[tokio::test]
    async fn continuation_child_manager_does_not_poison_parent_hook_peer() {
        let Some((url, _)) = serve_catalogue().await else { return; };
        let mut parent = linked(&url).await;
        let name = format!("continuation-parent-hook-{}", uuid::Uuid::new_v4());
        parent.server_name = name.clone().into();
        crate::extras::hooks::backend::register_mcp_peers(std::slice::from_ref(&parent));
        let mut child = linked(&url).await;
        child.server_name = name.clone().into();
        crate::extras::hooks::backend::register_mcp_peers(std::slice::from_ref(&child));
        drop(child);
        assert!(!parent.live_peer().is_transport_closed(), "the original parent connection is still live");
        let result = crate::extras::hooks::backend::call_mcp(
            &name, "tool_v1", serde_json::Map::new(), Duration::from_secs(1)
        ).await;
        let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
        assert!(!stderr.contains("is not connected"),
            "child retirement poisoned a parent hook despite its live peer: {stderr}");
    }
