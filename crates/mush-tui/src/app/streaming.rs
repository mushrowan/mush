//! streaming ingestion and tool-call lifecycle
//!
//! everything that turns provider events into display state: text and
//! thinking deltas, the tool-call state machine (single and batched), and
//! the end-of-turn settle that folds usage and cost into the stats.

use super::*;

impl App {
    /// the assistant message that owns the in-flight turn's tool calls.
    /// queued steering bubbles always sit at the tail of `messages`, so a
    /// naive `last_mut()` would route tool start/end onto the user bubble
    /// when steering is pending. tool calls only ever belong to an
    /// assistant turn, so we walk backwards for the most recent assistant
    fn current_turn_message_mut(&mut self) -> Option<&mut DisplayMessage> {
        self.messages
            .iter_mut()
            .rfind(|m| m.role == MessageRole::Assistant)
    }

    /// start streaming a new assistant message
    pub fn start_streaming(&mut self) {
        self.stream.start();
        // don't reset the scroll position here: push_user_message() does it
        // when the user sends a message. resetting here would yank the user
        // to the bottom if streaming starts in other contexts (delegation,
        // resume, etc) while they're scrolled up
    }

    /// append text delta to the current stream
    pub fn push_text_delta(&mut self, delta: &str) {
        self.stream.push_text(delta);
    }

    /// append thinking delta to the current stream
    pub fn push_thinking_delta(&mut self, delta: &str) {
        self.stream.push_thinking(delta);
    }

    /// visible portion of streaming text (typewriter effect)
    pub fn visible_streaming_text(&self) -> &str {
        self.stream.visible_text()
    }

    /// visible streaming text length in chars, for tests and render metrics
    pub fn visible_streaming_text_len(&self) -> usize {
        self.stream.visible_text_len()
    }

    /// visible portion of streaming thinking (typewriter effect)
    pub fn visible_streaming_thinking(&self) -> &str {
        self.stream.visible_thinking()
    }

    /// accumulate streaming tool call arguments
    pub fn push_tool_args_delta(&mut self, delta: &str) {
        self.stream.tool_args.push_str(delta);
    }

    /// mark a tool as being executed
    pub fn start_tool(&mut self, tool_call_id: &ToolCallId, name: &str, summary: &str) {
        // new batch when no tools are currently running
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
        // attach to the current turn's assistant message rather than
        // messages.last_mut(): queued steering bubbles tail the list, and
        // we must not push tool calls onto a queued user bubble
        let batch_idx = self.current_tool_batch;
        if let Some(turn) = self.current_turn_message_mut() {
            turn.tool_calls.push(DisplayToolCall {
                name: name.to_string(),
                summary: summary.to_string(),
                status: ToolCallStatus::Running,
                output_preview: None,
                image_data: None,
                batch: batch_idx,
            });
        }
    }

    /// expand a batch tool into individual sub-tool display entries
    pub fn start_batch_tool(
        &mut self,
        tool_call_id: &ToolCallId,
        _summary: &str,
        sub_calls: &[(String, String)], // (tool_name, summary) per sub-call
    ) {
        self.current_tool_batch += 1;
        self.stream.tool_args.clear();
        // one live panel per sub-call, keyed by the synthetic exec id the
        // batch tool tags its sub-calls with, so each sub-call streams its
        // own output into its own panel instead of all landing in the last
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
        // individual display entries for message history. target the
        // assistant turn explicitly since queued steering bubbles tail the
        // list and must not receive tool calls
        let batch_idx = self.current_tool_batch;
        if let Some(turn) = self.current_turn_message_mut() {
            for (name, sub_summary) in sub_calls {
                turn.tool_calls.push(DisplayToolCall {
                    name: name.clone(),
                    summary: sub_summary.clone(),
                    status: ToolCallStatus::Running,
                    output_preview: None,
                    image_data: None,
                    batch: batch_idx,
                });
            }
        }
    }

    /// finish a batch tool, distributing results to individual sub-calls
    pub fn end_batch_tool(
        &mut self,
        tool_call_id: &ToolCallId,
        outcome: mush_ai::types::ToolOutcome,
        output: Option<&str>,
    ) {
        // parse output into per-sub-call sections
        let Some(text) = output else { return };
        let sections = parse_batch_output(text);

        // mark each sub-call's live panel (synthetic exec id) so the active
        // panel reflects per-sub-call outcomes until it's cleared next turn.
        // an empty section list means a batch-level failure (e.g. arg parse
        // error) with no per-call output: surface it on every sub-panel.
        let batch_error =
            (sections.is_empty() && !text.trim().is_empty()).then(|| truncate_output(text));
        for tool in &mut self.active_tools {
            let Some(idx) = mush_agent::exec_context::batch_sub_index(
                tool.tool_call_id.as_str(),
                tool_call_id.as_str(),
            ) else {
                continue;
            };
            tool.live_output = None;
            if let Some(ref err) = batch_error {
                tool.status = ToolCallStatus::from(outcome);
                tool.output = Some(err.clone());
            } else if let Some(section) = sections.get(idx) {
                tool.status = if section.is_error {
                    ToolCallStatus::Error
                } else {
                    ToolCallStatus::Done
                };
                tool.output =
                    (!section.content.is_empty()).then(|| truncate_output(&section.content));
            } else {
                tool.status = ToolCallStatus::Error;
                tool.output = Some("missing result in batch output".into());
            }
        }

        // distribute results to the message-history sub-calls (the
        // persistent display, rendered as boxes once they leave Running)
        if let Some(turn) = self.current_turn_message_mut() {
            // find the running batch sub-calls (they're the last N running entries)
            let running: Vec<usize> = turn
                .tool_calls
                .iter()
                .enumerate()
                .filter(|(_, tc)| tc.status == ToolCallStatus::Running)
                .map(|(i, _)| i)
                .collect();

            // batch-level error: the whole batch failed before producing
            // per-sub-call sections (e.g. BatchArgs parse error). surface
            // the real error text on every sub-call instead of hiding it
            // behind a "missing result" placeholder
            if sections.is_empty() && !text.trim().is_empty() {
                let preview = truncate_output(text);
                for &idx in &running {
                    let tc = &mut turn.tool_calls[idx];
                    tc.status = ToolCallStatus::Error;
                    tc.output_preview = Some(preview.clone());
                }
                return;
            }

            for (section_idx, section) in sections.iter().enumerate() {
                if let Some(&tc_idx) = running.get(section_idx) {
                    let tc = &mut turn.tool_calls[tc_idx];
                    tc.status = if section.is_error {
                        ToolCallStatus::Error
                    } else {
                        ToolCallStatus::Done
                    };
                    if !section.content.is_empty() {
                        tc.output_preview = Some(truncate_output(&section.content));
                    }
                }
            }
            // mark any remaining unmatched sub-calls as errors so partial
            // batch output can't silently render as a successful null tool
            // result in the message history
            for &idx in running.iter().skip(sections.len()) {
                let tc = &mut turn.tool_calls[idx];
                tc.status = ToolCallStatus::Error;
                tc.output_preview = Some("missing result in batch output".into());
            }
        }
    }

    /// mark a tool as done, with optional output preview
    pub fn end_tool(
        &mut self,
        tool_call_id: &ToolCallId,
        name: &str,
        outcome: mush_ai::types::ToolOutcome,
        output: Option<&str>,
        image_data: Option<Vec<u8>>,
    ) {
        let status = ToolCallStatus::from(outcome);
        // mark done in active_tools (panel persists until next turn)
        if let Some(tool) = self
            .active_tools
            .iter_mut()
            .find(|t| &t.tool_call_id == tool_call_id)
        {
            tool.status = status;
            tool.output = output.map(|o| preview_for_tool(name, o));
            tool.live_output = None;
        }
        if let Some(turn) = self.current_turn_message_mut()
            && let Some(tc) = turn.tool_calls.iter_mut().rfind(|t| t.name == name)
        {
            tc.status = status;
            tc.output_preview = output.map(|o| preview_for_tool(name, o));
            tc.image_data = image_data;
        }
    }

    /// update live output for an active tool, matched by id
    pub fn push_tool_output(&mut self, tool_call_id: &ToolCallId, output: &str) -> bool {
        self.push_tool_output_by_exec_id(tool_call_id.as_str(), output)
    }

    /// update live output for the active tool whose exec id matches. the
    /// exec id is the tool-call id for top-level calls, or a synthetic
    /// `{batch_id}#{index}` for batch sub-calls (so concurrent sub-calls
    /// each stream into their own panel). returns true if the panel changed.
    pub fn push_tool_output_by_exec_id(&mut self, exec_id: &str, output: &str) -> bool {
        if let Some(tool) = self
            .active_tools
            .iter_mut()
            .find(|t| t.tool_call_id.as_str() == exec_id)
        {
            if tool.live_output.as_deref() == Some(output) {
                return false;
            }
            tool.live_output = Some(output.to_string());
            return true;
        }
        false
    }

    /// finish streaming, create the assistant message
    pub fn finish_streaming(&mut self, usage: Option<Usage>, cost: Option<Dollars>) {
        let (text, thinking) = self.stream.take();

        let assistant_msg = DisplayMessage {
            thinking,
            thinking_expanded: self.thinking_display == ThinkingDisplay::Expanded,
            usage,
            cost,
            model_id: Some(self.model_id.clone()),
            ..DisplayMessage::new(MessageRole::Assistant, text.trim_start_matches('\n'))
        };

        // insert before any trailing queued (steering) messages so the
        // assistant reply appears above steering input in the message list
        let insert_pos = self
            .messages
            .iter()
            .rposition(|m| !m.queued)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.messages.insert(insert_pos, assistant_msg);
        // a viewport anchored inside the in-flight stream has to follow the
        // same content into its new home, otherwise the trailing block
        // shrinking out from under the anchor drops the view to the bottom
        if let ScrollPosition::Trailing { line } = self.scroll {
            self.scroll = ScrollPosition::Message {
                msg: insert_pos,
                line,
            };
        }

        if let Some(ref u) = usage {
            let prev_usage_snapshot = self.stats.prev_usage().copied();
            let prev_config_snapshot = self.stats.prev_call_config().cloned();
            let prev_context_snapshot = self.stats.context_tokens;
            let curr_config = CallConfig {
                model_id: self.model_id.to_string(),
                thinking_level: format!("{:?}", self.thinking_level),
                // effort is derived from (model, thinking_level) today; a
                // standalone effort field will land with the effort enum split
                effort: None,
                context_management: context_management_mode_for_model_id(
                    self.model_id.as_str(),
                    mush_ai::request_snapshot::last_used_context_management(),
                ),
                // populated below from the most recent request snapshot
                // taken by the provider (anthropic-only today)
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
                        // dump diagnostic to disk for post-mortem investigation
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
                            // expected bust from user-initiated config change:
                            // dump for forensics but don't alarm the user
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
        if self.is_scrolled_up() {
            self.navigation.has_unread = true;
        }
    }
}
