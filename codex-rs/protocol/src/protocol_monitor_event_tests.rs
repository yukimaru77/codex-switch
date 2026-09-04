use super::*;

#[test]
fn monitor_event_bounds_summary_and_stderr_on_utf8_boundaries() {
    let oversized = "界".repeat(MAX_MONITOR_EVENT_SUMMARY_BYTES);
    let event = MonitorEvent {
        id: "event-1".to_string(),
        monitor_name: "watch".to_string(),
        sequence: 1,
        kind: MonitorEventKind::Failed {
            exit_code: 1,
            stderr_tail: Some(oversized.clone()),
        },
        summary: oversized,
        wake_policy: MonitorWakePolicy::AttachOrWake,
    }
    .into_bounded();

    assert!(event.summary.len() <= MAX_MONITOR_EVENT_SUMMARY_BYTES);
    assert!(event.summary.is_char_boundary(event.summary.len()));
    assert!(event.summary.ends_with(MONITOR_EVENT_TRUNCATION_MARKER));
    let MonitorEventKind::Failed {
        stderr_tail: Some(stderr_tail),
        ..
    } = event.kind
    else {
        panic!("expected failed monitor event");
    };
    assert!(stderr_tail.len() <= MAX_MONITOR_EVENT_SUMMARY_BYTES);
    assert!(stderr_tail.is_char_boundary(stderr_tail.len()));
    assert!(stderr_tail.ends_with(MONITOR_EVENT_TRUNCATION_MARKER));
}
