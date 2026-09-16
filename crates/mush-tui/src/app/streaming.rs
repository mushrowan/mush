use super::*;

impl App {
    fn current_turn_message_mut(&mut self) -> Option<crate::display_types::MessageMut<'_>> {
        self.messages
            .iter_mut()
            .rfind(|m| m.role == MessageRole::Assistant)
    }

    pub fn start_streaming(&mut self) {
        self.stream.start();
    }

    pub fn push_text_delta(&mut self, delta: &str) {
        self.stream.push_text(delta);
    }

    pub fn push_thinking_delta(&mut self, delta: &str) {
        self.stream.push_thinking(delta);
    }

    pub fn visible_streaming_text(&self) -> &str {
        self.stream.visible_text()
    }

    pub fn visible_streaming_text_len(&self) -> usize {
        self.stream.visible_text_len()
    }

    pub fn visible_streaming_thinking(&self) -> &str {
        self.stream.visible_thinking()
    }

    pub fn push_tool_args_delta(&mut self, delta: &str) {
        self.stream.tool_args.push_str(delta);
    }

    pub fn announce_tool_calls(&mut self, message: &mush_ai::types::AssistantMessage) {
        self.announced_tool_calls = message
            .content
            .iter()
            .filter_map(|part| match part {
                mush_ai::types::AssistantContentPart::ToolCall(tc) => Some(tc.clone()),
                _ => None,
            })
            .collect();
    }

    #[must_use]
    pub fn announced_tool_call(
        &self,
        tool_call_id: &ToolCallId,
    ) -> Option<&mush_ai::types::ToolCall> {
        self.announced_tool_calls
            .iter()
            .find(|tc| &tc.id == tool_call_id)
    }

    #[must_use]
    pub fn turn_shows_tool_call(&self, tool_call_id: &ToolCallId) -> bool {
        self.messages
            .iter()
            .rfind(|m| m.role == MessageRole::Assistant)
            .is_some_and(|turn| {
                turn.tool_calls.iter().any(|tc| {
                    &tc.tool_call_id == tool_call_id
                        || mush_agent::exec_context::batch_sub_index(
                            tc.tool_call_id.as_str(),
                            tool_call_id.as_str(),
                        )
                        .is_some()
                })
            })
    }

    pub fn start_tool(&mut self, tool_call_id: &ToolCallId, name: &str, summary: &str) {
        if !self.has_running_tool() {
            self.current_tool_batch += 1;
        }
        self.active_tools.push(ActiveToolState {
            tool_call_id: tool_call_id.clone(),
            name: name.to_string(),
            summary: summary.to_string(),
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        });
        self.stream.tool_args.clear();
        let batch_idx = self.current_tool_batch;
        if let Some(mut turn) = self.current_turn_message_mut() {
            turn.tool_calls.push(DisplayToolCall {
                tool_call_id: tool_call_id.clone(),
                name: name.to_string(),
                summary: summary.to_string(),
                status: ToolCallStatus::Running,
                output: None,
                image_data: None,
                batch: batch_idx,
            });
        }
    }

    pub fn start_batch_tool(
        &mut self,
        tool_call_id: &ToolCallId,
        _summary: &str,
        sub_calls: &[(String, String)],
    ) {
        self.current_tool_batch += 1;
        self.stream.tool_args.clear();
        for (i, (name, sub_summary)) in sub_calls.iter().enumerate() {
            self.active_tools.push(ActiveToolState {
                tool_call_id: ToolCallId::from(mush_agent::exec_context::batch_sub_exec_id(
                    tool_call_id.as_str(),
                    i,
                )),
                name: name.clone(),
                summary: sub_summary.clone(),
                live_output: None,
                status: ToolCallStatus::Running,
                output: None,
            });
        }
        let batch_idx = self.current_tool_batch;
        if let Some(mut turn) = self.current_turn_message_mut() {
            for (i, (name, sub_summary)) in sub_calls.iter().enumerate() {
                turn.tool_calls.push(DisplayToolCall {
                    tool_call_id: ToolCallId::from(mush_agent::exec_context::batch_sub_exec_id(
                        tool_call_id.as_str(),
                        i,
                    )),
                    name: name.clone(),
                    summary: sub_summary.clone(),
                    status: ToolCallStatus::Running,
                    output: None,
                    image_data: None,
                    batch: batch_idx,
                });
            }
        }
    }

    pub fn end_batch_tool(
        &mut self,
        tool_call_id: &ToolCallId,
        outcome: mush_ai::types::ToolOutcome,
        output: Option<&str>,
    ) {
        let text = output.unwrap_or_default();
        let sections = parse_batch_output(text);
        let batch_error =
            (sections.is_empty() && !text.trim().is_empty()).then(|| text.to_string());
        let resolve = |idx: usize| -> (ToolCallStatus, Option<String>) {
            if let Some(ref err) = batch_error {
                (ToolCallStatus::from(outcome), Some(err.clone()))
            } else if let Some(section) = sections.get(idx) {
                let status = if section.is_error {
                    ToolCallStatus::Error
                } else {
                    ToolCallStatus::Done
                };
                let output = (!section.content.is_empty()).then(|| section.content.clone());
                (status, output)
            } else {
                (
                    ToolCallStatus::Error,
                    Some("missing result in batch output".into()),
                )
            }
        };

        for tool in &mut self.active_tools {
            let Some(idx) = mush_agent::exec_context::batch_sub_index(
                tool.tool_call_id.as_str(),
                tool_call_id.as_str(),
            ) else {
                continue;
            };
            tool.live_output = None;
            (tool.status, tool.output) = resolve(idx);
        }

        if let Some(mut turn) = self.current_turn_message_mut() {
            for tc in &mut turn.tool_calls {
                let Some(idx) = mush_agent::exec_context::batch_sub_index(
                    tc.tool_call_id.as_str(),
                    tool_call_id.as_str(),
                ) else {
                    continue;
                };
                let (status, preview) = resolve(idx);
                tc.status = status;
                if preview.is_some() {
                    tc.output = preview;
                }
            }
        }
    }

    pub fn end_tool(
        &mut self,
        tool_call_id: &ToolCallId,
        name: &str,
        outcome: mush_ai::types::ToolOutcome,
        output: Option<&str>,
        image_data: Option<Vec<u8>>,
    ) {
        let status = ToolCallStatus::from(outcome);
        if let Some(tool) = self
            .active_tools
            .iter_mut()
            .find(|t| &t.tool_call_id == tool_call_id)
        {
            tool.status = status;
            tool.output = output.map(|o| preview_for_tool(name, o));
            tool.live_output = None;
        }
        if let Some(mut turn) = self.current_turn_message_mut()
            && let Some(tc) = turn
                .tool_calls
                .iter_mut()
                .find(|t| &t.tool_call_id == tool_call_id)
        {
            tc.status = status;
            tc.output = output.map(str::to_string);
            tc.image_data = image_data;
        }
    }

    pub fn push_tool_output(&mut self, tool_call_id: &ToolCallId, output: &str) -> bool {
        self.push_tool_output_by_exec_id(tool_call_id.as_str(), output)
    }

    pub fn push_tool_output_by_exec_id(&mut self, exec_id: &str, output: &str) -> bool {
        let Some(tool) = self
            .active_tools
            .iter_mut()
            .find(|t| t.tool_call_id.as_str() == exec_id)
        else {
            return false;
        };
        if tool.live_output.as_deref() == Some(output) {
            return false;
        }
        tool.live_output = Some(output.to_string());
        if let Some(mut turn) = self.current_turn_message_mut()
            && let Some(call) = turn
                .tool_calls
                .iter_mut()
                .find(|c| c.tool_call_id.as_str() == exec_id)
        {
            call.output = Some(output.to_string());
        }
        true
    }

    pub fn finish_streaming(&mut self, usage: Option<Usage>, cost: Option<Dollars>) {
        let (text, thinking) = self.stream.take();

        let assistant_msg = DisplayMessage {
            thinking,
            thinking_expanded: self.thinking_display == ThinkingDisplay::Expanded,
            usage,
            cost,
            model_id: Some(self.model_id.clone()),
            ..DisplayMessage::new(MessageRole::Assistant, &text)
        };

        let insert_pos = self
            .messages
            .iter()
            .rposition(|m| !m.queued)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.messages.insert(insert_pos, assistant_msg);
        if let Some(ref u) = usage {
            let prev_usage_snapshot = self.stats.prev_usage().copied();
            let prev_config_snapshot = self.stats.prev_call_config().cloned();
            let prev_context_snapshot = self.stats.context_tokens;
            let curr_config = CallConfig {
                model_id: self.model_id.to_string(),
                thinking_level: format!("{:?}", self.thinking_level),
                effort: None,
                context_management: context_management_mode_for_model_id(
                    self.model_id.as_str(),
                    mush_ai::request_snapshot::last_used_context_management(),
                ),
                messages_prefix_fingerprint:
                    mush_ai::request_snapshot::last_messages_prefix_fingerprint(),
            };
            let anomalies = self
                .stats
                .update_with_config(u, cost, Some(curr_config.clone()));
            for anomaly in &anomalies {
                let msg_opt: Option<String> = match anomaly {
                    CacheAnomaly::ContextDecrease { prev, curr } => Some(format!(
                        "⚠ context decreased: {}k → {}k (delta -{}k) without compact",
                        prev.get() / 1000,
                        curr.get() / 1000,
                        (prev.get() - curr.get()) / 1000,
                    )),
                    CacheAnomaly::CacheBust {
                        prev_cache_read,
                        curr_cache_read,
                        curr_cache_write,
                        reason,
                    } => {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let diag =
                            CacheBustDiagnostic {
                                timestamp: format!("{now}"),
                                secs_since_last_cache_activity: self.cache.elapsed_secs(),
                                cache_ttl_secs: self.cache.ttl_secs,
                                thinking_level: curr_config.thinking_level.clone(),
                                model_id: curr_config.model_id.clone(),
                                effort: curr_config.effort.clone(),
                                bust_reason: reason.clone(),
                                prev_model_id: prev_config_snapshot
                                    .as_ref()
                                    .map(|c| c.model_id.clone()),
                                prev_thinking_level: prev_config_snapshot
                                    .as_ref()
                                    .map(|c| c.thinking_level.clone()),
                                prev_effort: prev_config_snapshot
                                    .as_ref()
                                    .and_then(|c| c.effort.clone()),
                                prev_messages_prefix_fingerprint: prev_config_snapshot
                                    .as_ref()
                                    .and_then(|c| c.messages_prefix_fingerprint),
                                curr_messages_prefix_fingerprint: curr_config
                                    .messages_prefix_fingerprint,
                                prev_usage: prev_usage_snapshot,
                                curr_usage: *u,
                                prev_context_tokens: prev_context_snapshot.get(),
                                curr_context_tokens: u.total_input_tokens().get(),
                                session_total_cost: format!("{:.4}", self.stats.total_cost.get()),
                                session_api_calls: self.stats.total_tokens.get()
                                    / u.total_tokens().get().max(1),
                                recent_request_snapshots:
                                    mush_ai::request_snapshot::recent_snapshots("anthropic", 2),
                            };
                        let dump_note = match dump_cache_bust_diagnostic(&diag) {
                            Some(path) => format!(" (dump: {})", path.display()),
                            None => String::new(),
                        };

                        match reason {
                            BustReason::Unexplained => Some(format!(
                                "⚠ probable cache bust: cache_read {}k → {}k, cache_write {}k (prefix evicted){dump_note}",
                                prev_cache_read.get() / 1000,
                                curr_cache_read.get() / 1000,
                                curr_cache_write.get() / 1000,
                            )),
                            _ => None,
                        }
                    }
                };
                if let Some(msg) = msg_opt {
                    self.push_system_message(msg);
                }
            }
        } else if let Some(c) = cost {
            self.stats.total_cost += c;
        }
    }
}
