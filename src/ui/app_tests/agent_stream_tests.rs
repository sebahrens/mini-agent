use super::*;

#[tokio::test]
async fn delayed_terminal_event_is_delivered_before_stream_closure() {
    let (tx, rx) = mpsc::channel(1);
    let mut receiver = Some(rx);
    let sender = tokio::spawn(async move {
        tokio::task::yield_now().await;
        tx.send(AgentEvent::Done {
            response: "done".into(),
            interactions: Vec::new(),
        })
        .await
        .unwrap();
    });

    let item = tokio::time::timeout(
        Duration::from_secs(1),
        recv_agent_stream_item(&mut receiver),
    )
    .await
    .unwrap();
    assert!(matches!(
        item,
        Some(AgentStreamItem::Event(AgentEvent::Done { .. }))
    ));
    sender.await.unwrap();
}

#[tokio::test]
async fn missing_terminal_event_is_reported_as_stream_closure() {
    let (tx, rx) = mpsc::channel(1);
    let mut receiver = Some(rx);
    drop(tx);

    assert!(matches!(
        recv_agent_stream_item(&mut receiver).await,
        Some(AgentStreamItem::Closed)
    ));
}
