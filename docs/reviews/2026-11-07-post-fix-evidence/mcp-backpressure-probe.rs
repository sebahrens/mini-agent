    #[tokio::test]
    async fn rereview_eof_retires_runs_while_stdout_is_backpressured() {
        let marker = std::env::temp_dir().join(format!("mcp-eof-backpressure-{}.pid", std::process::id()));
        let script = format!("sleep 300 & echo $! > '{}'; wait", marker.display());
        let (mut client_in, server_in) = tokio::io::duplex(1 << 16);
        let (server_out, mut client_out) = tokio::io::duplex(1);
        let mut server = tokio::spawn(serve(shell(&script), server_in, server_out));
        let mut bytes = serde_json::to_vec(&call(7, json!({"prompt":"x"}))).unwrap();
        bytes.push(b'\n');
        client_in.write_all(&bytes).await.unwrap();
        let grandchild = tokio::time::timeout(Duration::from_secs(3), async { loop {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                if let Ok(pid) = text.trim().parse::<i32>() { break pid; }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        } }).await.expect("fixture starts");
        client_in.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"ping\"}\n").await.unwrap();
        // Prove dispatch is writing its reply, then leave stdout open but unread.
        let mut first = [0u8; 1];
        tokio::time::timeout(Duration::from_secs(2), client_out.read_exact(&mut first)).await.unwrap().unwrap();
        drop(client_in);
        let retired = tokio::time::timeout(Duration::from_millis(500), &mut server).await.is_ok();
        let child_live = nix::sys::signal::kill(nix::unistd::Pid::from_raw(grandchild), None).is_ok();
        // Always retire the probe before asserting so the negative case leaks nothing.
        if !retired { server.abort(); let _ = server.await; }
        let _ = std::fs::remove_file(&marker);
        assert!(retired && !child_live, "stdin EOF did not retire a backpressured server: retired={retired}, child_live={child_live}");
    }
