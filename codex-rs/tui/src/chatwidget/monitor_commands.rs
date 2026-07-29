use super::*;

impl ChatWidget {
    pub(super) fn on_monitor_notification(
        &mut self,
        notification: codex_app_server_protocol::MonitorNotificationNotification,
    ) {
        let prefix = match notification.kind.as_str() {
            "output" => "Monitor event",
            "completed" => "Monitor stream ended",
            "failed" => "Monitor failed",
            "timed_out" => "Monitor timed out",
            "cancelled" => "Monitor cancelled",
            _ => "Monitor",
        };
        let message = format!("{}: \"{}\"", prefix, notification.monitor_name);
        if notification.kind == "output" {
            self.add_info_message(format!("{}\n{}", message, notification.summary), None);
        } else {
            self.add_info_message(message, None);
        }
        self.request_redraw();
    }

    pub(super) fn handle_monitor_command(&mut self, args: &str) {
        let parts: Vec<&str> = args.splitn(3, char::is_whitespace).collect();
        match parts.first().copied() {
            Some("start") => {
                if parts.len() < 3 {
                    self.add_error_message("Usage: /monitor start <name> <command>".to_string());
                    return;
                }
                let name = parts[1].to_string();
                let command = parts[2].to_string();
                let monitor_manager = &self.monitor_manager;
                let event_sender = self.app_event_tx.clone();
                let name_clone = name.clone();
                let command_clone = command.clone();

                tokio::spawn({
                    let monitors = monitor_manager.monitors.clone();
                    let seq_counter = monitor_manager.sequence_counter.clone();
                    async move {
                        let mgr = crate::monitor::MonitorManager {
                            monitors,
                            sequence_counter: seq_counter,
                        };
                        match mgr.start(name_clone, &command_clone, event_sender).await {
                            Ok(()) => {}
                            Err(e) => {
                                tracing::warn!("monitor start failed: {e}");
                            }
                        }
                    }
                });

                self.add_info_message(format!("Monitor '{name}' started: {command}"), None);
            }
            Some("stop") => {
                if parts.len() < 2 {
                    self.add_error_message("Usage: /monitor stop <name>".to_string());
                    return;
                }
                let name = parts[1].to_string();
                let monitors = self.monitor_manager.monitors.clone();
                let name_for_task = name.clone();

                tokio::spawn(async move {
                    let mut monitors = monitors.lock().await;
                    if let Some(mut handle) = monitors.remove(&name_for_task) {
                        handle.cancel.cancel();
                        let _ = handle.child.kill().await;
                    }
                });

                self.add_info_message(format!("Monitor '{name}' stopped."), None);
            }
            Some("status") => {
                self.add_info_message(
                    "Use /monitor start <name> <cmd> to start a monitor.".to_string(),
                    None,
                );
            }
            Some("emit") => {
                if parts.len() < 2 {
                    self.add_error_message("Usage: /monitor emit <message>".to_string());
                    return;
                }
                let message = args.strip_prefix("emit").unwrap_or("").trim().to_string();
                let event = codex_protocol::protocol::MonitorEvent {
                    id: format!("mon-manual-{}", uuid::Uuid::new_v4()),
                    monitor_name: "manual".to_string(),
                    sequence: 0,
                    kind: codex_protocol::protocol::MonitorEventKind::OutputBatch,
                    summary: message,
                    wake_policy: codex_protocol::protocol::MonitorWakePolicy::AttachOrWake,
                };
                self.submit_op(crate::app_command::AppCommand::MonitorEvent { event });
            }
            _ => {
                self.add_error_message("Usage: /monitor start|stop|status|emit <args>".to_string());
            }
        }
        self.request_redraw();
    }
}
