use codex_protocol::AgentPath;

use super::*;

fn make_mail(
    author: AgentPath,
    recipient: AgentPath,
    content: &str,
    trigger_turn: bool,
) -> InterAgentCommunication {
    InterAgentCommunication::new(
        author,
        recipient,
        Vec::new(),
        content.to_string(),
        trigger_turn,
    )
}

fn make_monitor_event(name: &str, summary: &str, wake: bool) -> MonitorEvent {
    MonitorEvent {
        id: format!("test-{name}"),
        monitor_name: name.to_string(),
        sequence: 0,
        kind: codex_protocol::protocol::MonitorEventKind::OutputBatch,
        summary: summary.to_string(),
        wake_policy: if wake {
            codex_protocol::protocol::MonitorWakePolicy::AttachOrWake
        } else {
            codex_protocol::protocol::MonitorWakePolicy::AttachOnly
        },
    }
}

#[tokio::test]
async fn monitor_event_enqueue_makes_mailbox_pending() {
    let input_queue = InputQueue::new();
    assert!(!input_queue.has_pending_mailbox_items().await);

    input_queue
        .enqueue_monitor_event(make_monitor_event("test", "hello", false))
        .await;
    assert!(input_queue.has_pending_mailbox_items().await);
}

#[tokio::test]
async fn monitor_event_attach_or_wake_triggers_turn() {
    let input_queue = InputQueue::new();
    assert!(!input_queue.has_trigger_turn_mailbox_items().await);

    input_queue
        .enqueue_monitor_event(make_monitor_event("test", "wake up", true))
        .await;
    assert!(input_queue.has_trigger_turn_mailbox_items().await);
}

#[tokio::test]
async fn monitor_event_attach_only_does_not_trigger_turn() {
    let input_queue = InputQueue::new();

    input_queue
        .enqueue_monitor_event(make_monitor_event("test", "quiet", false))
        .await;
    assert!(!input_queue.has_trigger_turn_mailbox_items().await);
    assert!(input_queue.has_pending_mailbox_items().await);
}

#[tokio::test]
async fn monitor_events_drain_with_mailbox() {
    let input_queue = InputQueue::new();

    let mail = make_mail(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        "mail",
        false,
    );
    input_queue
        .enqueue_mailbox_communication(mail.clone(), Default::default())
        .await;
    let event = make_monitor_event("test", "event", false);
    input_queue.enqueue_monitor_event(event.clone()).await;

    let (items, _) = input_queue.drain_mailbox_input_items().await;
    assert_eq!(
        items,
        vec![
            TurnInput::InterAgentCommunication(mail),
            TurnInput::MonitorEvent(event),
        ]
    );

    assert!(!input_queue.has_pending_mailbox_items().await);
}

#[tokio::test]
async fn deferred_mailbox_suppresses_mail_but_not_monitor_events() {
    let input_queue = InputQueue::new();
    let active_turn = Mutex::new(Some(ActiveTurn::default()));
    let turn_state = Arc::clone(
        &active_turn
            .lock()
            .await
            .as_ref()
            .expect("active turn")
            .turn_state,
    );
    turn_state
        .lock()
        .await
        .set_mailbox_delivery_phase(MailboxDeliveryPhase::NextTurn);

    input_queue
        .enqueue_mailbox_communication(
            make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "queued mail",
                false,
            ),
            Default::default(),
        )
        .await;
    assert!(!input_queue.has_pending_input(&active_turn).await);

    input_queue
        .enqueue_monitor_event(make_monitor_event("test", "event", false))
        .await;
    assert!(input_queue.has_pending_input(&active_turn).await);
}

#[tokio::test]
async fn monitor_event_queue_drops_old_output_at_capacity() {
    let input_queue = InputQueue::new();
    for index in 0..(MAX_PENDING_MONITOR_EVENTS + 5) {
        input_queue
            .enqueue_monitor_event(make_monitor_event("test", &format!("event-{index}"), false))
            .await;
    }

    let events = input_queue.monitor_pending_events.lock().await;
    assert_eq!(events.len(), MAX_PENDING_MONITOR_EVENTS);
    assert_eq!(
        events.front().map(|event| event.summary.as_str()),
        Some("event-5")
    );
}

#[tokio::test]
async fn monitor_event_queue_preserves_lifecycle_events_before_output() {
    let input_queue = InputQueue::new();
    let mut completed = make_monitor_event("completed", "done", false);
    completed.kind = codex_protocol::protocol::MonitorEventKind::Completed { exit_code: 0 };
    input_queue.enqueue_monitor_event(completed.clone()).await;
    for index in 0..MAX_PENDING_MONITOR_EVENTS {
        input_queue
            .enqueue_monitor_event(make_monitor_event("test", &format!("event-{index}"), false))
            .await;
    }

    let events = input_queue.monitor_pending_events.lock().await;
    assert_eq!(events.len(), MAX_PENDING_MONITOR_EVENTS);
    assert!(events.contains(&completed));
}
