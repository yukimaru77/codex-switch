use super::*;

#[test]
fn lifecycle_notifications_have_distinct_presentations() {
    for (kind, expected_prefix, is_error) in [
        ("output", "Monitor event", false),
        ("completed", "Monitor stream ended", false),
        ("failed", "Monitor failed", true),
        ("timed_out", "Monitor timed out", true),
        ("cancelled", "Monitor cancelled", false),
    ] {
        let actual = presentation(
            &codex_app_server_protocol::MonitorNotificationNotification {
                thread_id: "thread-1".to_string(),
                monitor_name: "tests".to_string(),
                summary: "changed".to_string(),
                kind: kind.to_string(),
            },
        );
        assert_eq!(actual.title, format!("{expected_prefix}: \"tests\""));
        assert_eq!(actual.output_lines, vec![Line::from("changed")]);
        assert_eq!(actual.is_error, is_error);
    }
}

#[test]
fn monitor_output_is_split_into_terminal_safe_lines() {
    let notification = codex_app_server_protocol::MonitorNotificationNotification {
        thread_id: "thread-1".to_string(),
        monitor_name: "tests".to_string(),
        summary: "first\n\u{1b}[31msecond\u{1b}[0m".to_string(),
        kind: "output".to_string(),
    };
    let actual = presentation(&notification);

    let text = actual
        .output_lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    assert_eq!(text, vec!["first", "second"]);
    assert!(
        text.iter()
            .all(|line| !line.contains('\n') && !line.contains('\u{1b}'))
    );

    let rendered = notification_lines(&notification)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r#"
    • Monitor event: "tests"
      first
      second
    "#);
}
