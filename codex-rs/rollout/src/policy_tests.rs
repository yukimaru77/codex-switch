use codex_protocol::protocol::MonitorNotificationEvent;

use super::*;

#[test]
fn monitor_notifications_are_transient_in_all_history_modes() {
    let event = EventMsg::MonitorNotification(MonitorNotificationEvent {
        monitor_name: "watch".to_string(),
        summary: "changed".to_string(),
        kind: "output".to_string(),
    });

    assert!(!should_persist_event_msg(&event, ThreadHistoryMode::Legacy));
    assert!(!should_persist_event_msg(
        &event,
        ThreadHistoryMode::Paginated
    ));
}
