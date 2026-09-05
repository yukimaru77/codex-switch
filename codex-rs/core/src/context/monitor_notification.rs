use super::ContextualUserFragment;

/// A batch of output lines from a `monitor` watcher, delivered as a contextual
/// user fragment so it stays distinguishable from real user input.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MonitorNotification {
    pub(crate) description: String,
    pub(crate) body: String,
}

impl MonitorNotification {
    pub(crate) fn new(description: impl Into<String>, body: impl Into<String>) -> Self {
        fn bounded(mut text: String, limit: usize) -> String {
            if text.len() > limit {
                let mut end = limit - " [truncated]".len();
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push_str(" [truncated]");
            }
            text
        }
        Self {
            description: bounded(description.into(), 128),
            body: bounded(body.into(), 700),
        }
    }
}

impl ContextualUserFragment for MonitorNotification {
    fn content_kind(&self) -> codex_protocol::models::ContentItemKind {
        codex_protocol::models::ContentItemKind("monitor.notification".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<monitor_notification>", "</monitor_notification>")
    }

    fn body(&self) -> String {
        format!("\n[{}] {}\n", self.description, self.body)
    }
}
