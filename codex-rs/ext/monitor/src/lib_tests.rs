use super::*;

fn test_state() -> Arc<MonitorState> {
    Arc::new(MonitorState {
        monitors: Mutex::new(HashMap::new()),
        seq: AtomicU64::new(0),
        instance_counter: AtomicU64::new(0),
        pending_event_count: AtomicU64::new(0),
        thread_manager: Weak::new(),
        thread_id: ThreadId::new(),
    })
}

#[test]
fn monitor_names_are_bounded_and_shell_safe() {
    assert_eq!(validate_name("test-watcher_1"), Ok(()));
    assert!(validate_name("").is_err());
    assert!(validate_name(&"a".repeat(MAX_NAME_LEN + 1)).is_err());
    assert!(validate_name("test watcher").is_err());
    assert!(validate_name("test;watcher").is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn stop_monitor_terminates_process_and_removes_registration() {
    let state = test_state();
    start_monitor(
        Arc::clone(&state),
        StartArgs {
            name: "test-watcher".to_string(),
            command: "while :; do sleep 1; done".to_string(),
        },
    )
    .await
    .expect("start monitor");

    assert!(state.monitors.lock().await.contains_key("test-watcher"));

    stop_monitor(
        Arc::clone(&state),
        StopArgs {
            name: "test-watcher".to_string(),
        },
    )
    .await
    .expect("stop monitor");

    assert!(!state.monitors.lock().await.contains_key("test-watcher"));
}
