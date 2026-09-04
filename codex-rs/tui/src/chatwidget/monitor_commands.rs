use super::*;
use codex_ansi_escape::ansi_escape_line;

struct MonitorPresentation {
    title: String,
    output_lines: Vec<Line<'static>>,
    is_error: bool,
}

fn presentation(
    notification: &codex_app_server_protocol::MonitorNotificationNotification,
) -> MonitorPresentation {
    let prefix = match notification.kind.as_str() {
        "output" => "Monitor event",
        "completed" => "Monitor stream ended",
        "failed" => "Monitor failed",
        "timed_out" => "Monitor timed out",
        "cancelled" => "Monitor cancelled",
        _ => "Monitor",
    };
    MonitorPresentation {
        title: format!("{}: \"{}\"", prefix, notification.monitor_name),
        output_lines: notification.summary.lines().map(ansi_escape_line).collect(),
        is_error: matches!(notification.kind.as_str(), "failed" | "timed_out"),
    }
}

fn notification_lines(
    notification: &codex_app_server_protocol::MonitorNotificationNotification,
) -> Vec<Line<'static>> {
    let mut presentation = presentation(notification);
    let mut lines = Vec::with_capacity(presentation.output_lines.len() + 1);
    if presentation.is_error {
        lines.push(format!("■ {}", presentation.title).red().into());
    } else {
        lines.push(vec!["• ".dim(), presentation.title.into()].into());
    }
    for mut line in presentation.output_lines.drain(..) {
        line.spans.insert(0, "  ".into());
        lines.push(line);
    }
    lines
}

impl ChatWidget {
    pub(super) fn on_monitor_notification(
        &mut self,
        notification: codex_app_server_protocol::MonitorNotificationNotification,
    ) {
        self.add_plain_history_lines(notification_lines(&notification));
    }
}

#[cfg(test)]
#[path = "monitor_commands_tests.rs"]
mod tests;
