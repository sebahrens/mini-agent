
#[tokio::test]
async fn continuation_oauth_refresh_waiter_uses_the_completed_refresh() {
    let roots = crate::tests::OwnedTestDirectory::new("review-oauth-refresh-contention");
    let root = roots.path().join("credentials");
    let mut store = LockedStore::open(root.clone(), true).unwrap().unwrap();
    store.data.accounts.push(Account {
        client_id: "oaiapp_review".into(), subject: "review-subject".into(), email: None,
        tokens: Some(Tokens { access: "old-fixture-token".into(), refresh: "fixture-refresh".into(),
            id: "fixture-id".into(), scopes: SCOPES.into(), expires: now().unwrap() }),
    });
    store.data.active = Some("oaiapp_review".into());
    store.save().unwrap();
    // The lock holder models the first caller completing its refresh. A
    // concurrent caller must wait for that bounded refresh, not fail forever.
    let owner = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        let token = store.data.accounts[0].tokens.as_mut().unwrap();
        token.access = "new-fixture-token".into();
        token.expires = now().unwrap() + 3600;
        store.save().unwrap();
    });
    let registration = Registration { root, client_id: "oaiapp_review".into(), endpoints: None };
    let result = registration.access_token().await;
    owner.join().unwrap();
    if let Err(error) = &result {
        assert!(!crate::retry::is_retryable(error.as_ref()),
            "control: current lock-busy failure must be nonretryable");
    }
    assert_eq!(result.unwrap(), "new-fixture-token");
}
