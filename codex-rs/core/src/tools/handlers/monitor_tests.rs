use super::*;

#[test]
fn validates_names_and_commands() {
    assert!(
        validate_start_args(&StartArgs {
            name: "test-watcher_1".to_string(),
            command: "cargo test".to_string(),
        })
        .is_ok()
    );
    for name in ["", "has space", "semi;colon"] {
        assert!(
            validate_start_args(&StartArgs {
                name: name.to_string(),
                command: "true".to_string(),
            })
            .is_err()
        );
    }
    assert!(
        validate_start_args(&StartArgs {
            name: "a".repeat(MAX_NAME_LEN + 1),
            command: "true".to_string(),
        })
        .is_err()
    );
    assert!(
        validate_start_args(&StartArgs {
            name: "監視1".to_string(),
            command: "true".to_string(),
        })
        .is_ok()
    );
    assert!(
        validate_start_args(&StartArgs {
            name: "valid".to_string(),
            command: "  ".to_string(),
        })
        .is_err()
    );
}

#[tokio::test]
async fn manager_rejects_duplicate_names_and_cleans_by_instance() {
    let manager = MonitorManager::default();
    let (instance, _, _) = manager
        .reserve("watch".to_string(), "true".to_string())
        .await
        .expect("first reservation");
    assert!(
        manager
            .reserve("watch".to_string(), "false".to_string())
            .await
            .is_err()
    );
    assert!(manager.activate("watch", instance, 1234).await);
    assert!(
        manager
            .remove_matching("watch", instance + 1)
            .await
            .is_none()
    );
    assert!(manager.remove_matching("watch", instance).await.is_some());
}
