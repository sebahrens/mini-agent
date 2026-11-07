
    #[test]
    fn rereview_retry_across_rotation_counts_receipt_once() {
        let root = crate::tests::OwnedTestDirectory::new("rereview-retry-rotation");
        let state = root.path().join("state");
        crate::fs::ensure_private_directory(&state).unwrap();
        synthetic_full_ledger(&state, MAX_LEDGER_RECORDS - 1);
        let ledger = Ledger::new(&state);
        let retry = one_receipt("retry-session", 11);
        let mut persisted = std::fs::OpenOptions::new().append(true).open(state.join("usage.jsonl")).unwrap();
        persisted.write_all(&serde_json::to_vec(retry.row.as_ref()).unwrap()).unwrap();
        persisted.write_all(b"\n").unwrap();
        persisted.sync_all().unwrap();
        drop(persisted);
        // The first write reached disk but its acknowledgement was lost; its
        // original account still carries the same immutable pending receipt.
        let account = Account::new("retry-session", None, 0.0).unwrap();
        account.observe(&retry).unwrap();
        let fresh = one_receipt("retry-session", 13);
        account.observe(&fresh).unwrap();
        ledger.flush(&account).unwrap();
        let summary = ledger.summary_for_session("retry-session").unwrap();
        assert_eq!(summary.session.records, 2, "retried receipt must stay exactly once across rotation");
        assert_eq!(summary.session.input_tokens, 24);
    }

    #[test]
    fn rereview_rollup_keeps_untracked_session_charge() {
        let root = crate::tests::OwnedTestDirectory::new("rereview-rollup-session-cap");
        let state = root.path().join("state");
        crate::fs::ensure_private_directory(&state).unwrap();
        let mut bytes = Vec::new();
        for i in 0..=MAX_CHECKPOINT_SESSIONS {
            let r = one_receipt(&format!("session-{i}"), 11);
            bytes.extend_from_slice(&serde_json::to_vec(r.row.as_ref()).unwrap());
            bytes.push(b'\n');
        }
        // Valid archive left by rotation; recovered in memory as at startup.
        crate::fs::private_atomic_create_sync(&state.join(archive_name(1)), &bytes).unwrap();
        let summary = Ledger::new(&state).session_summary(&format!("session-{MAX_CHECKPOINT_SESSIONS}")).unwrap();
        assert_eq!(summary.session.records, 1, "session detail cap must not report an incurred charge as zero");
    }
