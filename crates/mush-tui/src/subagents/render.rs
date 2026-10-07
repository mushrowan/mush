use chrono::{DateTime, Utc};
use mush_supervisor::status::{RootStatus, RunState, Snapshot, TaskStatus, safe_text};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::SubagentPanel;
use crate::theme::Theme;

pub fn render(frame: &mut Frame, panel: &SubagentPanel, theme: &Theme, now: DateTime<Utc>) {
    let screen = frame.area();
    let width = screen
        .width
        .saturating_sub(u16::from(screen.width > 4) * 4)
        .min(112);
    let height = screen
        .height
        .saturating_sub(u16::from(screen.height > 4) * 2)
        .min(32);
    let area = Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" subagents ")
        .border_style(theme.picker_title)
        .style(theme.tool_panel_bg);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let tasks: Vec<_> = panel
        .snapshot
        .roots
        .iter()
        .enumerate()
        .flat_map(|(i, r)| r.tasks.iter().map(move |t| (i, r, t)))
        .collect();
    let notices = stale_notices(&panel.snapshot);
    let mut y = inner.y;
    if !notices.is_empty() {
        frame.render_widget(
            Paragraph::new(notices.join(" | ")).style(theme.alert),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
    }
    let remaining = inner.bottom().saturating_sub(y);
    if tasks.is_empty() {
        let empty = if panel.snapshot.roots.is_empty() {
            "no supervisors registered\n/subagents add /absolute/run\n/subagents shows registration help"
        } else {
            "no workers available"
        };
        frame.render_widget(
            Paragraph::new(empty)
                .style(theme.dim)
                .wrap(Wrap { trim: false }),
            Rect::new(inner.x, y, inner.width, remaining),
        );
        return;
    }
    let selected = panel.selected.min(tasks.len() - 1);
    let body = Rect::new(inner.x, y, inner.width, remaining);
    if panel.details {
        render_details(frame, panel, theme, now, tasks[selected], body);
    } else {
        render_list(frame, panel, theme, now, &tasks, selected, body);
    }
}

type Entry<'a> = (usize, &'a RootStatus, &'a TaskStatus);

fn render_details(
    frame: &mut Frame,
    panel: &SubagentPanel,
    theme: &Theme,
    now: DateTime<Utc>,
    (root_index, root, task): Entry<'_>,
    body: Rect,
) {
    let review = task
        .review_updated_at
        .map(|at| at.to_rfc3339())
        .unwrap_or_else(|| "missing".into());
    let queue = review_queue_label(root);
    let older = predates_worker_suffix(task);
    let mut lines = vec![
        Line::styled(safe_text(&task.name), theme.picker_title),
        Line::raw(format!(
            "worker {} | root {} | round {}",
            safe_text(&task.worker),
            root_index + 1,
            task.round
        )),
        Line::raw(format!(
            "run {} | recorded {} | {}",
            task.run.label(),
            safe_text(&task.recorded_state),
            age(task, now)
        )),
        Line::styled("liveness unverified", theme.alert),
        Line::raw(activity(task)),
        Line::raw(safe_text(
            task.headless_activity
                .as_deref()
                .unwrap_or("headless activity unavailable"),
        )),
        Line::raw(format!("{}{older}", task.review.label())),
        Line::raw(format!("review update {review}")),
        Line::raw(safe_text(&task.review_detail)),
        Line::raw(format!(
            "source {}",
            safe_text(&root.source.directory.to_string_lossy())
        )),
        Line::raw(format!("operator_queue_v1 {queue}")),
    ];
    if let Some(error) = task.read_error {
        lines.insert(0, Line::styled(format!("stale / {error}"), theme.alert));
    }
    if let Some(supervisor) = &root.supervisor {
        let pid = supervisor
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "missing".into());
        let updated = supervisor
            .finished_at
            .or(supervisor.started_at)
            .map(|at| at.to_rfc3339())
            .unwrap_or_else(|| "unknown update".into());
        lines.push(Line::raw(format!(
            "supervisor record {} / pid {pid} / {updated} / liveness unverified",
            safe_text(&supervisor.state)
        )));
    }
    if let Some(error) = task.headless_error {
        lines.insert(
            0,
            Line::styled(format!("headless stale / {error}"), theme.alert),
        );
    }
    if task.record_is_old(now) {
        lines.insert(0, Line::styled("old record", theme.alert));
    }
    let content = Paragraph::new(lines)
        .style(theme.tool_output)
        .wrap(Wrap { trim: false });
    let content_height = body.height.saturating_sub(1);
    panel.visible_rows.set(usize::from(content_height));
    let max_scroll = content
        .line_count(body.width)
        .saturating_sub(usize::from(content_height));
    frame.render_widget(
        content.scroll((panel.detail_scroll.min(max_scroll as u16), 0)),
        Rect::new(body.x, body.y, body.width, content_height),
    );
    if body.height > 0 {
        frame.render_widget(
            Paragraph::new("enter list | arrows scroll | esc closes").style(theme.dim),
            Rect::new(body.x, body.bottom() - 1, body.width, 1),
        );
    }
}

fn render_list(
    frame: &mut Frame,
    panel: &SubagentPanel,
    theme: &Theme,
    now: DateTime<Utc>,
    tasks: &[Entry<'_>],
    selected: usize,
    body: Rect,
) {
    let mut y = body.y;
    let detail_height = if body.height >= 10 { 6 } else { 0 };
    let footer_height = u16::from(body.height >= 3);
    let list_height = body.height.saturating_sub(detail_height + footer_height);
    let row_height = if body.width < 65 && list_height >= 3 {
        3
    } else if list_height >= 2 {
        2
    } else {
        1
    };
    let visible = usize::from(list_height / row_height).max(1);
    panel.visible_rows.set(visible);
    let start = selected.saturating_sub(visible - 1);
    for (index, (root_index, root, task)) in tasks.iter().enumerate().skip(start).take(visible) {
        if y >= body.bottom() {
            break;
        }
        let marker = if index == selected { ">" } else { " " };
        let stale = task.read_error.is_some()
            || root.read_error.is_some()
            || panel.snapshot.registration_error.is_some();
        let status = if stale {
            " stale/read-error"
        } else if task.record_is_old(now) {
            " old record"
        } else {
            ""
        };
        let review_stale = if root.review_error.is_some() {
            " stale"
        } else if task.review_predates_worker() {
            " older"
        } else {
            ""
        };
        let style = if index == selected {
            theme.picker_selected
        } else {
            run_style(theme, task.run)
        };
        let name = crate::text::truncate_with_ellipsis(
            &safe_text(&task.name),
            usize::from(body.width / 3).max(4),
        );
        let line = if row_height == 3 {
            Line::styled(
                format!(
                    "{marker} {} {name} {}{status}",
                    root_index + 1,
                    age(task, now)
                ),
                style,
            )
        } else {
            Line::from(vec![
                Span::styled(format!("{marker} {name} "), style),
                Span::styled(
                    format!(
                        "{} | {}{review_stale} | {}{status}",
                        task.run.label(),
                        task.review.label(),
                        age(task, now)
                    ),
                    style,
                ),
            ])
        };
        frame.render_widget(Paragraph::new(line), Rect::new(body.x, y, body.width, 1));
        y += 1;
        if row_height == 3 && y < body.bottom() {
            frame.render_widget(
                Paragraph::new(format!(
                    " {} | {}{review_stale}",
                    task.run.label(),
                    task.review.label()
                ))
                .style(style),
                Rect::new(body.x, y, body.width, 1),
            );
            y += 1;
        }
        if row_height >= 2 && y < body.bottom() {
            frame.render_widget(
                Paragraph::new(activity(task)).style(theme.dim),
                Rect::new(body.x + 1, y, body.width.saturating_sub(1), 1),
            );
            y += 1;
        }
    }
    if detail_height > 0 {
        let detail = Rect::new(
            body.x,
            body.bottom().saturating_sub(detail_height + footer_height),
            body.width,
            detail_height,
        );
        frame.render_widget(
            Paragraph::new(selected_summary(theme, now, tasks[selected])).style(theme.tool_output),
            detail,
        );
    }
    if footer_height > 0 {
        frame.render_widget(
            Paragraph::new(format!(
                "{}/{} | arrows / page keys | enter details | esc closes",
                selected + 1,
                tasks.len()
            ))
            .style(theme.dim),
            Rect::new(body.x, body.bottom() - 1, body.width, 1),
        );
    }
}

fn selected_summary(
    theme: &Theme,
    now: DateTime<Utc>,
    (root_index, root, task): Entry<'_>,
) -> Vec<Line<'static>> {
    let review_age = task
        .review_updated_at
        .map(|at| format!("{}s", (now - at).num_seconds().max(0)))
        .unwrap_or_else(|| "unknown age".into());
    let older = predates_worker_suffix(task);
    let queue = review_queue_label(root);
    vec![
        Line::styled(
            format!(
                "{} | worker {} | round {}",
                safe_text(&task.name),
                task.worker,
                task.round
            ),
            theme.picker_title,
        ),
        Line::raw(format!(
            "record {} | {} | liveness unverified",
            safe_text(&task.recorded_state),
            age(task, now)
        )),
        Line::raw(activity(task)),
        Line::raw(format!(
            "root {} {}",
            root_index + 1,
            safe_text(&root.source.directory.to_string_lossy())
        )),
        Line::raw(format!(
            "{} | {review_age}{older} | {}",
            task.review.label(),
            safe_text(&task.review_detail)
        )),
        Line::raw(format!("queue {queue}")),
    ]
}

fn review_queue_label(root: &RootStatus) -> String {
    root.source
        .review_queue
        .as_ref()
        .map(|q| safe_text(&q.path.to_string_lossy()))
        .unwrap_or_else(|| "not registered".into())
}

fn predates_worker_suffix(task: &TaskStatus) -> &'static str {
    if task.review_predates_worker() {
        " / predates worker"
    } else {
        ""
    }
}

fn run_style(theme: &Theme, run: RunState) -> ratatui::style::Style {
    match run {
        RunState::Failed => theme.tool_error,
        RunState::Blocked | RunState::GateWaiting | RunState::AwaitingInput => theme.alert,
        RunState::Running => theme.tool_running,
        RunState::Finished | RunState::Queued | RunState::Cancelled | RunState::Unknown => {
            theme.tool_output
        }
    }
}

fn stale_notices(snapshot: &Snapshot) -> Vec<String> {
    let mut notices = Vec::new();
    if let Some(error) = snapshot.registration_error {
        notices.push(format!("registration stale / {error}"));
    }
    for (index, root) in snapshot.roots.iter().enumerate() {
        if let Some(error) = root.read_error {
            notices.push(format!("root {} stale / {error}", index + 1));
        }
        if let Some(error) = root.review_error {
            notices.push(format!("review {} stale / {error}", index + 1));
        }
        if let Some(error) = root.supervisor_error {
            notices.push(format!("supervisor {} stale / {error}", index + 1));
        }
    }
    notices
}

fn age(task: &TaskStatus, now: DateTime<Utc>) -> String {
    task.age_seconds(now)
        .map(|age| {
            if age < 60 {
                format!("{age}s")
            } else if age < 3600 {
                format!("{}m", age / 60)
            } else {
                format!("{}h", age / 3600)
            }
        })
        .unwrap_or_else(|| "age unknown".into())
}

fn activity(task: &TaskStatus) -> String {
    match task.run {
        RunState::Running => {
            let detail = task.headless_activity.as_deref().unwrap_or(&task.activity);
            let stale = if task.headless_error.is_some() {
                "headless stale / "
            } else {
                ""
            };
            let detail = if detail.is_empty() {
                "activity unavailable".into()
            } else {
                safe_text(detail)
            };
            format!("liveness unverified / {stale}{detail}")
        }
        _ if !task.activity.is_empty() => safe_text(&task.activity),
        RunState::Blocked => "blocking reason unavailable".into(),
        RunState::GateWaiting => "waiting for coordinator handoff".into(),
        RunState::AwaitingInput => "decision required".into(),
        RunState::Queued => "waiting for execution".into(),
        RunState::Finished => "implementation finished / review recorded separately".into(),
        RunState::Failed => "execution failed".into(),
        RunState::Cancelled => "execution cancelled".into(),
        RunState::Unknown => "execution state unknown".into(),
    }
}
