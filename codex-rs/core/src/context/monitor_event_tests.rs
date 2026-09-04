use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::MonitorEventKind;
use codex_protocol::protocol::MonitorWakePolicy;

use super::*;

#[test]
fn monitor_event_fragment_is_bounded_and_classified() {
    let fragment = MonitorEventFragment::new(MonitorEvent {
        id: "event-1".to_string(),
        monitor_name: "test-watcher".to_string(),
        sequence: 1,
        kind: MonitorEventKind::OutputBatch,
        summary: format!("3 tests failed\n{}", "x".repeat(40 * 1024)),
        wake_policy: MonitorWakePolicy::AttachOrWake,
    });

    assert_eq!(fragment.role(), "assistant");
    assert_eq!(
        fragment.content_kind(),
        ContentItemKind("monitor.event".to_string())
    );
    assert!(fragment.requires_separate_message());

    let item = ContextualUserFragment::into(fragment);
    let ResponseItem::Message { role, content, .. } = &item else {
        panic!("expected monitor context message");
    };
    assert_eq!(role, "assistant");
    assert!(matches!(
        content.as_slice(),
        [ContentItem::OutputText { .. }]
    ));
    let json = serde_json::to_string(&item).expect("serialize monitor context");
    assert!(json.contains("test-watcher"));
    assert!(json.contains("3 tests failed"));
    assert!(json.contains("SYSTEM NOTIFICATION"));
    assert!(json.contains("monitor event truncated"));
    assert!(json.len() < 40 * 1024);
}
