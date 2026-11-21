use super::*;

#[test]
fn saturated_reader_stop_preserves_order_and_background_senders() {
    let (sender, mut receiver) = mpsc::channel(1);
    let background = sender.clone();
    #[cfg(feature = "goal")]
    let mut run = AgentRunState::default();
    let mut deferred = std::collections::VecDeque::from([UserEvent::Paste("earlier".into())]);
    // Exercise a second reader on the same event channel after a handoff.
    for round in 0..2 {
        // A goal round's verification is in flight across the handoff,
        // registered exactly as `start_goal_verification` registers it.
        #[cfg(feature = "goal")]
        let (operation_id, _cancelled) = {
            run.goal_gate_generation += 1;
            let operation_id = crate::event::ValidationOperationId(run.goal_gate_generation);
            let (cancel, cancelled) = tokio::sync::oneshot::channel();
            run.pending_goal_gate = Some(crate::ui::state::PendingGoalGate {
                operation_id,
                cancel,
                goal_id: "handoff-goal".into(),
                summary: crate::extras::goal::gate::RoundSummary::completed(),
                request: crate::extras::goal::gate::VerifyRequest {
                    run_verify_command: false,
                    run_checks: true,
                    run_judge: false,
                    cause: crate::extras::goal::gate::VerifyCause::MetClaim,
                },
            });
            (operation_id, cancelled)
        };
        sender
            .blocking_send(UserEvent::Paste(format!("queued-{round}")))
            .unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let reader_sender = sender.clone();
        let reader_running = running.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let mut handle = Some(crate::ui::InputReader::spawn(move || {
            entered_tx.send(()).unwrap();
            reader_sender
                .blocking_send(UserEvent::Paste(format!("reader-{round}")))
                .unwrap();
            assert!(!reader_running.load(Ordering::Relaxed));
        }));
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        pause_input_reader(&running, &mut handle, &mut receiver, &mut deferred);
        assert!(handle.is_none());
        assert!(!receiver.is_closed());
        while let Ok(event) = receiver.try_recv() {
            deferred.push_back(event);
        }
        // A completion producer captured before the handoff is still live.
        #[cfg(feature = "goal")]
        let completion = {
            assert!(run.validation_active());
            UserEvent::GoalVerificationDone(Box::new(crate::event::GoalVerificationEvent {
                operation_id,
                checks: Some(crate::extras::goal::gate::CheckOutcome {
                    failure_kind: Default::default(),
                    all_passed: true,
                    failure_tail: Some(format!("completion-{round}")),
                    verified: Vec::new(),
                }),
                judge: None,
                judge_tokens: 0,
                judge_charge: None,
                interrupted: false,
            }))
        };
        #[cfg(not(feature = "goal"))]
        let completion = UserEvent::LinkOpenFailed(format!("completion-{round}"));
        background.blocking_send(completion).unwrap();
        let completion = receiver.blocking_recv().unwrap();
        // The completion still settles the round registered before the
        // handoff, and only that round.
        #[cfg(feature = "goal")]
        {
            let UserEvent::GoalVerificationDone(event) = &completion else {
                panic!("unexpected completion: {completion:?}");
            };
            assert!(
                run.pending_goal_gate
                    .take_if(|pending| pending.operation_id == event.operation_id)
                    .is_some()
            );
            assert!(!run.validation_active());
        }
        deferred.push_back(completion);
        pause_input_reader(&running, &mut handle, &mut receiver, &mut deferred);
    }
    let actual: Vec<_> = deferred
        .into_iter()
        .map(|event| match event {
            UserEvent::Paste(text) | UserEvent::LinkOpenFailed(text) => text,
            #[cfg(feature = "goal")]
            UserEvent::GoalVerificationDone(event) => event
                .checks
                .and_then(|outcome| outcome.failure_tail)
                .unwrap_or_default(),
            other => panic!("unexpected event: {other:?}"),
        })
        .collect();
    assert_eq!(
        actual,
        [
            "earlier",
            "queued-0",
            "reader-0",
            "completion-0",
            "queued-1",
            "reader-1",
            "completion-1"
        ]
    );
}
