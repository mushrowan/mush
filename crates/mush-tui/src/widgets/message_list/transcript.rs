use std::cell::Ref;
use std::ops::Range;
use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::text::Line;

use super::{
    Cached, MessageStyle, TranscriptChunk, TranscriptChunkId, built_lines, cache_state,
    cached_message_lines, laid_out_lines, streaming_lines,
};
use crate::app::App;
use crate::soft_wrap::Break;

const EAGER_REBUILD_BYTES: usize = 256 * 1024;
const MAX_ARRANGE_ROUNDS: usize = 16;

pub(crate) struct Measure {
    pub(crate) chunks: Vec<TranscriptChunk>,
    pub(crate) total: usize,
    stale: Vec<usize>,
    stale_bytes: usize,
}

pub(crate) fn measure(
    app: &App,
    style: &MessageStyle<'_>,
    width: u16,
    streaming: usize,
) -> Measure {
    let mut measure = Measure {
        chunks: Vec::with_capacity(app.messages.len() + 1),
        total: 0,
        stale: Vec::new(),
        stale_bytes: 0,
    };
    for queued in [false, true] {
        if queued && streaming > 0 {
            measure.push(TranscriptChunkId::Streaming, streaming);
        }
        for (index, message) in app.messages.iter().enumerate() {
            if message.queued != queued {
                continue;
            }
            let (state, height) = cache_state(message, style, width);
            if state == Cached::Stale {
                measure.stale.push(index);
                measure.stale_bytes += message.text_bytes();
            }
            measure.push(TranscriptChunkId::Message(index), height + 1);
        }
    }
    measure
}

impl Measure {
    fn push(&mut self, id: TranscriptChunkId, len: usize) {
        self.chunks.push(TranscriptChunk {
            id,
            start: self.total,
            end: self.total + len,
        });
        self.total += len;
    }
}

pub(crate) fn overlapping(
    chunks: &[TranscriptChunk],
    lines: Range<usize>,
) -> impl Iterator<Item = &TranscriptChunk> {
    let first = chunks.partition_point(|chunk| chunk.end <= lines.start);
    chunks[first..]
        .iter()
        .take_while(move |chunk| chunk.start < lines.end)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Need {
    Shown,
    LaidOut,
}

fn needing(
    app: &App,
    style: &MessageStyle<'_>,
    width: u16,
    chunks: &[TranscriptChunk],
    lines: Range<usize>,
    need: Need,
) -> Vec<usize> {
    if lines.is_empty() {
        return Vec::new();
    }
    overlapping(chunks, lines)
        .filter_map(|chunk| match chunk.id {
            TranscriptChunkId::Message(index) => Some(index),
            TranscriptChunkId::Streaming => None,
        })
        .filter(|index| {
            app.messages.get(*index).is_some_and(|message| {
                let state = cache_state(message, style, width).0;
                match need {
                    Need::Shown => state != Cached::Fresh,
                    Need::LaidOut => state == Cached::Stale,
                }
            })
        })
        .collect()
}

fn build(app: &App, style: &MessageStyle<'_>, width: u16, indices: &[usize], need: Need) {
    for index in indices {
        let Some(message) = app.messages.get(*index) else {
            continue;
        };
        match need {
            Need::Shown => drop(cached_message_lines(message, style, width)),
            Need::LaidOut => drop(laid_out_lines(message, style, width)),
        }
    }
}

pub(crate) struct Arranged {
    pub(crate) chunks: Vec<TranscriptChunk>,
    pub(crate) window: Range<usize>,
}

pub(crate) fn arrange(
    app: &App,
    style: &MessageStyle<'_>,
    width: u16,
    streaming: usize,
    area: Rect,
) -> Arranged {
    let page = usize::from(area.height);
    let mut rounds = 0;
    loop {
        let measure = measure(app, style, width, streaming);
        let window = app.viewport.arrange(&measure.chunks, measure.total, area);
        let shown = needing(
            app,
            style,
            width,
            &measure.chunks,
            window.clone(),
            Need::Shown,
        );
        let around = window.start.saturating_sub(2 * page)..window.end + 2 * page;
        let mut laid = needing(app, style, width, &measure.chunks, around, Need::LaidOut);
        if measure.stale_bytes <= EAGER_REBUILD_BYTES {
            laid.extend(&measure.stale);
        }
        if (shown.is_empty() && laid.is_empty()) || rounds == MAX_ARRANGE_ROUNDS {
            app.render_state
                .transcript_pending
                .set(!measure.stale.is_empty());
            return Arranged {
                chunks: measure.chunks,
                window,
            };
        }
        build(app, style, width, &shown, Need::Shown);
        build(app, style, width, &laid, Need::LaidOut);
        rounds += 1;
    }
}

enum Source<'a> {
    Message(Ref<'a, [Line<'static>]>),
    Streaming,
}

pub(crate) struct Visible<'a> {
    sources: Vec<(TranscriptChunk, Source<'a>)>,
    streaming: &'a [Line<'static>],
    blank: Line<'static>,
}

impl<'a> Visible<'a> {
    pub(crate) fn collect(
        app: &'a App,
        chunks: &[TranscriptChunk],
        lines: Range<usize>,
        streaming: &'a [Line<'static>],
    ) -> Self {
        let sources = overlapping(chunks, lines)
            .filter_map(|chunk| {
                let source = match chunk.id {
                    TranscriptChunkId::Message(index) => {
                        Source::Message(built_lines(app.messages.get(index)?))
                    }
                    TranscriptChunkId::Streaming => Source::Streaming,
                };
                Some((*chunk, source))
            })
            .collect();
        Self {
            sources,
            streaming,
            blank: Line::raw(""),
        }
    }

    pub(crate) fn lines(&self, lines: Range<usize>) -> Vec<&Line<'static>> {
        let mut out = Vec::with_capacity(lines.len());
        let mut sources = self.sources.iter().peekable();
        for line in lines {
            while sources.next_if(|(chunk, _)| chunk.end <= line).is_some() {}
            let Some((chunk, source)) = sources.peek() else {
                break;
            };
            let offset = line.saturating_sub(chunk.start);
            let row = match source {
                Source::Message(built) => built.get(offset),
                Source::Streaming => self.streaming.get(offset),
            };
            out.push(row.unwrap_or(&self.blank));
        }
        out
    }
}

pub(crate) struct Rows {
    pub(crate) base: usize,
    pub(crate) total: usize,
    pub(crate) rows: Vec<String>,
    pub(crate) breaks: Vec<Option<Break>>,
}

pub(crate) fn ensure(
    app: &App,
    style: &MessageStyle<'_>,
    streaming: usize,
    wanted: impl Fn(&crate::screen::Viewport) -> Range<usize>,
) -> Measure {
    let area = app.viewport.area();
    let mut measured = measure(app, style, area.width, streaming);
    for _ in 0..MAX_ARRANGE_ROUNDS {
        let laid = needing(
            app,
            style,
            area.width,
            &measured.chunks,
            wanted(&app.viewport),
            Need::LaidOut,
        );
        if laid.is_empty() {
            break;
        }
        build(app, style, area.width, &laid, Need::LaidOut);
        measured = measure(app, style, area.width, streaming);
        app.viewport.arrange(&measured.chunks, measured.total, area);
    }
    measured
}

pub(crate) fn reach(app: &App, lines: Range<usize>) {
    let style = MessageStyle::for_app(app);
    let streaming = streaming_lines(app, app.viewport.area().width);
    let page = app.viewport.page();
    ensure(app, &style, streaming.len(), |_| {
        lines.start.saturating_sub(page)..lines.end.saturating_add(page)
    });
}

pub(crate) fn rows(app: &App, wanted: impl Fn(&crate::screen::Viewport) -> Range<usize>) -> Rows {
    let style = MessageStyle::for_app(app);
    let streaming = streaming_lines(app, app.viewport.area().width);
    let measured = ensure(app, &style, streaming.len(), &wanted);
    let wanted = wanted(&app.viewport);
    let range = wanted.start.min(measured.total)..wanted.end.min(measured.total);
    let visible = Visible::collect(app, &measured.chunks, range.clone(), &streaming);
    let lines = visible.lines(range.clone());
    Rows {
        base: range.start,
        total: measured.total,
        rows: lines.iter().map(|line| line.to_string()).collect(),
        breaks: lines
            .iter()
            .map(|line| crate::soft_wrap::break_before(line))
            .collect(),
    }
}

pub(crate) fn settle(app: &App, deadline: Instant) -> bool {
    if !app.render_state.transcript_pending.get() {
        return false;
    }
    let style = MessageStyle::for_app(app);
    let width = app.viewport.area().width;
    let anchor = app.viewport.top_message().unwrap_or(app.messages.len());
    let mut built = false;
    for index in outward(anchor, app.messages.len()) {
        let Some(message) = app.messages.get(index) else {
            continue;
        };
        if cache_state(message, &style, width).0 != Cached::Stale {
            continue;
        }
        drop(laid_out_lines(message, &style, width));
        built = true;
        if Instant::now() >= deadline {
            return true;
        }
    }
    app.render_state.transcript_pending.set(false);
    built
}

fn outward(anchor: usize, len: usize) -> impl Iterator<Item = usize> {
    let anchor = anchor.min(len.saturating_sub(1));
    (0..len.max(1)).flat_map(move |step| {
        let below = anchor.checked_add(step).filter(|index| *index < len);
        let above = anchor.checked_sub(step).filter(|_| step > 0);
        below.into_iter().chain(above)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Spot {
    chunk: usize,
    line: usize,
    column: usize,
}

struct ChunkText {
    text: String,
    content: usize,
}

impl ChunkText {
    fn new(rows: &[&Line<'static>], lead: bool, trail: bool) -> Self {
        let mut text = String::new();
        if lead {
            text.push('\n');
        }
        let content = text.len();
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            for span in &row.spans {
                text.push_str(&span.content);
            }
        }
        if trail {
            text.push('\n');
        }
        Self { text, content }
    }

    fn byte_of(&self, line: usize, column: usize) -> usize {
        let body = &self.text[self.content..];
        let mut start = 0;
        for _ in 0..line {
            match body[start..].find('\n') {
                Some(at) => start += at + 1,
                None => return body.len(),
            }
        }
        let row_end = body[start..].find('\n').map_or(body.len(), |at| start + at);
        let row = &body[start..row_end];
        start
            + row
                .char_indices()
                .nth(column)
                .map_or(row.len(), |(byte, _)| byte)
    }

    fn spot_of(&self, chunk: usize, byte: usize) -> Spot {
        let before = &self.text[self.content..self.content + byte];
        let line = before.matches('\n').count();
        let row_start = before.rfind('\n').map_or(0, |at| at + 1);
        Spot {
            chunk,
            line,
            column: before[row_start..].chars().count(),
        }
    }

    fn starts(&self, regex: &regex::Regex) -> Vec<usize> {
        regex
            .find_iter(&self.text)
            .map(|found| found.start())
            .filter(|start| *start >= self.content)
            .map(|start| start - self.content)
            .collect()
    }
}

struct Searcher<'a> {
    app: &'a App,
    style: MessageStyle<'a>,
    width: u16,
    streaming: &'a [Line<'static>],
    ids: Vec<TranscriptChunkId>,
}

impl Searcher<'_> {
    fn text(&self, chunk: usize) -> ChunkText {
        let last = self.ids.len().saturating_sub(1);
        let blank = Line::raw("");
        let (lead, trail) = (chunk > 0, chunk < last);
        match self.ids.get(chunk) {
            Some(TranscriptChunkId::Message(index)) => {
                let Some(message) = self.app.messages.get(*index) else {
                    return ChunkText::new(&[], lead, trail);
                };
                let built = laid_out_lines(message, &self.style, self.width);
                let mut rows: Vec<&Line<'static>> = built.iter().collect();
                rows.push(&blank);
                ChunkText::new(&rows, lead, trail)
            }
            Some(TranscriptChunkId::Streaming) => {
                let rows: Vec<&Line<'static>> = self.streaming.iter().collect();
                ChunkText::new(&rows, lead, trail)
            }
            None => ChunkText::new(&[], lead, trail),
        }
    }

    fn next(&self, at: Spot, regex: &regex::Regex, forward: bool) -> Option<Spot> {
        let count = self.ids.len();
        let order: Vec<usize> = if forward {
            (at.chunk..count).chain(0..=at.chunk).collect()
        } else {
            (0..=at.chunk)
                .rev()
                .chain((at.chunk..count).rev())
                .collect()
        };
        let last = order.len().saturating_sub(1);
        for (step, chunk) in order.into_iter().enumerate() {
            let text = self.text(chunk);
            let starts = text.starts(regex);
            let from = (step == 0 || step == last).then(|| text.byte_of(at.line, at.column));
            let found = match (forward, from) {
                (true, Some(from)) if step == 0 => starts.iter().find(|start| **start > from),
                (true, _) => starts.first(),
                (false, Some(from)) if step == 0 => {
                    starts.iter().rev().find(|start| **start < from)
                }
                (false, _) => starts.last(),
            };
            if let Some(start) = found {
                return Some(text.spot_of(chunk, *start));
            }
        }
        None
    }
}

pub(crate) fn search(
    app: &App,
    regex: &regex::Regex,
    forward: bool,
    count: usize,
) -> Option<(usize, usize)> {
    let style = MessageStyle::for_app(app);
    let area = app.viewport.area();
    let streaming = streaming_lines(app, area.width);
    let at_cursor = |viewport: &crate::screen::Viewport| {
        let line = viewport.cursor().line;
        line..line + 1
    };
    let measured = ensure(app, &style, streaming.len(), at_cursor);
    let cursor = app.viewport.cursor();
    let chunk = measured
        .chunks
        .partition_point(|chunk| chunk.end <= cursor.line)
        .min(measured.chunks.len().checked_sub(1)?);
    let mut at = Spot {
        chunk,
        line: cursor.line.saturating_sub(measured.chunks[chunk].start),
        column: cursor.column,
    };
    let searcher = Searcher {
        app,
        style: MessageStyle::for_app(app),
        width: area.width,
        streaming: &streaming,
        ids: measured.chunks.iter().map(|chunk| chunk.id).collect(),
    };
    for _ in 0..count {
        at = searcher.next(at, regex, forward)?;
    }
    let after = measure(app, &style, area.width, streaming.len());
    if after.chunks != measured.chunks {
        app.viewport.arrange(&after.chunks, after.total, area);
    }
    let chunk = after.chunks.get(at.chunk)?;
    Some((chunk.start + at.line, at.column))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppMode, ChatDetail, DisplayMessage, DisplayToolCall, MessageRole};
    use crate::app_event::Focus;
    use crate::screen::Screen;
    use crate::widgets::message_list::{message_build_count, reset_message_build_count};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use mush_ai::types::TokenCount;
    use ratatui::buffer::Buffer;

    fn long_app(messages: usize) -> App {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let filler = "words that fill the transcript up ".repeat(60);
        for index in 0..messages {
            let mut message = DisplayMessage::new(
                MessageRole::Assistant,
                format!("marker-{index:04} {filler}"),
            );
            let mut tool = DisplayToolCall::new(
                format!("tc-{index}").into(),
                "bash".into(),
                "seq 1 40".into(),
                crate::app::ToolCallStatus::Done,
                0,
            );
            tool.output = Some(
                (1..=40)
                    .map(|line| format!("output line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            message.tool_calls.push(tool);
            app.messages.push(message);
        }
        app
    }

    fn draw(app: &App, screen: &mut Screen, width: u16, height: u16) -> Vec<String> {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        screen.render(app, buf.area, &mut buf);
        let window = app.viewport.area();
        (window.top()..window.bottom())
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    fn settle_all(app: &App) {
        let far = Instant::now() + std::time::Duration::from_secs(3600);
        while settle(app, far) {}
    }

    fn press(app: &mut App, code: KeyCode) {
        crate::input::handle_key(app, KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn read_log(app: &mut App) {
        app.interaction.mode = AppMode::Normal;
        app.interaction.focus = Focus::Log;
        app.viewport.pause();
    }

    #[test]
    fn settling_starts_at_the_anchor_and_works_outward() {
        assert_eq!(outward(3, 6).collect::<Vec<_>>(), [3, 4, 2, 5, 1, 0]);
        assert_eq!(outward(0, 3).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(outward(9, 3).collect::<Vec<_>>(), [2, 1, 0]);
        assert_eq!(outward(0, 0).count(), 0);
    }

    #[test]
    fn a_long_transcript_builds_what_is_on_screen_first_and_settles_the_rest_later() {
        let app = long_app(300);
        let mut screen = Screen::new();
        reset_message_build_count();

        let first = draw(&app, &mut screen, 100, 30);

        let on_screen = message_build_count();
        assert!(on_screen < 30, "built {on_screen} of 300 for one screen");
        assert!(
            first.iter().any(|row| row.contains("output line 40")),
            "{first:#?}"
        );
        assert!(app.render_state.transcript_pending.get());

        settle_all(&app);

        assert_eq!(message_build_count(), 300);
        assert!(!app.render_state.transcript_pending.get());
        reset_message_build_count();
        assert_eq!(draw(&app, &mut screen, 100, 30), first);
        assert_eq!(message_build_count(), 0);
    }

    #[test]
    fn a_detail_change_on_a_long_transcript_redraws_the_screen_without_rebuilding_the_rest() {
        let mut app = long_app(300);
        let mut screen = Screen::new();
        draw(&app, &mut screen, 100, 30);
        settle_all(&app);
        reset_message_build_count();

        app.detail = ChatDetail::All;
        let rows = draw(&app, &mut screen, 100, 30);

        assert!(message_build_count() < 30, "{}", message_build_count());
        assert!(
            rows.iter().any(|row| row.contains("output line 40")),
            "{rows:#?}"
        );
        settle_all(&app);
        assert_eq!(message_build_count(), 300);
    }

    #[test]
    fn a_paused_view_keeps_its_rows_while_the_rest_settles() {
        let mut app = long_app(300);
        let mut screen = Screen::new();
        draw(&app, &mut screen, 100, 30);
        read_log(&mut app);
        app.viewport.scroll_by(-20);
        let before = draw(&app, &mut screen, 100, 30);

        settle_all(&app);

        assert_eq!(draw(&app, &mut screen, 100, 30), before);
    }

    #[test]
    fn a_narrower_window_is_redrawn_without_waiting_for_the_whole_transcript() {
        let app = long_app(300);
        let mut screen = Screen::new();
        draw(&app, &mut screen, 100, 30);
        settle_all(&app);
        reset_message_build_count();

        let rows = draw(&app, &mut screen, 60, 30);

        assert!(message_build_count() < 40, "{}", message_build_count());
        assert!(rows.iter().all(|row| row.chars().count() == 60));
        assert!(
            rows.iter().any(|row| row.contains("output line 40")),
            "{rows:#?}"
        );
    }

    #[test]
    fn a_search_reaches_messages_that_were_never_drawn() {
        let mut app = long_app(300);
        let mut screen = Screen::new();
        draw(&app, &mut screen, 100, 30);
        assert!(app.messages[3].line_cache.borrow().is_none());
        read_log(&mut app);

        search_for(&mut app, "marker-0003");

        let target = crate::normal::LogTarget::new(&app);
        let (line, column) = crate::normal::NormalTarget::cursor(&target);
        let row = &crate::normal::NormalTarget::rows(&target)[line];
        assert_eq!(
            row.chars().skip(column).take(11).collect::<String>(),
            "marker-0003"
        );
        drop(target);
        let rows = draw(&app, &mut screen, 100, 30);
        assert!(
            rows.iter().any(|row| row.contains("marker-0003")),
            "{rows:#?}"
        );
    }

    #[test]
    fn a_word_motion_in_the_log_reads_only_the_rows_around_the_cursor() {
        let mut app = long_app(300);
        let mut screen = Screen::new();
        draw(&app, &mut screen, 100, 30);
        read_log(&mut app);
        reset_message_build_count();

        press(&mut app, KeyCode::Char('b'));
        press(&mut app, KeyCode::Char('w'));

        assert!(message_build_count() < 30, "{}", message_build_count());
        assert!(app.messages[0].line_cache.borrow().is_none());
    }

    fn search_for(app: &mut App, pattern: &str) {
        press(app, KeyCode::Char('/'));
        for c in pattern.chars() {
            press(app, KeyCode::Char(c));
        }
        press(app, KeyCode::Enter);
    }

    fn cursor_row(app: &App) -> String {
        let target = crate::normal::LogTarget::new(app);
        let (line, _) = crate::normal::NormalTarget::cursor(&target);
        crate::normal::NormalTarget::rows(&target)[line].clone()
    }

    #[test]
    fn a_mark_follows_its_message_when_the_transcript_rewraps() {
        let mut app = long_app(300);
        let mut screen = Screen::new();
        draw(&app, &mut screen, 100, 30);
        read_log(&mut app);
        search_for(&mut app, "marker-0290");
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Char('a'));
        assert!(cursor_row(&app).contains("marker-0290"));

        draw(&app, &mut screen, 70, 30);
        settle_all(&app);
        draw(&app, &mut screen, 70, 30);
        press(&mut app, KeyCode::Char('G'));
        press(&mut app, KeyCode::Char('`'));
        press(&mut app, KeyCode::Char('a'));

        assert!(
            cursor_row(&app).contains("marker-0290"),
            "{}",
            cursor_row(&app)
        );
    }
}
