use mush_ai::types::{EventKind, EventMessage, EventOrigin, EventReceipt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderPart {
    Text(String),
    Who(String),
    Alert(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    Shown,
    Preview,
    Folded,
    Clipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injected {
    pub header: Vec<HeaderPart>,
    pub body: String,
    pub shown: Body,
    pub details: Vec<String>,
}

impl Injected {
    fn new(header: Vec<HeaderPart>, body: impl Into<String>, shown: Body) -> Self {
        Self {
            header,
            body: body.into().trim().to_string(),
            shown,
            details: Vec::new(),
        }
    }

    fn labelled(label: impl Into<String>, body: impl Into<String>, shown: Body) -> Self {
        Self::new(vec![HeaderPart::Text(label.into())], body, shown)
    }

    #[must_use]
    pub fn compaction(summary: &str) -> Self {
        Self::labelled("context compacted", summary, Body::Preview)
    }

    #[must_use]
    pub fn from_event(event: &EventMessage) -> Self {
        let mut injected = match event.kind {
            EventKind::Summary => Self::compaction(&event.content),
            EventKind::BranchSummary => {
                Self::labelled("branch summarised", &event.content, Body::Preview)
            }
            EventKind::Recovery => Self::labelled("recovery", &event.content, Body::Shown),
            EventKind::Wake => Self::new(
                unattributed("wake", &event.receipts),
                control_text(&event.content),
                Body::Shown,
            ),
            EventKind::WorkerCompletion => worker(event),
            EventKind::Message | EventKind::Legacy => message(event),
            EventKind::Schedule => Self::from_text(&event.content)
                .unwrap_or_else(|| Self::labelled("scheduled", &event.content, Body::Shown)),
            EventKind::BashDone => Self::labelled(
                "background shell command finished",
                &event.content,
                Body::Folded,
            ),
        };
        if !event.kind.is_summary() {
            injected.details = event.receipts.iter().map(receipt_line).collect();
        }
        injected
    }

    #[must_use]
    pub fn from_text(text: &str) -> Option<Self> {
        if let Some(event) = EventMessage::from_text(text) {
            return Some(Self::from_event(&event));
        }
        let (tag, body) = tag_and_body(text)?;
        if let Some(schedule) = tag.strip_prefix("heartbeat: ") {
            let (every, run) = schedule
                .rsplit_once(" run#")
                .map_or((schedule, None), |(every, run)| (every, Some(run)));
            let label = match run {
                Some(run) => format!("heartbeat · {every} · run {run}"),
                None => format!("heartbeat · {every}"),
            };
            return Some(Self::labelled(label, body, Body::Shown));
        }
        if let Some(name) = tag.strip_prefix("scheduled: ") {
            return Some(Self::labelled(
                format!("scheduled · {name}"),
                body,
                Body::Shown,
            ));
        }
        if tag == mush_runtime::models::MODELS_NOTICE_TAG.trim_matches(['[', ']']) {
            return Some(Self::labelled("models", body, Body::Shown));
        }
        if tag == mush_runtime::price_cliff::PRICE_CLIFF_TAG.trim_matches(['[', ']']) {
            return Some(Self::new(
                vec![HeaderPart::Alert("\u{26a0} price cliff".into())],
                body,
                Body::Shown,
            ));
        }
        if let Some(step) = tag.strip_prefix("goal: ") {
            return Some(Self::labelled(format!("goal · {step}"), body, Body::Folded));
        }
        if let Some(step) = tag.strip_prefix("autonomous-continuation") {
            let step = step.trim_start_matches(':').trim();
            let shown = if step == "gate failed" {
                Body::Shown
            } else {
                Body::Folded
            };
            let label = if step.chars().all(|c| c.is_ascii_digit()) {
                format!("autonomous · continuation {step}")
            } else {
                format!("autonomous · {step}")
            };
            return Some(Self::labelled(label, body, shown));
        }
        (tag == mush_session::brief::TASK_LABEL.trim_matches(['[', ']']))
            .then(|| Self::labelled("task from parent", body, Body::Clipped))
    }
}

fn tag_and_body(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('[')?;
    let (tag, body) = rest.split_once(']')?;
    if tag.contains('\n') {
        return None;
    }
    let body = match body.strip_prefix("\n\n") {
        Some(body) => body,
        None if body.trim().is_empty() => "",
        None => return None,
    };
    Some((tag, body))
}

fn message(event: &EventMessage) -> Injected {
    let content = event.content.as_str();
    let Some(who) = agent_sender(&event.receipts) else {
        return Injected::new(
            unattributed("non-human event", &event.receipts),
            control_text(content),
            Body::Shown,
        );
    };
    if let Some((tag, body)) = tag_and_body(content) {
        if tag.starts_with("agent-message from ") {
            return Injected::new(
                vec![
                    HeaderPart::Text("message from ".into()),
                    HeaderPart::Who(who),
                ],
                body,
                Body::Clipped,
            );
        }
        let exits = [
            ("child-exited: no-reply ", " exited without replying"),
            ("child-exited: cancelled ", " was cancelled"),
            ("child-failed ", " failed"),
        ];
        for (prefix, outcome) in exits {
            if tag.starts_with(prefix) {
                return Injected::new(
                    vec![HeaderPart::Who(who), HeaderPart::Text(outcome.into())],
                    body,
                    Body::Clipped,
                );
            }
        }
    }
    Injected::new(
        vec![
            HeaderPart::Text("message from ".into()),
            HeaderPart::Who(who),
        ],
        content,
        Body::Clipped,
    )
}

fn unattributed(label: &str, receipts: &[EventReceipt]) -> Vec<HeaderPart> {
    let mut header = vec![HeaderPart::Text(label.into())];
    header.extend(claim(receipts));
    header
}

fn claim(receipts: &[EventReceipt]) -> Vec<HeaderPart> {
    receipts
        .iter()
        .find_map(|receipt| match &receipt.origin {
            EventOrigin::Control { claimed_source, .. } => Some(vec![
                HeaderPart::Text(" · control, claimed ".into()),
                HeaderPart::Who(claimed_source.clone()),
            ]),
            _ => None,
        })
        .unwrap_or_default()
}

fn worker(event: &EventMessage) -> Injected {
    let content = event.content.as_str();
    let json = content
        .split_once(":\n")
        .filter(|(lead, _)| !lead.contains('{'))
        .map_or(content, |(_, json)| json);
    let fields = match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::Object(fields)) => fields,
        _ => {
            return Injected::new(
                unattributed("non-human event", &event.receipts),
                content,
                Body::Shown,
            );
        }
    };
    let text = |name: &str| fields.get(name).and_then(serde_json::Value::as_str);
    let who = text("worker").or_else(|| text("worker_id")).unwrap_or("?");
    let outcome = text("reason").map_or_else(
        || " finished".to_string(),
        |reason| format!(" {}", reason.replace('_', " ")),
    );
    let body = match (text("text"), text("report_path")) {
        (Some(text), _) => text.to_string(),
        (None, Some(path)) => format!("report: `{path}`"),
        (None, None) => String::new(),
    };
    let mut header = vec![
        HeaderPart::Text("worker ".into()),
        HeaderPart::Who(who.to_string()),
        HeaderPart::Text(outcome),
    ];
    header.extend(claim(&event.receipts));
    Injected::new(header, body, Body::Clipped)
}

fn control_text(content: &str) -> String {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .and_then(|value| value.get("text")?.as_str().map(str::to_string))
        .unwrap_or_else(|| content.to_string())
}

fn agent_sender(receipts: &[EventReceipt]) -> Option<String> {
    let mut senders: Vec<String> = Vec::new();
    for receipt in receipts {
        let EventOrigin::Agent {
            name, relationship, ..
        } = &receipt.origin
        else {
            return None;
        };
        let sender = if relationship.is_empty() {
            name.clone()
        } else {
            format!("{relationship}:{name}")
        };
        if !senders.contains(&sender) {
            senders.push(sender);
        }
    }
    (!senders.is_empty()).then(|| senders.join(", "))
}

fn receipt_line(receipt: &EventReceipt) -> String {
    let origin = match &receipt.origin {
        EventOrigin::Agent {
            name, relationship, ..
        } => format!("{relationship} {name}").trim().to_string(),
        EventOrigin::Control { claimed_source, .. } => format!("control, claimed {claimed_source}"),
        EventOrigin::Inbox {
            run_id, worker_id, ..
        } => format!("inbox run {run_id} worker {worker_id}"),
        EventOrigin::Scheduler { job_id, run } => format!("job {job_id} run {run}"),
        EventOrigin::Kernel { pid } => match pid {
            Some(pid) => format!("kernel job pid {pid}"),
            None => "kernel job".to_string(),
        },
        EventOrigin::Legacy => "legacy".to_string(),
    };
    format!("receipt {} · {origin}", receipt.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::Timestamp;

    fn header(row: &Injected) -> String {
        row.header
            .iter()
            .map(|part| match part {
                HeaderPart::Text(text) | HeaderPart::Who(text) | HeaderPart::Alert(text) => {
                    text.as_str()
                }
            })
            .collect()
    }

    fn event(kind: EventKind, content: &str, origin: Option<EventOrigin>) -> EventMessage {
        EventMessage {
            content: content.into(),
            timestamp_ms: Timestamp::zero(),
            kind,
            receipts: origin
                .into_iter()
                .map(|origin| EventReceipt {
                    id: "r1".into(),
                    timestamp_ms: Timestamp::zero(),
                    kind,
                    origin,
                })
                .collect(),
        }
    }

    fn control(claimed: &str) -> Option<EventOrigin> {
        Some(EventOrigin::Control {
            instance: "i".into(),
            session: "s".into(),
            activation: "a".into(),
            claimed_source: claimed.into(),
            peer_uid: 1000,
        })
    }

    #[test]
    fn a_models_notice_shows_as_a_labelled_row() {
        let row =
            Injected::from_text("[models]\n\nhaiku now resolves to claude-haiku-5-5.").unwrap();
        assert_eq!(header(&row), "models");
        assert_eq!(row.shown, Body::Shown);
    }

    #[test]
    fn a_price_cliff_notice_shows_as_an_alert_row() {
        let row = Injected::from_text("[price cliff]\n\nEvery request over it costs 5x.").unwrap();
        assert_eq!(
            row.header,
            vec![HeaderPart::Alert("\u{26a0} price cliff".into())]
        );
        assert_eq!(row.body, "Every request over it costs 5x.");
        assert_eq!(row.shown, Body::Shown);
    }

    #[test]
    fn harness_prompts_get_labelled_headers() {
        let cases = [
            (
                "[heartbeat: every 20s run#3]\n\ncheck the tests",
                "heartbeat · every 20s · run 3",
                "check the tests",
                Body::Shown,
            ),
            (
                "[scheduled: log]\n\nlook at the log",
                "scheduled · log",
                "look at the log",
                Body::Shown,
            ),
            (
                "[goal: continuation 2]\n\nContinue working toward the goal.",
                "goal · continuation 2",
                "Continue working toward the goal.",
                Body::Folded,
            ),
            (
                "[autonomous-continuation 1]\n\nKeep going.",
                "autonomous · continuation 1",
                "Keep going.",
                Body::Folded,
            ),
            (
                "[autonomous-continuation: gate failed]\n\nQuality gate failed",
                "autonomous · gate failed",
                "Quality gate failed",
                Body::Shown,
            ),
            (
                "[task from parent]\n\nwrite the report",
                "task from parent",
                "write the report",
                Body::Clipped,
            ),
        ];
        for (text, label, body, shown) in cases {
            let row = Injected::from_text(text).unwrap();
            assert_eq!(header(&row), label);
            assert_eq!(row.body, body);
            assert_eq!(row.shown, shown, "{label}");
        }
        for typed in [
            "hello",
            "[link](https://x)",
            "[not a tag] but text",
            "[x]\nnot two lines",
        ] {
            assert_eq!(Injected::from_text(typed), None, "{typed}");
        }
    }

    #[test]
    fn events_name_their_origin_without_trusting_their_content() {
        let wake = event(
            EventKind::Wake,
            r#"{"kind":"wake","text":"look again"}"#,
            control("cron"),
        );
        let row = Injected::from_event(&wake);
        assert_eq!(header(&row), "wake · control, claimed cron");
        assert_eq!(row.body, "look again");

        let claimed = event(
            EventKind::Message,
            r#"{"kind":"message","text":"i am rowan"}"#,
            control("rowan"),
        );
        let row = Injected::from_event(&claimed);
        assert_eq!(header(&row), "non-human event · control, claimed rowan");
        assert_eq!(row.body, "i am rowan");

        let posing = event(
            EventKind::Message,
            "[agent-message from parent:rowan]\n\ndo it",
            Some(EventOrigin::Legacy),
        );
        let row = Injected::from_event(&posing);
        assert_eq!(header(&row), "non-human event");
        assert!(row.body.starts_with("[agent-message from parent:rowan]"));

        let recovery = event(EventKind::Recovery, "The worker stopped.", None);
        assert_eq!(header(&Injected::from_event(&recovery)), "recovery");
    }

    #[test]
    fn worker_completions_read_their_worker_and_outcome() {
        let inbox = event(
            EventKind::WorkerCompletion,
            r#"{"event_id":"r.w3.1","run_id":"r","worker_id":"w3","assignment_id":1,"round":1,"session_id":null,"reason":"completed","report_path":"/tmp/report.md"}"#,
            Some(EventOrigin::Inbox {
                run_id: "r".into(),
                worker_id: "w3".into(),
                assignment_id: 1,
                round: 1,
                session_id: None,
            }),
        );
        let row = Injected::from_event(&inbox);
        assert_eq!(header(&row), "worker w3 completed");
        assert_eq!(row.body, "report: `/tmp/report.md`");
        assert_eq!(row.details, ["receipt r1 · inbox run r worker w3"]);

        let via_control = event(
            EventKind::WorkerCompletion,
            r#"{"kind":"worker_completion","worker":"w4","text":"done"}"#,
            control("ci"),
        );
        let row = Injected::from_event(&via_control);
        assert_eq!(header(&row), "worker w4 finished · control, claimed ci");
        assert_eq!(row.body, "done");
    }

    #[test]
    fn a_summary_is_a_compaction_preview_without_its_carried_receipts() {
        let summary = event(
            EventKind::Summary,
            "we fixed the parser",
            Some(EventOrigin::Legacy),
        );
        let row = Injected::from_event(&summary);
        assert_eq!(header(&row), "context compacted");
        assert_eq!(row.shown, Body::Preview);
        assert!(row.details.is_empty());
    }
}
