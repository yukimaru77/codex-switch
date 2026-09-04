use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use codex_protocol::protocol::MonitorEvent;
use codex_protocol::protocol::MonitorEventKind;
use codex_protocol::protocol::MonitorWakePolicy;
use codex_utils_string::take_bytes_at_char_boundary;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::WriteStdinRequest;

use super::InstanceId;
use super::MAX_EVENT_BYTES;
use super::OUTPUT_POLL_MS;

const MAX_BATCH_WINDOW: Duration = Duration::from_secs(2);

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_monitor(
    session: Weak<Session>,
    step_context: Arc<StepContext>,
    call_id: String,
    name: String,
    instance_id: InstanceId,
    process_id: i32,
    cancel: CancellationToken,
    stopped: Arc<Notify>,
    initial_output: Vec<u8>,
) {
    let mut batch = OutputBatch::default();
    batch.push(&initial_output);
    let mut batch_started = (!batch.is_empty()).then(tokio::time::Instant::now);
    let mut final_state = None;

    while final_state.is_none() {
        let Some(session_ref) = session.upgrade() else {
            stopped.notify_waiters();
            return;
        };
        let context = UnifiedExecContext::new(
            Arc::clone(&session_ref),
            Arc::clone(&step_context),
            cancel.clone(),
            call_id.clone(),
        );
        let result = session_ref
            .services
            .unified_exec_manager
            .poll_process(
                &context,
                WriteStdinRequest {
                    process_id,
                    input: "",
                    yield_time_ms: OUTPUT_POLL_MS,
                    max_output_tokens: Some(MAX_EVENT_BYTES / 4),
                    truncation_policy: step_context.turn.model_info().truncation_policy.into(),
                    interaction_event: None,
                },
            )
            .await;

        match result {
            Ok(output) => {
                let received_output = !output.raw_output.is_empty();
                if received_output {
                    if batch_started.is_none() {
                        batch_started = Some(tokio::time::Instant::now());
                    }
                    batch.push(&output.raw_output);
                }

                let hard_deadline_reached =
                    batch_started.is_some_and(|started| started.elapsed() >= MAX_BATCH_WINDOW);
                if !batch.is_empty()
                    && (!received_output || hard_deadline_reached || batch.is_full())
                {
                    emit_output(
                        &session_ref,
                        &name,
                        instance_id,
                        batch.take_summary().as_bytes(),
                    )
                    .await;
                    batch_started = None;
                }

                if output.process_id.is_none() {
                    final_state = Some((output.exit_code.unwrap_or(-1), None));
                }
            }
            Err(error) => {
                final_state = Some((-1, Some(format!("{error:?}"))));
            }
        }
    }

    let Some(session_ref) = session.upgrade() else {
        stopped.notify_waiters();
        return;
    };
    if !batch.is_empty() {
        emit_output(
            &session_ref,
            &name,
            instance_id,
            batch.take_summary().as_bytes(),
        )
        .await;
    }
    let (exit_code, error) = final_state.unwrap_or((-1, None));
    let removed = session_ref
        .services
        .monitor_manager
        .remove_matching(&name, instance_id)
        .await
        .is_some();
    if removed {
        emit_exit(
            &session_ref,
            &name,
            instance_id,
            exit_code,
            cancel.is_cancelled(),
            error,
        )
        .await;
    }
    stopped.notify_waiters();
}

pub(super) async fn emit_output(
    session: &Arc<Session>,
    name: &str,
    instance_id: InstanceId,
    output: &[u8],
) {
    if output.is_empty() {
        return;
    }
    let summary = String::from_utf8_lossy(output).into_owned();
    emit_event(
        session,
        name,
        instance_id,
        MonitorEventKind::OutputBatch,
        summary,
    )
    .await;
}

pub(super) async fn emit_exit(
    session: &Arc<Session>,
    name: &str,
    instance_id: InstanceId,
    exit_code: i32,
    cancelled: bool,
    error: Option<String>,
) {
    let (kind, summary) = if cancelled {
        (
            MonitorEventKind::Cancelled,
            format!("Monitor '{name}' was stopped."),
        )
    } else if exit_code == 0 {
        (
            MonitorEventKind::Completed { exit_code },
            format!("Monitor '{name}' completed successfully."),
        )
    } else {
        let error = error.map(|error| format!(" {error}")).unwrap_or_default();
        (
            MonitorEventKind::Failed {
                exit_code,
                stderr_tail: None,
            },
            format!("Monitor '{name}' failed with exit code {exit_code}.{error}"),
        )
    };
    emit_event(session, name, instance_id, kind, summary).await;
}

async fn emit_event(
    session: &Arc<Session>,
    name: &str,
    instance_id: InstanceId,
    kind: MonitorEventKind,
    summary: String,
) {
    let sequence = session.services.monitor_manager.next_sequence();
    let event = MonitorEvent {
        id: format!("mon-{name}-{instance_id}-{sequence}"),
        monitor_name: name.to_string(),
        sequence,
        kind,
        summary,
        wake_policy: MonitorWakePolicy::AttachOrWake,
    };
    crate::session::monitor_event(session, crate::session::new_submission_id(), event).await;
}

#[derive(Default)]
struct OutputBatch {
    bytes: Vec<u8>,
    omitted: usize,
}

impl OutputBatch {
    fn push(&mut self, chunk: &[u8]) {
        let remaining = MAX_EVENT_BYTES.saturating_sub(self.bytes.len());
        let retained = remaining.min(chunk.len());
        self.bytes.extend_from_slice(&chunk[..retained]);
        self.omitted = self.omitted.saturating_add(chunk.len() - retained);
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty() && self.omitted == 0
    }

    fn is_full(&self) -> bool {
        self.bytes.len() == MAX_EVENT_BYTES
    }

    fn take_summary(&mut self) -> String {
        let bytes = std::mem::take(&mut self.bytes);
        let omitted = std::mem::take(&mut self.omitted);
        let text = String::from_utf8_lossy(&bytes);
        if omitted == 0 {
            return take_bytes_at_char_boundary(&text, MAX_EVENT_BYTES).to_string();
        }
        let marker = format!("\n... {omitted} bytes omitted ...");
        let text_budget = MAX_EVENT_BYTES.saturating_sub(marker.len());
        format!(
            "{}{}",
            take_bytes_at_char_boundary(&text, text_budget),
            marker
        )
    }
}

#[cfg(test)]
#[path = "monitor_runtime_tests.rs"]
mod tests;
