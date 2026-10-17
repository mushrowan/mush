use mush_ai::types::{Dollars, TokenCount};
use mush_runtime::{ChildStatus, ChildSummary};

const BAR_WIDTH: usize = 10;

#[derive(Debug, Clone, PartialEq)]
pub struct AgentNode {
    pub label: String,
    pub marker: &'static str,
    pub model_id: Option<String>,
    pub tokens: TokenCount,
    pub cost: Dollars,
    pub context: Option<(TokenCount, TokenCount)>,
    pub activity: Option<String>,
    pub children: Vec<AgentNode>,
}

#[must_use]
pub fn agent_tree(
    label: &str,
    model_id: &str,
    stats: &crate::prompt_cache::TokenStats,
    children: &[ChildSummary],
) -> AgentNode {
    AgentNode {
        label: label.to_string(),
        marker: "●",
        model_id: Some(model_id.to_string()),
        tokens: stats.total_tokens,
        cost: stats.total_cost,
        context: (stats.context_window > TokenCount::ZERO)
            .then_some((stats.context_tokens, stats.context_window)),
        activity: None,
        children: child_nodes(children, None),
    }
}

fn child_nodes(children: &[ChildSummary], parent: Option<&str>) -> Vec<AgentNode> {
    children
        .iter()
        .filter(|child| child.parent.as_deref() == parent && child.status != ChildStatus::Deleted)
        .map(|child| AgentNode {
            label: child.name.clone(),
            marker: status_marker(child.status),
            model_id: Some(child.model_id.clone()),
            tokens: child.usage.total_tokens(),
            cost: child.cost.total(),
            context: None,
            activity: activity(child, children),
            children: child_nodes(children, Some(&child.id)),
        })
        .collect()
}

fn activity(child: &ChildSummary, family: &[ChildSummary]) -> Option<String> {
    if let Some(prompt) = &child.awaiting_confirmation {
        return Some(format!("waiting for you: {prompt}"));
    }
    if let Some(tool) = &child.tool_running {
        return Some(tool.clone());
    }
    let running = family
        .iter()
        .filter(|other| {
            other.parent.as_deref() == Some(&child.id) && other.status == ChildStatus::Running
        })
        .count();
    if running > 0 {
        return Some(format!("waiting on {running} child(ren)"));
    }
    if child.streaming {
        return Some("thinking".into());
    }
    child.progress_note.clone()
}

const fn status_marker(status: ChildStatus) -> &'static str {
    match status {
        ChildStatus::Starting => "◇",
        ChildStatus::Running => "◆",
        ChildStatus::Idle => "✓",
        ChildStatus::Failed | ChildStatus::Cancelled => "✗",
        ChildStatus::Interrupted => "◌",
        ChildStatus::Deleted => "·",
    }
}

fn flatten<'a>(
    node: &'a AgentNode,
    prefix: String,
    indent: &str,
    rows: &mut Vec<(String, &'a AgentNode)>,
) {
    rows.push((prefix, node));
    for (index, child) in node.children.iter().enumerate() {
        let last = index + 1 == node.children.len();
        let branch = if last {
            "\u{2514}\u{2500} "
        } else {
            "\u{251c}\u{2500} "
        };
        let child_indent = format!("{indent}{}", if last { "   " } else { "\u{2502}  " });
        flatten(child, format!("{indent}{branch}"), &child_indent, rows);
    }
}

fn totals(node: &AgentNode) -> (TokenCount, Dollars, usize) {
    node.children.iter().fold(
        (node.tokens, node.cost, 1),
        |(tokens, cost, count), child| {
            let (child_tokens, child_cost, child_count) = totals(child);
            (
                tokens + child_tokens,
                cost + child_cost,
                count + child_count,
            )
        },
    )
}

#[must_use]
pub fn format_context(root: &AgentNode, stats: &crate::prompt_cache::TokenStats) -> String {
    let mut rows = Vec::new();
    flatten(root, String::new(), "", &mut rows);

    let token_cells: Vec<String> = rows
        .iter()
        .map(|(_, node)| crate::turn_activity::format_tokens(node.tokens.get()))
        .collect();
    let cost_cells: Vec<String> = rows
        .iter()
        .map(|(_, node)| format!("{}", node.cost))
        .collect();
    let label_cells: Vec<String> = rows
        .iter()
        .map(|(prefix, node)| format!("{prefix}{} {}", node.marker, node.label))
        .collect();
    let label_width = label_cells
        .iter()
        .map(|cell| cell.chars().count())
        .max()
        .unwrap_or(0);
    let token_width = column_width("tokens", &token_cells);
    let cost_width = column_width("cost", &cost_cells);

    let mut lines = Vec::new();
    if let Some(model) = &root.model_id {
        lines.push(format!("model: {model}"));
        lines.push(String::new());
    }
    let shows_activity = rows.iter().any(|(_, node)| node.activity.is_some());
    let context_width = if shows_activity { 12 } else { 0 };
    lines.push(format!(
        "{:<label_width$}  {:>token_width$}  {:>cost_width$}  {:<context_width$}{}",
        "agent",
        "tokens",
        "cost",
        "context",
        if shows_activity { "activity" } else { "" }
    ));
    for (index, (_, node)) in rows.iter().enumerate() {
        let padding = label_width.saturating_sub(label_cells[index].chars().count());
        let line = format!(
            "{}{}  {:>token_width$}  {:>cost_width$}  {:<context_width$}{}",
            label_cells[index],
            " ".repeat(padding),
            token_cells[index],
            cost_cells[index],
            context_cell(node.context),
            node.activity.as_deref().unwrap_or_default(),
        );
        lines.push(line.trim_end().to_string());
    }

    let (tokens, cost, agents) = totals(root);
    lines.push(String::new());
    let across = if agents > 1 {
        format!(" across {agents} agents")
    } else {
        String::new()
    };
    lines.push(format!(
        "total: {} tokens · {cost}{across}",
        crate::turn_activity::format_tokens(tokens.get())
    ));

    lines.push(String::new());
    lines.push(format!(
        "this session: ↑{} ↓{} read {} write {}",
        stats.input_tokens, stats.output_tokens, stats.cache_read_tokens, stats.cache_write_tokens
    ));
    let cached = stats.cache_read_tokens + stats.cache_write_tokens + stats.input_tokens;
    if cached > TokenCount::ZERO {
        lines.push(format!(
            "cache reuse: {}% of input",
            stats.cache_read_tokens.percent_of(cached) as u64
        ));
    }
    lines.join("\n")
}

fn column_width(header: &str, cells: &[String]) -> usize {
    cells.iter().map(String::len).fold(header.len(), usize::max)
}

fn context_cell(context: Option<(TokenCount, TokenCount)>) -> String {
    let Some((used, window)) = context else {
        return "-".into();
    };
    if window == TokenCount::ZERO {
        return "-".into();
    }
    let percent = used.percent_of(window);
    let filled = ((percent / 100.0) * BAR_WIDTH as f64)
        .round()
        .clamp(0.0, BAR_WIDTH as f64) as usize;
    format!(
        "{}{} {}% ({}/{})",
        "\u{2593}".repeat(filled),
        "\u{2591}".repeat(BAR_WIDTH - filled),
        percent.round() as u64,
        crate::turn_activity::format_tokens(used.get()),
        crate::turn_activity::format_tokens(window.get()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{Cost, Timestamp, Usage};

    fn stats() -> crate::prompt_cache::TokenStats {
        let mut stats = crate::prompt_cache::TokenStats::new(TokenCount::new(200_000));
        stats.context_tokens = TokenCount::new(50_000);
        stats.input_tokens = TokenCount::new(1_200);
        stats.output_tokens = TokenCount::new(800);
        stats.cache_read_tokens = TokenCount::new(8_000);
        stats.cache_write_tokens = TokenCount::new(2_000);
        stats.total_tokens = TokenCount::new(12_000);
        stats.total_cost = Dollars::new(0.42);
        stats
    }

    fn child(id: &str, name: &str, parent: Option<&str>, status: ChildStatus) -> ChildSummary {
        ChildSummary {
            paused: None,
            context: None,
            id: id.into(),
            name: name.into(),
            parent: parent.map(Into::into),
            depth: u32::from(parent.is_some()) + 1,
            session_id: format!("session-{id}"),
            model_id: "claude-opus-4-7".into(),
            thinking: None,
            status,
            spawned_at: Timestamp::zero(),
            updated_at: Timestamp::zero(),
            idle_since: Timestamp::zero(),
            answer: None,
            error: None,
            progress_note: None,
            usage: Usage {
                input_tokens: TokenCount::new(400),
                output_tokens: TokenCount::new(100),
                ..Usage::default()
            },
            cost: Cost {
                input: Dollars::new(0.01),
                ..Cost::default()
            },
            turns: 1,
            streaming: false,
            tool_running: None,
            awaiting_confirmation: None,
        }
    }

    #[test]
    fn a_lone_session_reports_its_own_row_and_the_context_bar() {
        let stats = stats();
        let tree = agent_tree("port parser", "claude-opus-4-7", &stats, &[]);
        let text = format_context(&tree, &stats);

        assert!(text.starts_with("model: claude-opus-4-7"), "{text}");
        assert!(text.contains("● port parser"), "{text}");
        assert!(text.contains("12k"), "{text}");
        assert!(text.contains("25% (50k/200k)"), "{text}");
        assert!(text.contains("▓▓▓░░░░░░░"), "{text}");
        assert!(text.contains("total: 12k tokens · $0.4200"), "{text}");
        assert!(!text.contains("across"), "{text}");
        assert!(text.contains("cache reuse: 71% of input"), "{text}");
    }

    #[test]
    fn children_nest_under_their_parent_and_fold_into_the_total() {
        let stats = stats();
        let children = vec![
            child("c1", "reviewer", None, ChildStatus::Running),
            child("c2", "tester", None, ChildStatus::Idle),
            child("c3", "deep", Some("c1"), ChildStatus::Failed),
        ];
        let tree = agent_tree("port parser", "claude-opus-4-7", &stats, &children);
        let text = format_context(&tree, &stats);
        let rows: Vec<&str> = text.lines().collect();

        assert!(
            rows.iter().any(|row| row.starts_with("├─ ◆ reviewer")),
            "{text}"
        );
        assert!(
            rows.iter().any(|row| row.starts_with("│  └─ ✗ deep")),
            "{text}"
        );
        assert!(
            rows.iter().any(|row| row.starts_with("└─ ✓ tester")),
            "{text}"
        );
        assert!(text.contains("across 4 agents"), "{text}");
        assert!(text.contains("total: 13k tokens"), "{text}");
        assert!(
            text.lines()
                .filter(|row| row.contains("claude-opus"))
                .count()
                == 1,
            "only the root names its model: {text}"
        );
    }

    #[test]
    fn a_deleted_child_is_left_out() {
        let stats = stats();
        let children = vec![child("c1", "gone", None, ChildStatus::Deleted)];
        let tree = agent_tree("port parser", "claude-opus-4-7", &stats, &children);
        assert!(tree.children.is_empty());
    }

    #[test]
    fn columns_line_up_when_a_child_name_is_longer_than_the_session_name() {
        let stats = stats();
        let children = vec![child(
            "c1",
            "a-very-long-child-agent-name",
            None,
            ChildStatus::Idle,
        )];
        let tree = agent_tree("short", "m", &stats, &children);
        let text = format_context(&tree, &stats);
        let right_edge = |needle: &str| {
            text.lines()
                .find(|row| row.contains(needle))
                .map(|row| {
                    row[..row.find(needle).unwrap() + needle.len()]
                        .chars()
                        .count()
                })
                .unwrap_or_else(|| panic!("{needle} missing from {text}"))
        };

        assert_eq!(right_edge("tokens"), right_edge("12k"));
        assert_eq!(right_edge("tokens"), right_edge("500"));
        assert_eq!(right_edge("cost"), right_edge("$0.4200"));
    }

    #[test]
    fn a_child_blocked_on_you_says_so_and_beats_every_other_activity() {
        let mut blocked = child("a", "scout", None, ChildStatus::Running);
        blocked.streaming = true;
        blocked.tool_running = Some("bash: rm -rf build".into());
        blocked.progress_note = Some("halfway".into());
        blocked.awaiting_confirmation = Some("run `rm -rf build`?".into());

        let rendered = format_context(&agent_tree("root", "m", &stats(), &[blocked]), &stats());

        assert!(rendered.contains("activity"));
        assert!(rendered.contains("waiting for you: run `rm -rf build`?"));
        assert!(!rendered.contains("halfway"));
    }

    #[test]
    fn a_running_tool_shows_over_thinking_and_a_note() {
        let mut busy = child("a", "scout", None, ChildStatus::Running);
        busy.streaming = true;
        busy.tool_running = Some("ipython: import pandas".into());
        busy.progress_note = Some("halfway".into());

        let rendered = format_context(&agent_tree("root", "m", &stats(), &[busy]), &stats());

        assert!(rendered.contains("ipython: import pandas"));
        assert!(!rendered.contains("thinking"));
    }

    #[test]
    fn a_parent_waiting_on_children_says_how_many() {
        let parent = child("a", "lead", None, ChildStatus::Running);
        let first = child("b", "one", Some("a"), ChildStatus::Running);
        let second = child("c", "two", Some("a"), ChildStatus::Running);
        let done = child("d", "three", Some("a"), ChildStatus::Idle);

        let rendered = format_context(
            &agent_tree("root", "m", &stats(), &[parent, first, second, done]),
            &stats(),
        );

        assert!(rendered.contains("waiting on 2 child(ren)"));
    }

    #[test]
    fn an_idle_family_has_no_activity_column() {
        let quiet = child("a", "scout", None, ChildStatus::Idle);
        let rendered = format_context(&agent_tree("root", "m", &stats(), &[quiet]), &stats());
        assert!(!rendered.contains("activity"));
    }
}
