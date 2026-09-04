use codex_context_fragments::AnnotatedContent;
use codex_context_fragments::RenderedFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::MonitorEvent;

use super::ContextualUserFragment;

/// Model-visible context produced by a background monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MonitorEventFragment {
    event: MonitorEvent,
}

impl MonitorEventFragment {
    pub(crate) fn new(event: MonitorEvent) -> Self {
        Self {
            event: event.into_bounded(),
        }
    }
}

impl ContextualUserFragment for MonitorEventFragment {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("monitor.event".to_string())
    }

    fn role(&self) -> &'static str {
        "assistant"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn render_fragment(&self) -> RenderedFragment {
        RenderedFragment::new(
            self.role(),
            AnnotatedContent::new(
                ContentItem::OutputText {
                    text: self.render(),
                },
                self.content_kind(),
            ),
        )
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        let payload = serde_json::json!({
            "monitor": self.event.monitor_name,
            "event": self.event.kind_label(),
            "output": self.event.summary,
        });
        format!(
            "[SYSTEM NOTIFICATION - NOT USER INPUT]\n\
             This is an automated background-task event.\n\
             Monitor event payload (JSON): {payload}"
        )
    }
}

#[cfg(test)]
#[path = "monitor_event_tests.rs"]
mod tests;
