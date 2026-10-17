use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mush_ai::types::TokenCount;
use mush_session::{SessionId, SessionStore};
use mush_tui::app::{App, ChatDetail};
use mush_tui::screen::Screen;
use mush_tui::ui::Ui;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const USAGE: &str = "usage: replay_session <session.json | session.history.jsonl> \
[--whole] [--size WxH] [--rounds N] [--only SCENARIO,...] [--settle] [--perf-fifo PATH]\n\
scenarios: typing, detail, normal, log, search, resize, scroll, idle";
const SCENARIOS: &[&str] = &[
    "typing", "detail", "normal", "log", "search", "resize", "scroll", "idle",
];
const RESUME_WINDOW: usize = 2000;
const SETTLE_SLICE: Duration = Duration::from_millis(12);
const IDLE_FRAMES: u32 = 30;

struct Options {
    session: PathBuf,
    whole: bool,
    width: u16,
    height: u16,
    rounds: usize,
    only: Option<Vec<String>>,
    settle: bool,
    perf_fifo: Option<PathBuf>,
}

enum Request {
    Replay(Options),
    Help,
}

impl Options {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Request, String> {
        let mut options = Self {
            session: PathBuf::new(),
            whole: false,
            width: 200,
            height: 60,
            rounds: 1,
            only: None,
            settle: false,
            perf_fifo: None,
        };
        let mut session = None;
        while let Some(arg) = args.next() {
            let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
            match arg.as_str() {
                "--whole" => options.whole = true,
                "--settle" => options.settle = true,
                "--size" => {
                    let size = value("--size")?;
                    let (width, height) = size
                        .split_once('x')
                        .ok_or(format!("--size takes WxH, not {size}"))?;
                    options.width = parse_number(width, "--size")?;
                    options.height = parse_number(height, "--size")?;
                }
                "--rounds" => options.rounds = parse_number(&value("--rounds")?, "--rounds")?,
                "--only" => {
                    let only: Vec<String> =
                        value("--only")?.split(',').map(str::to_string).collect();
                    if let Some(unknown) =
                        only.iter().find(|name| !SCENARIOS.contains(&name.as_str()))
                    {
                        return Err(format!("unknown scenario {unknown}"));
                    }
                    options.only = Some(only);
                }
                "--perf-fifo" => options.perf_fifo = Some(value("--perf-fifo")?.into()),
                "-h" | "--help" => return Ok(Request::Help),
                flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
                path if session.is_none() => session = Some(PathBuf::from(path)),
                extra => return Err(format!("unexpected argument {extra}")),
            }
        }
        options.session = session.ok_or("no session path given")?;
        Ok(Request::Replay(options))
    }

    fn runs(&self, scenario: &str) -> bool {
        self.only
            .as_ref()
            .is_none_or(|only| only.iter().any(|name| name == scenario))
    }
}

fn parse_number<T: std::str::FromStr>(text: &str, flag: &str) -> Result<T, String> {
    text.parse()
        .map_err(|_| format!("{flag} takes a number, not {text}"))
}

fn session_location(path: &Path) -> Result<(PathBuf, SessionId), String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("the session path has no file name")?;
    let id = name
        .strip_suffix(".history.jsonl")
        .or_else(|| name.strip_suffix(".json"))
        .ok_or("the session path should end in <id>.json or <id>.history.jsonl")?;
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok((dir.to_path_buf(), SessionId::from(id)))
}

struct Rig {
    app: App,
    screen: Screen,
    terminal: Terminal<TestBackend>,
    settle: bool,
}

impl Rig {
    fn draw(&mut self) -> Duration {
        let start = Instant::now();
        let Self {
            app,
            screen,
            terminal,
            ..
        } = self;
        let _ = terminal.draw(|frame| {
            app.flush_status();
            app.render_state.status_bar_cache.borrow_mut().take();
            let area = frame.area();
            let cursor = Ui::new(app).cursor_position(area);
            screen.render(app, area, frame.buffer_mut());
            frame.set_cursor_position(cursor);
        });
        start.elapsed()
    }

    fn press(&mut self, label: &str, code: KeyCode, modifiers: KeyModifiers) {
        let start = Instant::now();
        let _ = mush_tui::input::handle_key(&mut self.app, KeyEvent::new(code, modifiers));
        let handled = start.elapsed();
        let drawn = self.draw();
        println!(
            "{label:<36} key {:>9}  draw {:>9}  total {:>9}",
            ms(handled),
            ms(drawn),
            ms(handled + drawn)
        );
    }

    fn key(&mut self, label: &str, code: KeyCode) {
        self.press(label, code, KeyModifiers::NONE);
    }

    fn ctrl(&mut self, label: &str, c: char) {
        self.press(label, KeyCode::Char(c), KeyModifiers::CONTROL);
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.terminal.backend_mut().resize(width, height);
        let drawn = self.draw();
        println!(
            "{:<36} draw {:>9}",
            format!("resize to {width}x{height}"),
            ms(drawn)
        );
        self.report_settle();
    }

    fn report_settle(&mut self) {
        if !self.settle {
            return;
        }
        let start = Instant::now();
        let mut slices = 0;
        while mush_tui::widgets::message_list::settle_transcript(
            &self.app,
            Instant::now() + SETTLE_SLICE,
        ) {
            slices += 1;
        }
        let took = start.elapsed();
        println!(
            "{:<36} settled {:>9} in {slices} slices, next draw {:>9}",
            "",
            ms(took),
            ms(self.draw())
        );
    }

    fn detail_to(&mut self, detail: ChatDetail) {
        self.app.detail = detail;
        self.draw();
        self.report_settle();
    }
}

fn ms(duration: Duration) -> String {
    format!("{:.1}ms", duration.as_secs_f64() * 1000.0)
}

fn enable_perf(fifo: &Path) {
    use std::io::Write;
    match std::fs::OpenOptions::new().write(true).open(fifo) {
        Ok(mut fifo) => {
            let _ = writeln!(fifo, "enable");
        }
        Err(error) => eprintln!("could not open {}: {error}", fifo.display()),
    }
}

fn typing(rig: &mut Rig) {
    for c in "hello world".chars() {
        rig.key(&format!("type {c:?}"), KeyCode::Char(c));
    }
    for _ in 0..3 {
        rig.key("backspace", KeyCode::Backspace);
    }
}

fn detail(rig: &mut Rig) {
    for pass in ["", " again"] {
        for _ in 0..3 {
            let label = format!("ctrl+r to {}{pass}", rig.app.detail.next().label());
            rig.ctrl(&label, 'r');
            rig.report_settle();
        }
    }
}

fn normal(rig: &mut Rig) {
    rig.key("esc to normal mode", KeyCode::Esc);
    rig.key("u (undo)", KeyCode::Char('u'));
    rig.ctrl("ctrl+r (redo)", 'r');
    rig.key("i back to insert", KeyCode::Char('i'));
}

fn log(rig: &mut Rig) {
    rig.ctrl("ctrl+o into the log", 'o');
    for c in ['k', 'k'] {
        rig.key(&format!("log {c}"), KeyCode::Char(c));
    }
    for _ in 0..2 {
        rig.ctrl("log ctrl+u", 'u');
    }
    for c in ['w', 'b', 'e', '$'] {
        rig.key(&format!("log {c}"), KeyCode::Char(c));
    }
    for _ in 0..3 {
        let label = format!("log ctrl+r to {}", rig.app.detail.next().label());
        rig.ctrl(&label, 'r');
        rig.report_settle();
    }
    rig.key("log g", KeyCode::Char('g'));
    rig.key("log gg", KeyCode::Char('g'));
    rig.ctrl("log ctrl+d", 'd');
    rig.press("log G", KeyCode::Char('G'), KeyModifiers::SHIFT);
    rig.key("log k", KeyCode::Char('k'));
    rig.key("log /", KeyCode::Char('/'));
    for c in "the".chars() {
        rig.key(&format!("log / {c:?}"), KeyCode::Char(c));
    }
    rig.key("log / enter", KeyCode::Enter);
    rig.key("log n", KeyCode::Char('n'));
    rig.press("log N", KeyCode::Char('N'), KeyModifiers::SHIFT);
    rig.key("log v", KeyCode::Char('v'));
    rig.key("log v j", KeyCode::Char('j'));
    rig.key("log esc", KeyCode::Esc);
    rig.ctrl("ctrl+o back to the composer", 'o');
    rig.key("i back to insert", KeyCode::Char('i'));
}

fn search(rig: &mut Rig) {
    rig.ctrl("ctrl+f search box", 'f');
    for c in "error".chars() {
        rig.key(&format!("search {c:?}"), KeyCode::Char(c));
    }
    rig.ctrl("search ctrl+r", 'r');
    rig.key("search backspace", KeyCode::Backspace);
    rig.key("search esc", KeyCode::Esc);
}

fn resize(rig: &mut Rig, width: u16, height: u16) {
    let narrow = width.saturating_sub(40).max(20);
    let short = height.saturating_sub(10).max(10);
    let sizes = [
        (width.saturating_sub(1).max(20), height),
        (narrow, height),
        (narrow, short),
        (width, height),
        ((width / 2).max(20), (height / 2).max(10)),
        (width, height),
    ];
    let start = rig.app.detail;
    for detail in [start, ChatDetail::All] {
        rig.detail_to(detail);
        println!("{:<36} at {}", "resizing", detail.label());
        for (w, h) in sizes {
            rig.resize(w, h);
        }
    }
    rig.detail_to(start);
}

fn scroll(rig: &mut Rig) {
    for _ in 0..3 {
        rig.key("page up", KeyCode::PageUp);
    }
    rig.key("page down", KeyCode::PageDown);
}

fn idle(rig: &mut Rig) {
    let start = Instant::now();
    for _ in 0..IDLE_FRAMES {
        rig.draw();
    }
    println!(
        "{:<36} {} per frame",
        format!("idle redraw x{IDLE_FRAMES}"),
        ms(start.elapsed() / IDLE_FRAMES)
    );
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(Request::Replay(options)) => options,
        Ok(Request::Help) => {
            println!("{USAGE}");
            return Ok(());
        }
        Err(message) => return Err(format!("{message}\n{USAGE}")),
    };
    let (dir, id) = session_location(&options.session)?;

    let start = Instant::now();
    let session = SessionStore::new(dir)
        .load(&id)
        .map_err(|error| format!("could not load {}: {error}", options.session.display()))?;
    let tree = session.conversation.tree();
    let messages = if options.whole {
        tree.build_display_history()
    } else {
        tree.recent_display_history(RESUME_WINDOW).messages
    };
    println!(
        "loaded {} entries, replaying {} messages, in {}",
        tree.entries().len(),
        messages.len(),
        ms(start.elapsed())
    );

    let mut app = App::new(session.meta.model_id.clone(), TokenCount::new(200_000));
    let start = Instant::now();
    mush_tui::slash::rebuild_display(&mut app, &messages);
    println!(
        "rebuilt {} transcript rows in {}",
        app.messages.len(),
        ms(start.elapsed())
    );
    drop(session);

    let Ok(terminal) = Terminal::new(TestBackend::new(options.width, options.height));
    let mut rig = Rig {
        app,
        screen: Screen::new(),
        terminal,
        settle: options.settle,
    };
    println!("{:<36} draw {:>9}", "first draw", ms(rig.draw()));
    rig.report_settle();
    println!("{:<36} draw {:>9}", "second draw", ms(rig.draw()));

    if let Some(fifo) = &options.perf_fifo {
        enable_perf(fifo);
    }
    for round in 0..options.rounds {
        if round > 0 {
            println!("round {}", round + 1);
        }
        if options.runs("typing") {
            typing(&mut rig);
        }
        if options.runs("detail") {
            detail(&mut rig);
        }
        if options.runs("normal") {
            normal(&mut rig);
        }
        if options.runs("log") {
            log(&mut rig);
        }
        if options.runs("search") {
            search(&mut rig);
        }
        if options.runs("resize") {
            resize(&mut rig, options.width, options.height);
        }
        if options.runs("scroll") {
            scroll(&mut rig);
        }
        if options.runs("idle") {
            idle(&mut rig);
        }
    }
    Ok(())
}
