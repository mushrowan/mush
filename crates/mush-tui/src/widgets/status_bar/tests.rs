use std::time::Duration;

use chrono::{TimeDelta, Utc};
use mush_ai::oauth::usage::{OAuthUsage, UsageWindow};
use mush_ai::types::{Dollars, ThinkingLevel, TokenCount};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, CellWidth};

use super::*;
use crate::app_state::StatusBarLayout;
use crate::display_types::{ActiveToolState, ToolCallStatus};
use crate::test_support::buffer_to_string;

fn render_status(app: &App, width: u16) -> Buffer {
    app.render_state.status_bar_cache.borrow_mut().take();
    let height = status_bar_height(app, width);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| frame.render_widget(StatusBar::new(app), frame.area()))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn status_rows(app: &App, width: u16) -> Vec<String> {
    buffer_to_string(&render_status(app, width))
        .lines()
        .map(|row| row.trim_end().to_string())
        .collect()
}

fn status(app: &App, width: u16) -> String {
    status_rows(app, width).join("\n")
}

fn running_tool(name: &str) -> ActiveToolState {
    ActiveToolState {
        tool_call_id: format!("t-{name}").into(),
        name: name.into(),
        summary: String::new(),
        live_output: None,
        status: ToolCallStatus::Running,
        output: None,
    }
}

fn child(
    id: &str,
    status: mush_runtime::ChildStatus,
    note: Option<&str>,
) -> mush_runtime::ChildSummary {
    mush_runtime::ChildSummary {
        paused: None,
        context: None,
        id: id.into(),
        name: id.into(),
        parent: None,
        depth: 1,
        session_id: format!("s-{id}"),
        model_id: "claude-opus-5-5".into(),
        sandbox: None,
        sandbox_profile: None,
        thinking: None,
        status,
        spawned_at: mush_ai::types::Timestamp::zero(),
        updated_at: mush_ai::types::Timestamp::zero(),
        idle_since: mush_ai::types::Timestamp::zero(),
        answer: None,
        error: None,
        progress_note: note.map(Into::into),
        usage: mush_ai::types::Usage::default(),
        cost: mush_ai::types::Cost::default(),
        turns: 1,
        streaming: false,
        tool_running: None,
        awaiting_confirmation: None,
        waiting_for_network: None,
    }
}

fn heartbeat(expression: &str) -> mush_runtime::Job {
    mush_runtime::Job {
        id: "hb".into(),
        label: None,
        prompt: "check in".into(),
        schedule: mush_runtime::ScheduleKind::Every {
            interval_ms: 600_000,
            next_at: 0,
        },
        expression: expression.into(),
        source: mush_runtime::JobSource::Heartbeat,
        status: mush_runtime::JobStatus::Active,
        delivery: mush_runtime::Delivery::Steer,
        runs: 0,
        created_at: mush_ai::types::Timestamp::zero(),
        last_run_at: None,
    }
}

fn usage_window(used: f32, pace: f32, length: TimeDelta) -> UsageWindow {
    let left = length.num_seconds() as f64 * f64::from(100.0 - pace) / 100.0;
    UsageWindow {
        utilization: used,
        resets_at: Utc::now() + TimeDelta::seconds(left as i64),
    }
}

fn backdated(secs: u64) -> Duration {
    Duration::from_secs(secs) + Duration::from_millis(100)
}

fn session(context: u64, cache_elapsed: u64, five: (f32, f32), seven: (f32, f32)) -> App {
    let mut app = App::new("claude-opus-5-5".into(), TokenCount::new(1_000_000));
    app.thinking_level = ThinkingLevel::High;
    app.cwd = "~/dev/ai-safety".into();
    app.stats.context_tokens = TokenCount::new(context);
    app.cache.ttl_secs = 3600;
    app.cache.refresh();
    app.cache.backdate(backdated(cache_elapsed));
    app.oauth_usage_provider = Some("anthropic".into());
    app.oauth_usage = Some(OAuthUsage {
        five_hour: Some(usage_window(five.0, five.1, OAuthUsage::FIVE_HOUR)),
        seven_day: Some(usage_window(seven.0, seven.1, OAuthUsage::SEVEN_DAY)),
    });
    app
}

fn finish_a_turn(app: &mut App, secs: u64) {
    app.begin_turn();
    app.turn.backdate(backdated(secs));
    app.end_turn();
}

fn start_a_turn(app: &mut App, secs: u64) {
    app.begin_turn();
    app.turn.backdate(backdated(secs));
    app.start_streaming();
}

fn idle() -> App {
    let mut app = session(182_000, 48, (37.0, 45.0), (26.0, 40.0));
    finish_a_turn(&mut app, 125);
    app
}

fn streaming_with_a_tool() -> App {
    let mut app = session(482_000, 0, (41.0, 47.0), (27.0, 41.0));
    start_a_turn(&mut app, 65);
    app.push_text_delta("checking the build");
    app.turn.record_message(Some(1_200), 0);
    app.active_tools.push(running_tool("bash"));
    app
}

fn children_with_notes(root_busy: bool) -> App {
    use mush_runtime::ChildStatus::{Idle, Running};
    let mut app = session(312_000, 72, (52.0, 49.0), (31.0, 41.0));
    finish_a_turn(&mut app, 41);
    app.note_child(child("mush-vim", Idle, Some("round 2 done and reported")));
    app.note_child(child(
        "mush-models",
        Running,
        Some("checking the openrouter catalogue"),
    ));
    app.note_child(child("mush-history", Idle, None));
    app.note_child(child("mush-ctx", Running, None));
    let mut asking = child("mush-completions", Running, Some("warming the build"));
    asking.awaiting_confirmation = Some("bash cargo build".into());
    app.note_child(asking);
    app.note_child(child(
        "mush-quickwins",
        Running,
        Some("items 8,6,3,2,4b,4a,5 committed (11 commits). starting item 1 (transcript rows), then item 7 (vim indicator)"),
    ));
    app.jobs.push(heartbeat("every 10m"));
    if root_busy {
        start_a_turn(&mut app, 192);
        app.active_tools.push(running_tool("ipython"));
    }
    app
}

fn near_the_context_limit() -> App {
    use mush_runtime::ChildStatus::{Idle, Running};
    let mut app = session(912_000, 198, (83.0, 61.0), (64.0, 52.0));
    start_a_turn(&mut app, 273);
    app.push_text_delta("drafting the summary");
    app.turn.record_message(Some(3_100), 0);
    app.note_child(child("lean-store", Running, None));
    app.note_child(child("det-model", Running, None));
    app.note_child(child("wayback", Idle, None));
    app
}

fn vim_normal() -> App {
    use mush_runtime::ChildStatus::{Idle, Running};
    let mut app = idle();
    app.interaction.mode = crate::app::AppMode::Normal;
    app.note_child(child("mush-quickwins", Idle, None));
    app.note_child(child(
        "mush-vim",
        Running,
        Some("round 2 done and reported (tip ssmtokny, 12 commits). idle until the parent replies"),
    ));
    app
}

fn states() -> Vec<(&'static str, App)> {
    vec![
        ("idle", idle()),
        ("streaming-tools", streaming_with_a_tool()),
        ("children-notes", children_with_notes(false)),
        ("children-notes-root-busy", children_with_notes(true)),
        ("near-context-limit", near_the_context_limit()),
        ("vim-normal", vim_normal()),
    ]
}

fn assert_renders(state: fn() -> App, expected: &[(u16, [&str; 2])]) {
    for (width, rows) in expected {
        let app = state();
        let got = status_rows(&app, *width);
        let want: Vec<&str> = if *width >= 140 {
            vec![rows[0]]
        } else {
            rows.to_vec()
        };
        assert_eq!(got, want, "at {width} cols");
    }
}

#[test]
fn idle_renders_at_each_width() {
    assert_renders(
        idle,
        &[
            (
                40,
                [
                    " ◇ ready · last turn 2m 05s",
                    " c-o-5-5 high  18% · 3:12        5h 37%",
                ],
            ),
            (
                60,
                [
                    " ◇ ready · last turn 2m 05s",
                    " c-o-5-5 high  ctx 182k/1.0m 18% · 3:12     5h 37% · 7d 26%",
                ],
            ),
            (
                80,
                [
                    " ◇ ready · last turn 2m 05s",
                    " c-o-5-5 high  ctx 182k/1.0m 18% · 3:12              5h 37% · 7d 26%  ai-safety",
                ],
            ),
            (
                120,
                [
                    " ◇ ready · last turn 2m 05s",
                    " c-o-5-5 high  ctx 182k/1.0m 18% · cache 3:12/58:12                  5h ━━━━──── 37% · 7d ━━━───── 26%  ~/dev/ai-safety",
                ],
            ),
            (
                180,
                [
                    " c-o-5-5 high  ctx 182k/1.0m 18% · cache 3:12/58:12  ◇ ready · last turn 2m 05s                                                  5h ━━━━──── 37% · 7d ━━━───── 26%  ~/dev/ai-safety",
                    "",
                ],
            ),
        ],
    );
}

#[test]
fn streaming_with_a_tool_renders_at_each_width() {
    assert_renders(
        streaming_with_a_tool,
        &[
            (
                40,
                [
                    " ⠋ using bash · 1m 05s · ↓ 1.2k tokens",
                    " c-o-5-5 high  48% · 4:00        5h 41%",
                ],
            ),
            (
                60,
                [
                    " ⠋ using bash · 1m 05s · ↓ 1.2k tokens",
                    " c-o-5-5 high  ctx 482k/1.0m 48% · 4:00     5h 41% · 7d 27%",
                ],
            ),
            (
                80,
                [
                    " ⠋ using bash · 1m 05s · ↓ 1.2k tokens",
                    " c-o-5-5 high  ctx 482k/1.0m 48% · 4:00              5h 41% · 7d 27%  ai-safety",
                ],
            ),
            (
                120,
                [
                    " ⠋ using bash · 1m 05s · ↓ 1.2k tokens",
                    " c-o-5-5 high  ctx 482k/1.0m 48% · cache 4:00/59:00                  5h ━━━━──── 41% · 7d ━━━───── 27%  ~/dev/ai-safety",
                ],
            ),
            (
                180,
                [
                    " c-o-5-5 high  ctx 482k/1.0m 48% · cache 4:00/59:00  ⠋ using bash · 1m 05s · ↓ 1.2k tokens                                       5h ━━━━──── 41% · 7d ━━━───── 27%  ~/dev/ai-safety",
                    "",
                ],
            ),
        ],
    );
}

#[test]
fn children_with_notes_render_at_each_width() {
    assert_renders(
        || children_with_notes(false),
        &[
            (
                40,
                [
                    " ◆ 4/6 · 1 waiting  ◷ 1",
                    " c-o-5-5 high  31% · 2:48        5h 52%",
                ],
            ),
            (
                60,
                [
                    " ◆ 4/6 · 1 waiting  ◷ 1  mush-quickwins: items 8,6,3,2,4b,…",
                    " c-o-5-5 high  ctx 312k/1.0m 31% · 2:48     5h 52% · 7d 31%",
                ],
            ),
            (
                80,
                [
                    " ◆ 4/6 agents · 1 waiting on you  ◷ heartbeat  mush-quickwins: items 8,6,3,2,4…",
                    " c-o-5-5 high  ctx 312k/1.0m 31% · 2:48              5h 52% · 7d 31%  ai-safety",
                ],
            ),
            (
                120,
                [
                    " ◇ ready · last turn 41s  ◆ 4/6 agents · 1 waiting on you  ◷ heartbeat every 10m  mush-quickwins: items 8,6,3,2,4b,4a,…",
                    " c-o-5-5 high  ctx 312k/1.0m 31% · cache 2:48/57:48                  5h ━━━━──── 52% · 7d ━━━───── 31%  ~/dev/ai-safety",
                ],
            ),
            (
                180,
                [
                    " c-o-5-5 high  ctx 312k/1.0m 31% · 2:48  ◇ ready  ◆ 4/6 agents · 1 waiting on you  ◷ heartbeat every 10m  mush-quickwins: items 8,6,3,2,4b,4a,5 comm…    5h 52% · 7d 31%  ai-safety",
                    "",
                ],
            ),
        ],
    );
}

#[test]
fn children_with_a_busy_root_render_at_each_width() {
    assert_renders(
        || children_with_notes(true),
        &[
            (
                40,
                [
                    " ⠋ ipython 3m12s  ◆ 4/6 · 1 waiting",
                    " c-o-5-5 high  31% · 2:48        5h 52%",
                ],
            ),
            (
                60,
                [
                    " ⠋ using ipython · 3m 12s  ◆ 4/6 · 1 waiting  ◷ 1",
                    " c-o-5-5 high  ctx 312k/1.0m 31% · 2:48     5h 52% · 7d 31%",
                ],
            ),
            (
                80,
                [
                    " ⠋ using ipython · 3m 12s  ◆ 4/6 · 1 waiting  ◷ 1",
                    " c-o-5-5 high  ctx 312k/1.0m 31% · 2:48              5h 52% · 7d 31%  ai-safety",
                ],
            ),
            (
                120,
                [
                    " ⠋ using ipython · 3m 12s  ◆ 4/6 agents · 1 waiting on you  ◷ heartbeat every 10m  mush-quickwins: items 8,6,3,2,4b,4…",
                    " c-o-5-5 high  ctx 312k/1.0m 31% · cache 2:48/57:48                  5h ━━━━──── 52% · 7d ━━━───── 31%  ~/dev/ai-safety",
                ],
            ),
            (
                180,
                [
                    " c-o-5-5 high  ctx 312k/1.0m 31% · 2:48  ⠋ using ipython · 3m 12s  ◆ 4/6 agents · 1 waiting on you  ◷ heartbeat every 10m  mush-quickwins: items 8,6,3,2,4b,4a…     5h 52% · 7d 31%",
                    "",
                ],
            ),
        ],
    );
}

#[test]
fn near_the_context_limit_renders_at_each_width() {
    assert_renders(
        near_the_context_limit,
        &[
            (
                40,
                [
                    " ⠋ writing · 4m 33s · ↓3.1k  ◆ 2/3",
                    " c-o-5-5 high  91% · 0:42        5h 83%",
                ],
            ),
            (
                60,
                [
                    " ⠋ writing · 4m 33s · ↓ 3.1k tokens  ◆ 2/3 agents",
                    " c-o-5-5 high  ctx 912k/1.0m 91% · 0:42     5h 83% · 7d 64%",
                ],
            ),
            (
                80,
                [
                    " ⠋ writing · 4m 33s · ↓ 3.1k tokens  ◆ 2/3 agents",
                    " c-o-5-5 high  ctx 912k/1.0m 91% · 0:42              5h 83% · 7d 64%  ai-safety",
                ],
            ),
            (
                120,
                [
                    " ⠋ writing · 4m 33s · ↓ 3.1k tokens  ◆ 2/3 agents",
                    " c-o-5-5 high  ctx 912k/1.0m 91% · cache 0:42/55:42                  5h ━━━━━━━─ 83% · 7d ━━━━━─── 64%  ~/dev/ai-safety",
                ],
            ),
            (
                180,
                [
                    " c-o-5-5 high  ctx 912k/1.0m 91% · cache 0:42/55:42  ⠋ writing · 4m 33s · ↓ 3.1k tokens  ◆ 2/3 agents                            5h ━━━━━━━─ 83% · 7d ━━━━━─── 64%  ~/dev/ai-safety",
                    "",
                ],
            ),
        ],
    );
}

#[test]
fn vim_normal_mode_renders_at_each_width() {
    assert_renders(
        vim_normal,
        &[
            (
                40,
                [
                    " ◆ 1/2 agents  mush-vim: round 2 done…",
                    " c-o-5-5 high  18% · 3:12        5h 37%",
                ],
            ),
            (
                60,
                [
                    " ◇ ready  ◆ 1/2 agents  mush-vim: round 2 done and reporte…",
                    " c-o-5-5 high  ctx 182k/1.0m 18% · 3:12     5h 37% · 7d 26%",
                ],
            ),
            (
                80,
                [
                    " ◇ ready · last turn 2m 05s  ◆ 1/2 agents  mush-vim: round 2 done and reported…",
                    " c-o-5-5 high  ctx 182k/1.0m 18% · 3:12              5h 37% · 7d 26%  ai-safety",
                ],
            ),
            (
                120,
                [
                    " ◇ ready · last turn 2m 05s  ◆ 1/2 agents  mush-vim: round 2 done and reported (tip ssmtokny, 12 commits). idle until…",
                    " c-o-5-5 high  ctx 182k/1.0m 18% · cache 3:12/58:12                  5h ━━━━──── 37% · 7d ━━━───── 26%  ~/dev/ai-safety",
                ],
            ),
            (
                180,
                [
                    " c-o-5-5 high  ctx 182k/1.0m 18% · cache 3:12/58:12  ◇ ready · last turn 2m 05s  ◆ 1/2 agents  mush-vim: round 2 done and re…    5h ━━━━──── 37% · 7d ━━━───── 26%  ~/dev/ai-safety",
                    "",
                ],
            ),
        ],
    );
}

#[test]
fn key_segments_survive_every_width() {
    for (name, app) in states() {
        for width in 24..=240 {
            let text = status(&app, width);
            assert!(
                text.contains('%'),
                "{name} at {width}: context gone\n{text}"
            );
            if !app.children.is_empty() {
                assert!(text.contains('◆'), "{name} at {width}: agents gone\n{text}");
            }
            if app
                .children
                .iter()
                .any(|c| c.awaiting_confirmation.is_some())
            {
                assert!(
                    text.contains("?1") || text.contains("1 waiting"),
                    "{name} at {width}: waiting child gone\n{text}"
                );
            }
            if app.is_busy() {
                assert!(
                    text.contains('⠋'),
                    "{name} at {width}: activity gone\n{text}"
                );
            }
        }
    }
}

#[test]
fn rows_fit_the_width_and_the_row_count_follows_the_width_alone() {
    for (name, app) in states() {
        for width in 20..=240 {
            render_status(&app, width);
            let cache = app.render_state.status_bar_cache.borrow();
            let rows = &cache.as_ref().unwrap().rows;
            let expected = if width < 140 { 2 } else { 1 };
            assert_eq!(rows.len(), expected, "{name} at {width}");
            for row in rows {
                let used: usize = row
                    .spans
                    .iter()
                    .map(|span| usize::from(span.content.as_ref().cell_width()))
                    .sum();
                assert!(used <= usize::from(width), "{name} at {width}: {used}");
            }
        }
    }
}

#[test]
fn the_layout_setting_picks_the_rows() {
    let mut app = idle();
    assert_eq!(status_bar_height(&app, 139), 2);
    assert_eq!(status_bar_height(&app, 140), 1);
    app.interaction.status_bar.stack_below = 100;
    assert_eq!(status_bar_height(&app, 99), 2);
    assert_eq!(status_bar_height(&app, 100), 1);
    app.interaction.status_bar.layout = StatusBarLayout::Single;
    assert_eq!(status_bar_height(&app, 40), 1);
    app.interaction.status_bar.layout = StatusBarLayout::Stacked;
    assert_eq!(status_bar_height(&app, 240), 2);
    let rows = status_rows(&app, 240);
    assert!(rows[0].contains("ready"), "{rows:?}");
    assert!(rows[1].contains("c-o-5-5"), "{rows:?}");
}

#[test]
fn a_single_row_keeps_the_pinned_segments_at_narrow_widths() {
    let mut app = children_with_notes(true);
    app.interaction.status_bar.layout = StatusBarLayout::Single;
    let text = status(&app, 50);
    for wanted in ['⠋', '◆', '%'] {
        assert!(text.contains(wanted), "{wanted}: {text}");
    }
    assert!(text.contains("?1") || text.contains("waiting"), "{text}");
}

#[test]
fn waiting_and_failed_children_are_counted() {
    use mush_runtime::ChildStatus::{Failed, Running};
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let mut asking = child("a", Running, None);
    asking.awaiting_confirmation = Some("bash rm -rf build".into());
    app.note_child(asking);
    app.note_child(child("b", Failed, None));
    app.note_child(child("c", Running, None));

    let wide = status(&app, 200);
    assert!(
        wide.contains("◆ 2/3 agents · 1 waiting on you · 1 failed"),
        "{wide}"
    );
    let medium = status(&app, 40);
    assert!(medium.contains("◆ 2/3 · 1 waiting · 1 failed"), "{medium}");
    let narrow = status(&app, 24);
    assert!(narrow.contains("◆ 2/3 ?1 ✗1"), "{narrow}");
}

#[test]
fn children_waiting_for_the_network_are_counted() {
    use mush_runtime::ChildStatus::Running;
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let mut offline = child("a", Running, None);
    offline.waiting_for_network = Some(mush_runtime::NetworkWait {
        since: mush_ai::types::Timestamp::zero(),
        error: "failed to lookup address information".into(),
    });
    app.note_child(offline);
    app.note_child(child("b", Running, None));

    let wide = status(&app, 200);
    assert!(
        wide.contains("◆ 2/2 agents · 1 waiting for the network"),
        "{wide}"
    );
    let medium = status(&app, 40);
    assert!(medium.contains("◆ 2/2 · 1 offline"), "{medium}");
}

#[test]
fn a_turn_without_a_network_says_so_in_the_status_bar() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.stream.network_lost(std::time::Instant::now());
    let content = status(&app, 120);
    assert!(content.contains("waiting for the network"), "{content}");
}

#[test]
fn the_newest_note_of_a_running_child_shows() {
    use mush_runtime::ChildStatus::{Idle, Running};
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.note_child(child("first", Running, Some("reading the logs")));
    app.note_child(child("second", Running, Some("writing the fix")));
    assert_eq!(app.newest_child_note(), Some(("second", "writing the fix")));
    assert!(status(&app, 200).contains("second: writing the fix"));

    app.note_child(child("first", Running, Some("reading the logs")));
    assert_eq!(app.newest_child_note(), Some(("second", "writing the fix")));

    app.note_child(child("first", Running, Some("found it")));
    assert_eq!(app.newest_child_note(), Some(("first", "found it")));

    app.note_child(child("first", Idle, Some("found it")));
    assert_eq!(app.newest_child_note(), Some(("second", "writing the fix")));

    app.clear_children();
    assert_eq!(app.newest_child_note(), None);
}

#[test]
fn a_note_shrinks_to_fit_and_drops_before_the_counts() {
    use mush_runtime::ChildStatus::Running;
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.note_child(child(
        "worker",
        Running,
        Some("a long note about the work in progress"),
    ));
    let wide = status(&app, 200);
    assert!(
        wide.contains("worker: a long note about the work in progress"),
        "{wide}"
    );
    let narrow = status(&app, 40);
    assert!(narrow.contains("worker: a long"), "{narrow}");
    assert!(narrow.contains('…'), "{narrow}");
    let tight = status(&app, 24);
    assert!(!tight.contains("worker:"), "{tight}");
    assert!(tight.contains("◆ 1/1"), "{tight}");
}

#[test]
fn idle_shows_how_long_the_last_turn_took() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    assert!(status(&app, 200).contains("◇ ready"));
    assert!(!status(&app, 200).contains("last turn"));
    finish_a_turn(&mut app, 125);
    assert!(status(&app, 200).contains("◇ ready · last turn 2m 05s"));
}

#[test]
fn a_turn_between_messages_reads_as_waiting() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.begin_turn();
    let text = status(&app, 200);
    assert!(text.contains("⠋ waiting"), "{text}");
    assert!(!text.contains("ready"), "{text}");
}

#[test]
fn the_spinner_turns_with_the_animation_clock() {
    let frames: Vec<&str> = (0..10).map(|frame| segments::spinner(frame * 3)).collect();
    assert_eq!(frames, ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]);
    assert_eq!(segments::spinner(30), "⠋");
}

#[test]
fn a_subagent_model_that_clashes_lengthens_the_clashing_word() {
    let mut app = App::new("claude-opus-4-5".into(), TokenCount::new(200_000));
    app.subagent_model_id = Some("claude-orca-4-5".into());
    let text = status(&app, 200);
    assert!(text.contains("c-op-4-5"), "{text}");
    assert!(text.contains("sub c-or-4-5"), "{text}");
    app.subagent_model_id = Some("claude-sonnet-4-5".into());
    let text = status(&app, 200);
    assert!(text.contains("c-o-4-5"), "{text}");
    assert!(text.contains("sub c-s-4-5"), "{text}");
}

#[test]
fn the_model_never_shows_its_full_id() {
    let app = idle();
    for width in 24..=240 {
        assert!(!status(&app, width).contains("claude-opus"), "at {width}");
    }
}

#[test]
fn status_bar_says_when_sync_fails_and_when_the_session_moved_away() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.note_sync(mush_session::SyncState {
        taken_by: None,
        problem: Some("the sync server is unreachable".into()),
    });
    let content = status(&app, 120);
    assert!(content.contains("sync failing"), "{content}");
    assert!(app.messages.is_empty());

    app.note_sync(mush_session::SyncState {
        taken_by: Some("esme".into()),
        problem: None,
    });
    let content = status(&app, 120);
    assert!(content.contains("read-only, open on esme"), "{content}");
    assert_eq!(app.messages.len(), 1);
    assert!(
        app.messages[0]
            .content
            .contains("esme took this session over"),
        "{}",
        app.messages[0].content
    );
    app.note_sync(mush_session::SyncState {
        taken_by: Some("esme".into()),
        problem: None,
    });
    assert_eq!(app.messages.len(), 1);
    assert!(status(&app, 24).contains("read-only"));
}

#[test]
fn a_paused_session_says_so_at_every_width() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.paused = Some(mush_session::Pause {
        at: mush_ai::types::Timestamp::zero(),
        reason: Some("lunch".into()),
        by: None,
    });
    for width in 24..=200 {
        assert!(status(&app, width).contains("paused"), "at {width}");
    }
}

#[test]
fn status_bar_shows_model() {
    let app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
    let content = status(&app, 120);
    assert!(
        content.contains("c-s-4"),
        "expected abbreviated model: {content}"
    );
}

#[test]
fn status_bar_names_the_session_account() {
    let mut app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
    let content = status(&app, 120);
    assert!(!content.contains('@'), "{content}");
    app.accounts.set(
        "anthropic",
        Some(mush_ai::types::AccountLabel::parse("work").unwrap()),
    );
    let content = status(&app, 120);
    assert!(content.contains("c-s-4 off @work"), "{content}");
}

#[test]
fn status_bar_shows_cost_and_context() {
    let mut app = App::new("test-model".into(), TokenCount::new(200_000));
    app.stats.total_cost = Dollars::new(0.0123);
    app.interaction.show_cost = true;
    app.interaction.show_token_counters = true;
    app.stats.input_tokens = TokenCount::new(45_000);
    app.stats.output_tokens = TokenCount::new(12_000);
    app.stats.cache_read_tokens = TokenCount::new(8_000);
    app.stats.cache_write_tokens = TokenCount::new(2_000);
    app.stats.context_tokens = TokenCount::new(45_000);
    let content = status(&app, 160);
    for wanted in ["↑45k", "↓12k", "R8k", "W2k", "45k/200k", "$0.0123"] {
        assert!(content.contains(wanted), "{wanted}: {content}");
    }
}

#[test]
fn status_bar_hides_token_counters_by_default() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stats.input_tokens = TokenCount::new(45_000);
    app.stats.output_tokens = TokenCount::new(12_000);
    app.stats.cache_read_tokens = TokenCount::new(8_000);
    app.stats.cache_write_tokens = TokenCount::new(2_000);
    let content = status(&app, 120);
    for hidden in ["↑45k", "↓12k", "R8k", "W2k"] {
        assert!(!content.contains(hidden), "{hidden}: {content}");
    }
}

#[test]
fn a_status_never_lands_in_the_bar() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.status = Some("clipboard failed: wl-copy".into());
    assert!(!status(&app, 200).contains("clipboard failed"));
    app.status = Some("config reloaded".into());
    assert!(!status(&app, 120).contains("config reloaded"));
}

#[test]
fn status_bar_shows_activity_verb_when_streaming() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.push_text_delta("hi");
    let content = status(&app, 120);
    assert!(
        content.contains("writing"),
        "expected 'writing' in status: {content}"
    );
}

#[test]
fn status_bar_shows_using_tool_when_running() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.active_tools.push(running_tool("bash"));
    let content = status(&app, 120);
    assert!(
        content.contains("using bash"),
        "expected 'using bash' in status: {content}"
    );
}

#[test]
fn status_bar_activity_verb_overrides_status_message() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.status = Some("press ctrl+c again to exit".into());
    app.start_streaming();
    let content = status(&app, 120);
    assert!(content.contains("waiting"));
    assert!(!content.contains("press ctrl+c again to exit"));
}

#[test]
fn hiding_status_messages_hides_the_activity() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.interaction.status_bar.show_status_messages = false;
    app.start_streaming();
    let content = status(&app, 120);
    assert!(!content.contains("waiting"), "{content}");
    assert!(!content.contains("ready"), "{content}");
}

#[test]
fn status_bar_shows_cwd_right() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.cwd = "/home/user/project".into();
    let rows = status_rows(&app, 120);
    assert!(rows[1].ends_with("/home/user/project"), "{rows:?}");
}

#[test]
fn status_bar_shows_thinking_level() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.thinking_level = ThinkingLevel::High;
    assert!(status(&app, 120).contains("t high"));
}

#[test]
fn a_halfwidth_dakuten_cwd_is_measured_in_terminal_cells() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.cwd = "abcｶﾞ".repeat(4);
    let content = status(&app, 60);
    assert!(
        content.lines().any(|row| row.matches("ｶﾞ").count() == 4),
        "every halfwidth dakuten pair stays visible, got:\n{content}"
    );
}

#[test]
fn a_halfwidth_dakuten_model_never_overflows_the_row() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.interaction.status_bar.show_cwd = false;
    app.model_id = "abcｶﾞ".repeat(4).into();
    for width in 20..=80 {
        render_status(&app, width);
        let cache = app.render_state.status_bar_cache.borrow();
        for row in &cache.as_ref().unwrap().rows {
            let used: usize = row
                .spans
                .iter()
                .map(|span| usize::from(span.content.as_ref().cell_width()))
                .sum();
            assert!(used <= usize::from(width), "{used} > {width}");
        }
    }
}

#[test]
fn usage_meters_give_way_to_numbers_on_narrow_widths() {
    let mut app = App::new("claude-sonnet-4-5".into(), TokenCount::new(200_000));
    app.oauth_usage_provider = Some("anthropic".into());
    app.oauth_usage = Some(OAuthUsage {
        five_hour: Some(usage_window(50.0, 40.0, OAuthUsage::FIVE_HOUR)),
        seven_day: None,
    });
    app.cwd = "~/dev/mush".into();
    let wide = status(&app, 200);
    assert!(wide.contains("5h ━━━━──── 50%"), "{wide}");
    let narrow = status(&app, 30);
    assert!(narrow.contains("5h 50%"), "{narrow}");
    assert!(!narrow.contains('━'), "{narrow}");
}

#[test]
fn status_bar_shows_cache_countdown() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.cache.ttl_secs = 300;
    app.stats.context_tokens = TokenCount::new(10_000);
    app.cache.refresh();
    let content = status(&app, 120);
    assert!(content.contains("10k/200k"), "{content}");
    assert!(
        content.contains("cache 5:00") || content.contains("cache 4:59"),
        "{content}"
    );
}

fn find_symbols(buf: &Buffer, text: &str) -> Option<(u16, u16)> {
    let symbols: Vec<String> = text.chars().map(String::from).collect();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let fits = x as usize + symbols.len() <= buf.area.width as usize;
            if fits
                && symbols
                    .iter()
                    .enumerate()
                    .all(|(offset, symbol)| buf[(x + offset as u16, y)].symbol() == symbol)
            {
                return Some((x, y));
            }
        }
    }
    None
}

#[test]
fn status_bar_marks_cache_countdown_cells_always_update() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.cache.ttl_secs = 300;
    app.stats.context_tokens = TokenCount::new(10_000);
    app.cache.refresh();
    let buf = render_status(&app, 120);
    let timer = app
        .render_state
        .status_bar_cache
        .borrow()
        .as_ref()
        .and_then(|cache| cache.cache_timer_text.clone())
        .expect("a ticking timer");

    let (x, y) = find_symbols(&buf, "10k/200k").expect("context");
    for x in x..x + 8 {
        assert_eq!(
            buf[(x, y)].diff_option,
            CellDiffOption::None,
            "static cell {x}"
        );
    }
    let (x, y) = find_symbols(&buf, &timer).expect("timer");
    for x in x..x + timer.chars().count() as u16 {
        assert_eq!(
            buf[(x, y)].diff_option,
            CellDiffOption::AlwaysUpdate,
            "timer cell {x}"
        );
    }
}

#[test]
fn the_short_timer_form_still_repaints_every_second() {
    let mut app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
    app.cache.ttl_secs = 3600;
    app.stats.context_tokens = TokenCount::new(10_000);
    app.cache.refresh();
    app.cache.backdate(backdated(60));
    let buf = render_status(&app, 40);
    let (x, y) = find_symbols(&buf, "3:00").expect("short timer");
    for x in x..x + 4 {
        assert_eq!(buf[(x, y)].diff_option, CellDiffOption::AlwaysUpdate);
    }
}

#[test]
fn status_bar_shows_short_and_long_cache_tiers_for_hour_claude_cache() {
    let mut app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
    app.cache.ttl_secs = 3600;
    app.stats.context_tokens = TokenCount::new(10_000);
    app.cache.refresh();
    let content = status(&app, 140);
    assert!(
        content.contains("4:00/59:00") || content.contains("3:59/58:59"),
        "expected both short and long cache tier countdowns: {content}"
    );
}

#[test]
fn an_expiring_cache_timer_stays_at_every_width() {
    let mut app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
    app.cache.ttl_secs = 3600;
    app.stats.context_tokens = TokenCount::new(10_000);
    app.cache.refresh();
    app.cache.backdate(backdated(198));
    for width in 24..=200 {
        assert!(status(&app, width).contains("0:42"), "at {width}");
    }
}

#[test]
fn status_bar_hides_cold_when_cache_timer_disabled() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.cache.ttl_secs = 0;
    app.stats.context_tokens = TokenCount::new(10_000);
    app.cache.refresh();
    let content = status(&app, 120);
    assert!(content.contains("10k/200k"));
    assert!(!content.contains("cold"), "{content}");
}

#[test]
fn context_colour_follows_how_full_it_is() {
    use ratatui::style::{Color, Modifier};
    let theme = crate::theme::Theme::dark();
    for (tokens, fg, bold) in [
        (100_000, Color::Reset, false),
        (150_000, theme.context_warn.fg.unwrap(), false),
        (190_000, theme.context_danger.fg.unwrap(), true),
    ] {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.stats.context_tokens = TokenCount::new(tokens);
        let buf = render_status(&app, 120);
        let pct = format!("{}%", tokens * 100 / 200_000);
        let (x, y) = find_symbols(&buf, &pct).expect("percentage");
        let style = buf[(x, y)].style();
        assert_eq!(style.fg, Some(fg), "{pct}");
        assert_eq!(style.add_modifier.contains(Modifier::BOLD), bold, "{pct}");
    }
}

#[test]
fn segments_sit_two_spaces_apart_with_dim_dots_inside() {
    let app = idle();
    let buf = render_status(&app, 120);
    let rows: Vec<String> = buffer_to_string(&buf).lines().map(String::from).collect();
    assert!(rows[1].contains("c-o-5-5 high  ctx"), "{rows:?}");
    let (x, y) = find_symbols(&buf, " · ").expect("a dot");
    assert_eq!(buf[(x + 1, y)].style().fg, app.theme.dim.fg);
}

#[test]
fn status_bar_hides_thinking_when_toggle_off() {
    let mut app = App::new("test-model".into(), TokenCount::new(200_000));
    app.interaction.status_bar.show_thinking = false;
    let content = status(&app, 120);
    assert!(content.contains("t-m"), "{content}");
    assert!(!content.contains("off"), "{content}");
}

#[test]
fn status_bar_hides_context_when_toggle_off() {
    let mut app = App::new("test-model".into(), TokenCount::new(200_000));
    app.stats.context_tokens = TokenCount::new(10_000);
    app.interaction.status_bar.show_context = false;
    assert!(!status(&app, 120).contains("10k/200k"));
}

#[test]
fn status_bar_hides_cwd_when_toggle_off() {
    let mut app = App::new("test-model".into(), TokenCount::new(200_000));
    app.cwd = "/home/user/dev/mush".into();
    app.interaction.status_bar.show_cwd = false;
    assert!(!status(&app, 120).contains("mush"));
}

#[test]
fn status_bar_hides_oauth_usage_when_toggle_off() {
    let mut app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
    app.oauth_usage_provider = Some("anthropic".into());
    app.oauth_usage = Some(OAuthUsage {
        five_hour: Some(usage_window(37.0, 50.0, OAuthUsage::FIVE_HOUR)),
        seven_day: None,
    });
    app.interaction.status_bar.show_oauth_usage = false;
    assert!(!status(&app, 140).contains("5h"));
}

#[test]
fn status_bar_hides_anthropic_usage_for_codex_model() {
    let mut app = App::new("openai/gpt-5.1-codex".into(), TokenCount::new(200_000));
    app.oauth_usage_provider = Some("anthropic".into());
    app.oauth_usage = Some(OAuthUsage {
        five_hour: Some(usage_window(37.0, 50.0, OAuthUsage::FIVE_HOUR)),
        seven_day: None,
    });
    assert!(
        !status(&app, 140).contains("5h"),
        "stale claude usage leaked"
    );
}

#[test]
fn status_bar_shows_usage_meters() {
    let mut app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
    app.oauth_usage_provider = Some("anthropic".into());
    app.oauth_usage = Some(OAuthUsage {
        five_hour: Some(usage_window(37.0, 50.0, OAuthUsage::FIVE_HOUR)),
        seven_day: Some(usage_window(26.0, 43.0, OAuthUsage::SEVEN_DAY)),
    });
    let content = status(&app, 160);
    for wanted in ["5h", "37%", "7d", "26%"] {
        assert!(content.contains(wanted), "{wanted}: {content}");
    }
}

#[test]
fn a_quota_near_its_limit_stays_at_every_width() {
    let mut app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
    app.oauth_usage_provider = Some("anthropic".into());
    app.oauth_usage = Some(OAuthUsage {
        five_hour: Some(usage_window(83.0, 61.0, OAuthUsage::FIVE_HOUR)),
        seven_day: Some(usage_window(20.0, 50.0, OAuthUsage::SEVEN_DAY)),
    });
    for width in 24..=200 {
        let text = status(&app, width);
        assert!(
            text.contains("5h") && text.contains("83%"),
            "at {width}: {text}"
        );
    }
}

fn render_full_ui(app: &App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut screen = crate::screen::Screen::new();
    terminal
        .draw(|frame| {
            let area = frame.area();
            let _ = status_bar_height(app, area.width);
            let _ = crate::ui::Ui::new(app).cursor_position(area);
            screen.render(app, area, frame.buffer_mut());
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

#[test]
fn the_bar_is_built_once_per_frame() {
    let mut app = App::new("test-model".into(), TokenCount::new(200_000));
    app.stats.input_tokens = TokenCount::new(45_000);
    app.stats.output_tokens = TokenCount::new(12_000);

    reset_build_count();
    let _ = render_full_ui(&app, 120, 20);
    assert_eq!(build_count(), 1, "the bar should be built once per frame");
}

#[test]
fn status_bar_right_side_has_symmetric_trailing_padding() {
    let mut app = App::new("test-model".into(), TokenCount::new(200_000));
    app.cwd = "~/dev/mush".into();
    let buf = render_status(&app, 60);
    let content = buffer_to_string(&buf);
    let row = content.lines().nth(1).unwrap_or("");
    assert!(row.contains("~/dev/mush"), "{row:?}");
    assert!(row.ends_with("~/dev/mush "), "{row:?}");
}

#[test]
fn the_nearest_tier_under_a_minute_turns_amber_and_bold() {
    use ratatui::style::Modifier;
    let mut app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
    app.cache.ttl_secs = 3600;
    app.stats.context_tokens = TokenCount::new(10_000);
    app.cache.refresh();
    app.cache.backdate(backdated(198));
    let buf = render_status(&app, 120);
    let (x, y) = find_symbols(&buf, "0:42").expect("the nearest tier");
    let style = buf[(x, y)].style();
    assert_eq!(style.fg, app.theme.context_warn.fg);
    assert!(style.add_modifier.contains(Modifier::BOLD));

    app.cache.refresh();
    let buf = render_status(&app, 120);
    let (x, y) = find_symbols(&buf, "4:00").expect("a fresh tier");
    assert_eq!(buf[(x, y)].style().fg, app.theme.context_ok.fg);
}

#[test]
fn a_passed_tier_falls_back_to_the_cache_lifetime() {
    let mut app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
    app.cache.ttl_secs = 300;
    app.stats.context_tokens = TokenCount::new(10_000);
    app.cache.refresh();
    app.cache.backdate(backdated(250));
    let text = status(&app, 120);
    assert!(text.contains("cache 0:50"), "{text}");
}

fn open_decision(id: &str, blocking: bool) -> mush_runtime::Decision {
    let asker = mush_runtime::Asker {
        session: "child-s".into(),
        child: Some("sub-child".into()),
        name: "child".into(),
        depth: 1,
        path: Vec::new(),
        ancestors: vec!["root-s".into()],
    };
    mush_runtime::Decision {
        key: mush_runtime::DecisionKey {
            session: asker.session.clone(),
            id: id.into(),
        },
        asker,
        question: format!("{id}?"),
        options: Vec::new(),
        lean: None,
        context: None,
        refs: Vec::new(),
        blocking,
        assume_at: None,
        status: mush_runtime::DecisionStatus::Open,
        asked_at: mush_ai::types::Timestamp::now(),
        updated_at: mush_ai::types::Timestamp::now(),
        revision: 1,
        draft: None,
        delivered: false,
        clarified: None,
    }
}

#[test]
fn decisions_waiting_on_the_user_are_counted_and_never_drop() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    assert!(!status(&app, 200).contains("decision"));
    app.decisions = vec![
        open_decision("push", true),
        open_decision("layout", false),
        open_decision("meters", false),
    ];
    let wide = status(&app, 200);
    assert!(wide.contains("? 3 decisions \u{b7} 1 blocking"), "{wide}");
    let narrow = status(&app, 24);
    assert!(narrow.contains("? 3 \u{b7} 1 blocking"), "{narrow}");
    app.active_tools.push(running_tool("bash"));
    let crowded = status(&app, 24);
    assert!(crowded.contains("?3 !1"), "{crowded}");
    for width in 24..=240 {
        let text = status(&app, width);
        assert!(text.contains('?'), "at {width}: {text}");
    }
    app.decisions.truncate(1);
    app.decisions[0].blocking = false;
    assert!(status(&app, 200).contains("? 1 decision"));
}

#[test]
fn the_model_starts_its_row_at_every_width_and_layout() {
    for (name, app) in states() {
        for width in 40..=240 {
            let rows = status_rows(&app, width);
            let model_row = rows.last().unwrap();
            assert!(
                model_row.starts_with(" c-o-5-5 "),
                "{name} at {width} cols: {rows:?}"
            );
        }
    }
    for layout in [StatusBarLayout::Single, StatusBarLayout::Stacked] {
        let mut app = idle();
        app.interaction.status_bar.layout = layout;
        for width in [60, 100, 139, 140, 180] {
            let rows = status_rows(&app, width);
            assert!(
                rows.last().unwrap().starts_with(" c-o-5-5 "),
                "{layout:?} at {width} cols: {rows:?}"
            );
        }
    }
}
