use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use mush_ai::types::{Dollars, ThinkingLevel, TokenCount};
use mush_runtime::{ChildStatus, GoalStatus, JobSource, JobStatus};

use crate::app::App;
use crate::path_utils::truncate_path;
use crate::turn_activity::{TurnReadout, format_elapsed};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_TICKS: usize = 3;
const NOTE_MIN_TEXT: usize = 12;
const NOTE_MIN: usize = 20;
const CWD_MAX: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SegmentId {
    Ready,
    Activity,
    Agents,
    Decisions,
    Goal,
    Auto,
    Jobs,
    Note,
    Alerts,
    Model,
    SubModel,
    Context,
    Cost,
    Tokens,
    Quota,
    Cwd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    Left,
    Right,
}

#[derive(Debug, Clone)]
pub(super) struct Form {
    pub spans: Vec<Span<'static>>,
    pub width: usize,
}

impl Form {
    fn new(spans: Vec<Span<'static>>) -> Self {
        Self::reserving(spans, 0)
    }

    fn reserving(spans: Vec<Span<'static>>, reserve: usize) -> Self {
        let width = spans_width(&spans) + reserve;
        Self { spans, width }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Segment {
    pub id: SegmentId,
    pub side: Side,
    pub pinned: bool,
    pub forms: Vec<Form>,
    pub stretch: Option<Vec<Span<'static>>>,
    pub form: usize,
    pub dropped: bool,
}

impl Segment {
    fn new(id: SegmentId, side: Side, pinned: bool, forms: Vec<Form>) -> Self {
        Self {
            id,
            side,
            pinned,
            forms,
            stretch: None,
            form: 0,
            dropped: false,
        }
    }

    pub(super) fn width(&self) -> usize {
        if self.dropped {
            return 0;
        }
        self.forms.get(self.form).map_or(0, |form| form.width)
    }

    pub(super) fn spans(&self) -> &[Span<'static>] {
        if self.dropped {
            return &[];
        }
        self.forms.get(self.form).map_or(&[], |form| &form.spans)
    }
}

pub(super) struct Segments {
    pub activity: Vec<Segment>,
    pub identity: Vec<Segment>,
    pub timer: Option<String>,
}

pub(super) fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| crate::text::width(span.content.as_ref()))
        .sum()
}

fn text_width(text: &str) -> usize {
    crate::text::width(text)
}

pub(super) fn build(app: &App) -> Segments {
    let mut activity: Vec<Segment> = [
        agents(app),
        decisions(app),
        goal(app),
        autonomous(app),
        jobs(app),
        note(app),
    ]
    .into_iter()
    .flatten()
    .collect();
    if let Some(turn) = turn(app, !activity.is_empty()) {
        activity.insert(0, turn);
    }
    let (context, timer) = context(app);
    let (main_model, sub_model) = model_labels(app);
    let identity = [
        alerts(app),
        Some(model(app, main_model)),
        sub_model.map(|label| submodel(app, label)),
        context,
        cost(app),
        tokens(app),
        quota(app),
        cwd(app),
    ]
    .into_iter()
    .flatten()
    .collect();
    Segments {
        activity,
        identity,
        timer,
    }
}

fn sep(app: &App) -> Span<'static> {
    Span::styled(" · ", app.theme.dim)
}

pub(super) fn spinner(tick: usize) -> &'static str {
    SPINNER_FRAMES[(tick / SPINNER_TICKS) % SPINNER_FRAMES.len()]
}

fn widest_elapsed(elapsed: Duration) -> &'static str {
    match elapsed.as_secs() {
        0..60 => "00s",
        60..3600 => "00m 00s",
        _ => "00h 00m 00s",
    }
}

fn compact_elapsed(elapsed: Duration) -> (String, &'static str) {
    let total = elapsed.as_secs();
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        (format!("{hours}h{minutes:02}m"), "00h00m")
    } else if minutes > 0 {
        (format!("{minutes}m{seconds:02}s"), "00m00s")
    } else {
        (format!("{seconds}s"), "00s")
    }
}

fn widest_tokens(count: u64) -> &'static str {
    match count {
        0..1_000 => "000",
        1_000..10_000 => "0.0k",
        10_000..1_000_000 => "000k",
        _ => "0.0m",
    }
}

fn reserve(text: &str, widest: &str) -> usize {
    text_width(widest).saturating_sub(text_width(text))
}

fn short_verb(verb: &str) -> &str {
    match verb {
        "writing code" => "code",
        crate::turn_activity::WAITING_FOR_NETWORK => "offline",
        other => other.strip_prefix("using ").unwrap_or(other),
    }
}

fn turn(app: &App, others: bool) -> Option<Segment> {
    if !app.interaction.status_bar.show_status_messages {
        return None;
    }
    let readout = app.turn_readout().or_else(|| {
        app.turn.running().then(|| TurnReadout {
            verb: "waiting".into(),
            elapsed: app.turn.elapsed(),
            tokens: app.turn.tokens(0),
            arrow: "↑",
        })
    });
    match readout {
        Some(readout) => Some(busy(app, &readout)),
        None => Some(ready(app, others)),
    }
}

fn busy(app: &App, readout: &TurnReadout) -> Segment {
    let theme = &app.theme;
    let dim = theme.dim;
    let spin = Span::styled(
        format!("{} ", spinner(app.shimmer_phase())),
        theme.status_model,
    );
    let verb = crate::widgets::shimmer::shimmer_spans(
        &readout.verb,
        &theme.shimmer,
        app.shimmer_phase(),
        Style::default(),
    );
    let short = crate::widgets::shimmer::shimmer_spans(
        short_verb(&readout.verb),
        &theme.shimmer,
        app.shimmer_phase(),
        Style::default(),
    );

    let mut base = vec![spin.clone()];
    base.extend(verb);
    let mut base_reserve = 0;
    if let Some(elapsed) = readout.elapsed {
        let text = format_elapsed(elapsed);
        base_reserve = reserve(&text, widest_elapsed(elapsed));
        base.push(sep(app));
        base.push(Span::styled(text, dim));
    }

    let mut long = base.clone();
    let mut medium = base.clone();
    let mut tokens_reserve = 0;
    if readout.tokens > 0 {
        let count = crate::turn_activity::format_tokens(readout.tokens);
        tokens_reserve = reserve(&count, widest_tokens(readout.tokens));
        long.push(sep(app));
        long.push(Span::styled(
            format!("{} {count} tokens", readout.arrow),
            dim,
        ));
        medium.push(sep(app));
        medium.push(Span::styled(format!("{}{count}", readout.arrow), dim));
    }

    let mut terse = vec![spin.clone()];
    terse.extend(short);
    let mut tiny = vec![spin];
    let mut compact_reserve = 0;
    if let Some(elapsed) = readout.elapsed {
        let (text, widest) = compact_elapsed(elapsed);
        compact_reserve = reserve(&text, widest);
        terse.push(Span::styled(format!(" {text}"), dim));
        tiny.push(Span::styled(text, dim));
    }

    Segment::new(
        SegmentId::Activity,
        Side::Left,
        true,
        vec![
            Form::reserving(long, base_reserve + tokens_reserve),
            Form::reserving(medium, base_reserve + tokens_reserve),
            Form::reserving(base, base_reserve),
            Form::reserving(terse, compact_reserve),
            Form::reserving(tiny, compact_reserve),
        ],
    )
}

fn ready(app: &App, others: bool) -> Segment {
    let theme = &app.theme;
    let short = vec![
        Span::styled("◇ ", theme.dim),
        Span::styled("ready", theme.status_muted),
    ];
    let mut forms = Vec::with_capacity(2);
    if let Some(last) = app.turn.last_duration() {
        let mut long = short.clone();
        long.push(sep(app));
        long.push(Span::styled(
            format!("last turn {}", format_elapsed(last)),
            theme.dim,
        ));
        forms.push(Form::new(long));
    }
    forms.push(Form::new(short));
    Segment::new(SegmentId::Ready, Side::Left, !others, forms)
}

fn agents(app: &App) -> Option<Segment> {
    if app.children.is_empty() {
        return None;
    }
    let theme = &app.theme;
    let running = app.running_children();
    let total = app.children.len();
    let waiting = app
        .children
        .iter()
        .filter(|child| child.awaiting_confirmation.is_some())
        .count();
    let failed = app
        .children
        .iter()
        .filter(|child| child.status == ChildStatus::Failed)
        .count();
    let offline = app
        .children
        .iter()
        .filter(|child| child.waiting_for_network.is_some())
        .count();
    let live = if running > 0 {
        theme.context_ok
    } else {
        theme.dim
    };
    let asking = theme.context_warn.add_modifier(Modifier::BOLD);
    let broken = theme.context_danger;

    let worded = |waiting_word: &str, offline_word: &str| {
        let mut spans = Vec::new();
        if waiting > 0 {
            spans.push(sep(app));
            spans.push(Span::styled(format!("{waiting} {waiting_word}"), asking));
        }
        if offline > 0 {
            spans.push(sep(app));
            spans.push(Span::styled(
                format!("{offline} {offline_word}"),
                theme.context_warn,
            ));
        }
        if failed > 0 {
            spans.push(sep(app));
            spans.push(Span::styled(format!("{failed} failed"), broken));
        }
        spans
    };
    let marked = |gap: &str| {
        let mut spans = Vec::new();
        if waiting > 0 {
            spans.push(Span::styled(format!("{gap}?{waiting}"), asking));
        }
        if failed > 0 {
            spans.push(Span::styled(format!("{gap}✗{failed}"), broken));
        }
        spans
    };

    let mut long = vec![
        Span::styled("◆ ", live),
        Span::styled(format!("{running}/{total}"), live),
        Span::styled(" agents", theme.status_muted),
    ];
    long.extend(worded(
        "waiting on you",
        crate::turn_activity::WAITING_FOR_NETWORK,
    ));
    let mut medium = vec![
        Span::styled("◆ ", live),
        Span::styled(format!("{running}/{total}"), live),
    ];
    let mut short = medium.clone();
    medium.extend(worded("waiting", "offline"));
    short.extend(marked(" "));
    let mut tiny = vec![Span::styled(format!("◆{running}"), live)];
    tiny.extend(marked(""));

    Some(Segment::new(
        SegmentId::Agents,
        Side::Left,
        true,
        vec![
            Form::new(long),
            Form::new(medium),
            Form::new(short),
            Form::new(tiny),
        ],
    ))
}

fn decisions(app: &App) -> Option<Segment> {
    let (open, blocking) = crate::decisions::waiting(app);
    if open == 0 {
        return None;
    }
    let theme = &app.theme;
    let count = theme.context_warn.add_modifier(Modifier::BOLD);
    let urgent = theme.context_danger.add_modifier(Modifier::BOLD);
    let noun = if open == 1 { "decision" } else { "decisions" };
    let mut long = vec![
        Span::styled("? ", count),
        Span::styled(format!("{open} {noun}"), count),
    ];
    let mut medium = vec![Span::styled(format!("? {open}"), count)];
    let mut short = vec![Span::styled(format!("?{open}"), count)];
    if blocking > 0 {
        long.push(sep(app));
        long.push(Span::styled(format!("{blocking} blocking"), urgent));
        medium.push(sep(app));
        medium.push(Span::styled(format!("{blocking} blocking"), urgent));
        short.push(Span::styled(format!(" !{blocking}"), urgent));
    }
    Some(Segment::new(
        SegmentId::Decisions,
        Side::Left,
        true,
        vec![Form::new(long), Form::new(medium), Form::new(short)],
    ))
}

fn goal(app: &App) -> Option<Segment> {
    let goal = app.goal.as_ref()?;
    let theme = &app.theme;
    let (style, long, short): (Style, String, String) = match goal.status {
        GoalStatus::Active => {
            let elapsed = Duration::from_secs(goal.elapsed_secs());
            (
                theme.context_ok,
                format!("goal #{} · {}", goal.continuations, format_elapsed(elapsed)),
                format!("goal {}", compact_elapsed(elapsed).0),
            )
        }
        GoalStatus::Paused => (theme.context_warn, "goal paused".into(), "paused".into()),
        GoalStatus::BudgetLimited => (
            theme.context_warn,
            "goal over budget".into(),
            "over budget".into(),
        ),
        GoalStatus::Failed => (theme.context_danger, "goal failed".into(), "failed".into()),
        GoalStatus::Complete => return None,
    };
    let text = if goal.status == GoalStatus::Active {
        theme.status_muted
    } else {
        style
    };
    Some(Segment::new(
        SegmentId::Goal,
        Side::Left,
        true,
        vec![
            Form::new(vec![Span::styled("◎ ", style), Span::styled(long, text)]),
            Form::new(vec![Span::styled("◎ ", style), Span::styled(short, text)]),
            Form::new(vec![Span::styled("◎", style)]),
        ],
    ))
}

fn autonomous(app: &App) -> Option<Segment> {
    let status = app.autonomous.as_ref().filter(|status| status.enabled)?;
    let style = app.theme.status_model;
    Some(Segment::new(
        SegmentId::Auto,
        Side::Left,
        false,
        vec![
            Form::new(vec![Span::styled(
                format!("auto #{}", status.continuations),
                style,
            )]),
            Form::new(vec![Span::styled("auto", style)]),
        ],
    ))
}

fn jobs(app: &App) -> Option<Segment> {
    let theme = &app.theme;
    let heartbeat = app.heartbeat();
    let scheduled = app
        .jobs
        .iter()
        .filter(|job| job.status == JobStatus::Active && job.source != JobSource::Heartbeat)
        .count();
    if heartbeat.is_none() && scheduled == 0 {
        return None;
    }
    let paused = heartbeat.is_some_and(|job| job.status == JobStatus::Paused);
    let glyph = Span::styled(
        "◷ ",
        if paused {
            theme.context_warn
        } else {
            theme.context_ok
        },
    );
    let muted = theme.status_muted;
    let (long, medium) = match heartbeat {
        Some(job) => {
            let (long, medium) = if paused {
                (
                    "heartbeat paused".to_string(),
                    "heartbeat paused".to_string(),
                )
            } else {
                (
                    format!("heartbeat {}", job.expression),
                    "heartbeat".to_string(),
                )
            };
            if scheduled > 0 {
                (
                    format!("{long} · {scheduled} scheduled"),
                    format!("{medium} +{scheduled}"),
                )
            } else {
                (long, medium)
            }
        }
        None => {
            let text = format!("{scheduled} scheduled");
            (text.clone(), text)
        }
    };
    let count = usize::from(heartbeat.is_some()) + scheduled;
    Some(Segment::new(
        SegmentId::Jobs,
        Side::Left,
        false,
        vec![
            Form::new(vec![glyph.clone(), Span::styled(long, muted)]),
            Form::new(vec![glyph.clone(), Span::styled(medium, muted)]),
            Form::new(vec![glyph, Span::styled(count.to_string(), muted)]),
        ],
    ))
}

fn note(app: &App) -> Option<Segment> {
    let (name, note) = app.newest_child_note()?;
    let note = crate::widgets::top_bar::sanitise_title(note);
    if note.is_empty() {
        return None;
    }
    let full = vec![
        Span::styled(format!("{name}: "), app.theme.status_muted),
        Span::styled(note, app.theme.dim),
    ];
    let natural = spans_width(&full);
    let least = natural.min(NOTE_MIN.max(text_width(name) + 2 + NOTE_MIN_TEXT));
    let mut segment = Segment::new(
        SegmentId::Note,
        Side::Left,
        false,
        vec![Form::new(truncate_spans(&full, least))],
    );
    segment.stretch = Some(full);
    Some(segment)
}

fn alerts(app: &App) -> Option<Segment> {
    let mut long = Vec::new();
    let mut short = Vec::new();
    if app.paused.is_some() {
        long.push("paused".to_string());
        short.push("paused".to_string());
    }
    if let Some(holder) = &app.sync.taken_by {
        long.push(format!("read-only, open on {holder}"));
        short.push("read-only".to_string());
    } else if app.sync.problem.is_some() {
        long.push("sync failing".to_string());
        short.push("sync ✗".to_string());
    }
    if long.is_empty() {
        return None;
    }
    let style = app.theme.tool_error.add_modifier(Modifier::BOLD);
    let mut long_spans = Vec::new();
    for (i, text) in long.into_iter().enumerate() {
        if i > 0 {
            long_spans.push(sep(app));
        }
        long_spans.push(Span::styled(text, style));
    }
    Some(Segment::new(
        SegmentId::Alerts,
        Side::Left,
        true,
        vec![
            Form::new(long_spans),
            Form::new(vec![Span::styled(short.join(" "), style)]),
        ],
    ))
}

fn model_labels(app: &App) -> (String, Option<String>) {
    match &app.subagent_model_id {
        Some(sub) => {
            let (main, sub) = super::abbrev::distinct(app.model_id.as_str(), sub.as_str());
            (main, Some(sub))
        }
        None => (super::abbrev::abbreviate(app.model_id.as_str()), None),
    }
}

fn model(app: &App, label: String) -> Segment {
    let theme = &app.theme;
    let bare = vec![Span::styled(label, theme.status_model)];
    let mut effort = bare.clone();
    if app.interaction.status_bar.show_thinking {
        let level = app.thinking_level.normalize_visible();
        let name = level.effort_name().unwrap_or("off").to_string();
        let style = if app.thinking_level == ThinkingLevel::Off {
            theme.dim
        } else {
            theme.status_muted
        };
        effort.push(Span::styled(format!(" {name}"), style));
    }
    let mut accounts = effort.clone();
    for (_, label) in app.accounts.iter() {
        accounts.push(Span::styled(format!(" @{label}"), theme.status_muted));
    }
    Segment::new(
        SegmentId::Model,
        Side::Left,
        false,
        vec![Form::new(accounts), Form::new(effort), Form::new(bare)],
    )
}

fn submodel(app: &App, label: String) -> Segment {
    Segment::new(
        SegmentId::SubModel,
        Side::Left,
        false,
        vec![Form::new(vec![
            Span::styled("sub ", app.theme.dim),
            Span::styled(label, app.theme.status_muted),
        ])],
    )
}

pub(super) fn format_tokens(tokens: TokenCount) -> String {
    let n = tokens.get();
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        format!("{n}")
    }
}

struct Timer {
    long: String,
    short: String,
    style: Style,
    expiring: bool,
    ticking: bool,
    reserve_long: usize,
    reserve_short: usize,
}

fn clock(secs: u16) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn clock_reserve(text: &str) -> usize {
    text.split('/')
        .map(|tier| reserve(tier, "00:00"))
        .sum::<usize>()
}

fn countdowns(app: &App) -> Vec<u16> {
    let tiers = cache_tiers(app).zip(app.cache.elapsed_secs());
    let mut left: Vec<u16> = tiers
        .map(|(tiers, elapsed)| {
            let elapsed = u16::try_from(elapsed).unwrap_or(u16::MAX);
            tiers
                .iter()
                .filter_map(|tier| tier.checked_sub(elapsed))
                .filter(|left| *left > 0)
                .collect()
        })
        .unwrap_or_default();
    if left.is_empty() {
        left.extend(app.cache.remaining_secs().filter(|left| *left > 0));
    }
    left
}

fn timer(app: &App) -> Option<Timer> {
    if app.cache.ttl_secs == 0 {
        return None;
    }
    let theme = &app.theme;
    let countdowns = countdowns(app);
    if let Some(&nearest) = countdowns.first() {
        let expiring = nearest <= crate::app::CACHE_WARN_SECS;
        let style = if expiring {
            theme.context_warn.add_modifier(Modifier::BOLD)
        } else {
            theme.context_ok
        };
        let long = countdowns
            .iter()
            .map(|left| clock(*left))
            .collect::<Vec<_>>()
            .join("/");
        let short = clock(nearest);
        return Some(Timer {
            reserve_long: clock_reserve(&long),
            reserve_short: clock_reserve(&short),
            long,
            short,
            style,
            expiring,
            ticking: true,
        });
    }
    let elapsed = app.cache.elapsed_secs()?;
    (app.cache.remaining_secs() == Some(0)
        && elapsed < u64::from(app.cache.ttl_secs) + crate::app::CACHE_COLD_DISPLAY_SECS)
        .then(|| Timer {
            long: "cold".into(),
            short: "cold".into(),
            style: theme.context_cold,
            expiring: false,
            ticking: false,
            reserve_long: 0,
            reserve_short: 0,
        })
}

fn context(app: &App) -> (Option<Segment>, Option<String>) {
    if !app.interaction.status_bar.show_context || app.stats.context_tokens == TokenCount::ZERO {
        return (None, None);
    }
    let theme = &app.theme;
    let pct = app
        .stats
        .context_tokens
        .percent_of(app.stats.context_window) as u64;
    let (pct_style, count_style) = if pct >= 90 {
        let style = theme.context_danger.add_modifier(Modifier::BOLD);
        (style, theme.context_danger)
    } else if pct >= 70 {
        (theme.context_warn, theme.context_warn)
    } else {
        (Style::default(), theme.dim)
    };
    let tokens = format_tokens(app.stats.context_tokens);
    let window = format_tokens(app.stats.context_window);
    let percent = Span::styled(format!("{pct}%"), pct_style);
    let long = vec![
        Span::styled("ctx ", theme.dim),
        Span::styled(format!("{tokens}/{window} "), count_style),
        percent.clone(),
    ];
    let medium = vec![
        Span::styled(format!("{tokens} "), count_style),
        percent.clone(),
    ];
    let short = vec![percent];

    let Some(timer) = timer(app) else {
        return (
            Some(Segment::new(
                SegmentId::Context,
                Side::Left,
                true,
                vec![Form::new(long), Form::new(medium), Form::new(short)],
            )),
            None,
        );
    };
    let with = |base: &[Span<'static>], labelled: bool| {
        let mut spans = base.to_vec();
        spans.push(sep(app));
        if labelled {
            spans.push(Span::styled("cache ", theme.dim));
            spans.push(Span::styled(timer.long.clone(), timer.style));
        } else {
            spans.push(Span::styled(timer.short.clone(), timer.style));
        }
        spans
    };
    let mut forms = vec![
        Form::reserving(with(&long, true), timer.reserve_long),
        Form::reserving(with(&long, false), timer.reserve_short),
        Form::reserving(with(&medium, false), timer.reserve_short),
        Form::reserving(with(&short, false), timer.reserve_short),
    ];
    if !timer.expiring {
        forms.push(Form::new(short));
    }
    let ticking = timer.ticking.then(|| timer.long.clone());
    (
        Some(Segment::new(SegmentId::Context, Side::Left, true, forms)),
        ticking,
    )
}

fn cache_tiers(app: &App) -> Option<&'static [u16]> {
    let provider = if app.model_id.as_str().contains("codex") {
        mush_ai::types::Provider::Custom("openai-codex".into())
    } else if app.model_id.as_str().starts_with("gpt-") || app.model_id.as_str().contains("/gpt-") {
        mush_ai::types::Provider::Custom("openai".into())
    } else if app.model_id.as_str().contains("claude") {
        mush_ai::types::Provider::Anthropic
    } else {
        return None;
    };

    let retention =
        if matches!(provider, mush_ai::types::Provider::Anthropic) && app.cache.ttl_secs >= 3600 {
            Some(mush_ai::types::CacheRetention::Long)
        } else {
            None
        };

    Some(crate::prompt_cache::cache_warning_tiers(
        &provider,
        retention.as_ref(),
    ))
}

fn cost(app: &App) -> Option<Segment> {
    (app.interaction.show_cost && app.stats.total_cost > Dollars::ZERO).then(|| {
        Segment::new(
            SegmentId::Cost,
            Side::Left,
            false,
            vec![Form::new(vec![Span::styled(
                app.stats.total_cost.to_string(),
                app.theme.dim,
            )])],
        )
    })
}

fn tokens(app: &App) -> Option<Segment> {
    if !app.interaction.show_token_counters {
        return None;
    }
    let counters: Vec<String> = [
        ("↑", app.stats.input_tokens),
        ("↓", app.stats.output_tokens),
        ("R", app.stats.cache_read_tokens),
        ("W", app.stats.cache_write_tokens),
    ]
    .into_iter()
    .filter(|(_, tokens)| *tokens > TokenCount::ZERO)
    .map(|(marker, tokens)| format!("{marker}{}", format_tokens(tokens)))
    .collect();
    (!counters.is_empty()).then(|| {
        Segment::new(
            SegmentId::Tokens,
            Side::Left,
            false,
            vec![Form::new(vec![Span::styled(
                counters.join(" "),
                app.theme.dim,
            )])],
        )
    })
}

fn quota(app: &App) -> Option<Segment> {
    if !app.interaction.status_bar.show_oauth_usage
        || app.oauth_usage_provider.as_deref() != Some("anthropic")
        || !app.model_id.as_str().contains("claude")
    {
        return None;
    }
    let usage = app.oauth_usage.as_ref()?;
    let theme = &app.theme;
    let windows: Vec<(&str, f32, f32)> = [
        (
            "5h",
            usage.five_hour.as_ref(),
            mush_ai::oauth::usage::OAuthUsage::FIVE_HOUR,
        ),
        (
            "7d",
            usage.seven_day.as_ref(),
            mush_ai::oauth::usage::OAuthUsage::SEVEN_DAY,
        ),
    ]
    .into_iter()
    .filter_map(|(label, window, length)| {
        window.map(|window| (label, window.utilization, window.pace(length)))
    })
    .collect();
    let worst = windows.iter().copied().max_by(|a, b| a.1.total_cmp(&b.1))?;
    let percent = |used: f32| {
        let style = if used >= 50.0 {
            crate::widgets::usage_bar::level_style(used, theme)
        } else {
            theme.dim
        };
        Span::styled(format!("{}%", used as u32), style)
    };
    let mut meters = Vec::new();
    let mut numbers = Vec::new();
    for (i, (label, used, pace)) in windows.iter().copied().enumerate() {
        if i > 0 {
            meters.push(sep(app));
            numbers.push(sep(app));
        }
        meters.push(Span::styled(format!("{label} "), theme.dim));
        meters.extend(crate::widgets::usage_bar::render_usage_meter(
            used,
            pace,
            crate::widgets::usage_bar::METER_WIDTH,
            theme,
        ));
        meters.push(Span::raw(" "));
        meters.push(percent(used));
        numbers.push(Span::styled(format!("{label} "), theme.dim));
        numbers.push(percent(used));
    }
    let worst_form = vec![
        Span::styled(format!("{} ", worst.0), theme.dim),
        percent(worst.1),
    ];
    Some(Segment::new(
        SegmentId::Quota,
        Side::Right,
        worst.1 >= 80.0,
        vec![Form::new(meters), Form::new(numbers), Form::new(worst_form)],
    ))
}

fn cwd(app: &App) -> Option<Segment> {
    if !app.interaction.status_bar.show_cwd || app.cwd.is_empty() {
        return None;
    }
    let style = app.theme.status_dim;
    let name = std::path::Path::new(&app.cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&app.cwd)
        .to_string();
    Some(Segment::new(
        SegmentId::Cwd,
        Side::Right,
        false,
        vec![
            Form::new(vec![Span::styled(truncate_path(&app.cwd, CWD_MAX), style)]),
            Form::new(vec![Span::styled(name, style)]),
        ],
    ))
}

pub(super) fn truncate_spans(spans: &[Span<'static>], width: usize) -> Vec<Span<'static>> {
    if spans_width(spans) <= width {
        return spans.to_vec();
    }
    let ellipsis = "…";
    let budget = width.saturating_sub(text_width(ellipsis));
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in spans {
        let kept = crate::text::prefix_within(&span.content, budget.saturating_sub(used));
        used += text_width(kept);
        let kept = kept.to_string();
        let whole = kept.len() == span.content.len();
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
        if !whole {
            break;
        }
    }
    if let Some(last) = out.last_mut() {
        let trimmed = last.content.trim_end().to_string();
        last.content = trimmed.into();
    }
    if width >= text_width(ellipsis) {
        let style = out.last().map_or(Style::default(), |span| span.style);
        out.push(Span::styled(ellipsis, style));
    }
    out
}
