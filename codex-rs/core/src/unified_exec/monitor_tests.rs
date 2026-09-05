use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn monitor_context_is_bounded_and_overflow_is_visible() {
    use crate::context::ContextualUserFragment;
    let text = MonitorNotification::new("監視".repeat(100), "あ".repeat(2000)).render();
    assert!(text.len() < 900);
    assert!(text.contains("[truncated]"));
}

#[tokio::test]
async fn long_lines_are_bounded_even_with_a_trailing_newline() {
    let mut buffer = Vec::new();
    let mut lines = Vec::new();
    let mut count = 0;
    let mut deadline = None;
    let data = format!("{}\n", "x".repeat(100_000));
    assert!(!extend_lines(
        &mut buffer,
        data.as_bytes(),
        &mut lines,
        &mut count,
        &mut deadline
    ));
    assert!(buffer.is_empty());
    assert!(lines.iter().all(|line| line.len() <= MAX_LINE_BYTES + 32));
    assert!(lines[0].starts_with("(line truncated)"));
}

#[tokio::test]
async fn registry_tracks_insert_list_and_remove() {
    let manager = MonitorManager::new();
    manager
        .insert(
            "mon_a".to_string(),
            1,
            "watch a".to_string(),
            "cmd a".to_string(),
            tokio::spawn(async {}),
        )
        .await;
    manager
        .insert(
            "mon_b".to_string(),
            2,
            "watch b".to_string(),
            "cmd b".to_string(),
            tokio::spawn(async {}),
        )
        .await;

    assert_eq!(manager.list().await.len(), 2);

    // `remove` returns the process id so the caller can terminate it; a
    // second remove of the same id is a no-op.
    assert_eq!(manager.remove("mon_a").await, Some(1));
    assert_eq!(manager.remove("mon_a").await, None);

    let remaining = manager.list().await;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, "mon_b");

    manager.abort_all().await;
    assert!(manager.list().await.is_empty());
}

#[tokio::test]
async fn deregister_self_removes_entry_without_aborting_its_task() {
    let manager = MonitorManager::new();
    // A task that runs until aborted, so we can observe whether it survives.
    let task = tokio::spawn(std::future::pending::<()>());
    let handle = task.abort_handle();
    manager
        .insert(
            "mon_x".to_string(),
            7,
            "watch".to_string(),
            "cmd".to_string(),
            task,
        )
        .await;
    assert_eq!(manager.list().await.len(), 1);

    manager.deregister_self("mon_x").await;
    assert!(manager.list().await.is_empty(), "entry pruned");
    // Unlike `remove`, deregister_self must NOT abort the entry's task: the
    // loop removing itself still has its final exit notice to deliver.
    tokio::task::yield_now().await;
    assert!(
        !handle.is_finished(),
        "deregister_self must not abort the entry's task"
    );

    // Deregistering an absent id is a no-op.
    manager.deregister_self("mon_x").await;
    assert!(manager.list().await.is_empty());

    handle.abort();
}
