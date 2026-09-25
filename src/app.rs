//! Console state and the terminal event loop.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::Rect;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::Terminal;

use crate::backend::{
    ConsoleBackend, ConsoleEvent, ConsoleInput, Level, LogRecord, PlayerInfo, ServerStatus,
};
use crate::completion::{Completer, CompletionRequest, Completions, NoCompleter};
use crate::history::History;
use crate::input::LineEditor;
use crate::theme::Theme;
use crate::ui;

/// Knobs a host can turn without reimplementing the UI.
#[derive(Clone, Debug)]
pub struct ConsoleConfig {
    /// Shown in the header; usually the server brand.
    pub title: String,
    /// How many log records to keep in memory.
    pub scrollback: usize,
    /// How many command lines to remember.
    pub history_limit: usize,
    /// Where to persist command history, if anywhere.
    pub history_file: Option<PathBuf>,
    /// Redraw/poll interval. 50ms keeps the UI responsive without burning CPU.
    pub tick: Duration,
    /// Start with the player sidebar visible.
    pub show_players: bool,
    /// Records below this level are hidden (toggle at runtime with F4).
    pub min_level: Level,
    /// Enable mouse capture so the wheel scrolls the log. Turning this off
    /// gives the terminal's own text selection back.
    pub mouse: bool,
    /// Echo submitted commands into the log pane.
    pub echo_commands: bool,
    /// The prompt marker drawn before the input line.
    pub prompt: String,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            title: "Pumpkin".to_owned(),
            scrollback: 5_000,
            history_limit: 500,
            history_file: None,
            tick: Duration::from_millis(50),
            show_players: true,
            min_level: Level::Info,
            mouse: true,
            echo_commands: true,
            prompt: "❯".to_owned(),
        }
    }
}

/// Which overlay, if any, is grabbing input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Normal,
    Search,
    Help,
    ConfirmQuit,
}

#[derive(Default)]
pub(crate) struct CompletionState {
    pub open: bool,
    pub items: Completions,
    pub selected: usize,
    /// Remainder of the best candidate, drawn dimmed after the caret.
    pub hint: Option<String>,
}

/// The console UI, driven by a [`ConsoleBackend`].
///
/// ```no_run
/// # use pumpkin_tui::{Console, channel::console_channel};
/// let (handle, backend) = console_channel(1024);
/// std::thread::spawn(move || {
///     while let Some(input) = handle.next_input() {
///         // feed `input` to your command dispatcher
///         let _ = input;
///     }
/// });
/// Console::new(backend).run().unwrap();
/// ```
/// Formats "now" the way the host stamps its own log records.
pub type Clock = Arc<dyn Fn() -> String + Send + Sync>;

pub struct Console<B: ConsoleBackend> {
    backend: B,
    completer: Arc<dyn Completer>,
    config: ConsoleConfig,
    theme: Theme,
    clock: Option<Clock>,
}

impl<B: ConsoleBackend> Console<B> {
    #[must_use]
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            completer: Arc::new(NoCompleter),
            config: ConsoleConfig::default(),
            theme: Theme::default(),
            clock: None,
        }
    }

    /// Supply the timestamp for echoed commands.
    ///
    /// The console never formats a date itself — the host owns that format, and
    /// pulling in a date library to guess at it would only produce stamps that
    /// disagree with the surrounding log. Give it a closure and an echoed
    /// command is stamped exactly like the records around it; leave it out and
    /// the echo simply carries no timestamp.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = Some(clock);
        self
    }

    #[must_use]
    pub fn with_completer(mut self, completer: Arc<dyn Completer>) -> Self {
        self.completer = completer;
        self
    }

    #[must_use]
    pub fn with_config(mut self, config: ConsoleConfig) -> Self {
        self.config = config;
        self
    }

    #[must_use]
    pub const fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Take over the terminal and run until the operator quits or the backend
    /// sends [`ConsoleEvent::Close`].
    ///
    /// Restores the terminal on the way out, including on panic.
    pub fn run(mut self) -> io::Result<()> {
        let mut terminal = ratatui::init();
        if self.config.mouse {
            let _ = execute!(io::stdout(), EnableMouseCapture);
        }
        let result = self.event_loop(&mut terminal);
        if self.config.mouse {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
        ratatui::restore();
        result
    }

    /// Run on an explicit terminal writer instead of this process's stdout.
    ///
    /// A host that has redirected its own stdout — to capture log output that
    /// would otherwise land on top of the frame — hands the console a handle on
    /// the real terminal here. Only drawing moves: raw mode and key events
    /// still come from the controlling tty.
    ///
    /// Unlike [`run`](Self::run) this installs no panic hook, because the host
    /// owns the writer and therefore owns crash recovery. Keep a clone of it
    /// and call [`restore_on`](crate::restore_on) from the host's own hook.
    pub fn run_on<W: Write>(mut self, mut writer: W) -> io::Result<()> {
        enable_raw_mode()?;
        execute!(writer, EnterAlternateScreen)?;
        if self.config.mouse {
            let _ = execute!(writer, EnableMouseCapture);
        }

        let mut terminal = Terminal::new(CrosstermBackend::new(writer))?;
        let result = self.event_loop(&mut terminal);

        // `CrosstermBackend` is itself a `Write`, so the teardown sequence goes
        // to the same terminal without needing the writer back.
        let backend = terminal.backend_mut();
        if self.config.mouse {
            let _ = execute!(backend, DisableMouseCapture);
        }
        let _ = execute!(backend, LeaveAlternateScreen);
        let _ = disable_raw_mode();
        result
    }

    fn event_loop<T: Backend<Error = io::Error>>(&mut self, terminal: &mut Terminal<T>) -> io::Result<()> {
        let mut app = App::new(self.config.clone(), self.theme, Arc::clone(&self.completer));
        app.clock = self.clock.clone();

        while !app.should_quit {
            app.drain_backend(&mut self.backend);
            terminal.draw(|frame| ui::draw(frame, &mut app))?;

            if event::poll(app.config.tick)? {
                let terminal_event = event::read()?;
                for input in app.handle_event(&terminal_event) {
                    self.backend.send(input);
                }
            }
        }

        app.history.save()?;
        Ok(())
    }
}

pub(crate) struct App {
    pub config: ConsoleConfig,
    pub theme: Theme,
    pub records: VecDeque<LogRecord>,
    pub status: ServerStatus,
    pub players: Vec<PlayerInfo>,
    pub mspt_samples: VecDeque<u64>,
    pub editor: LineEditor,
    pub history: History,
    pub completer: Arc<dyn Completer>,
    pub clock: Option<Clock>,
    pub completion: CompletionState,
    pub search: LineEditor,
    pub mode: Mode,
    pub min_level: Level,
    /// Rendered lines between the newest record and the bottom of the pane.
    /// Zero means "following the tail".
    pub scroll: usize,
    pub show_players: bool,
    pub should_quit: bool,
    pub notice: Option<(String, Instant)>,
    /// Filled in by the renderer so paging and the mouse know the pane size.
    pub log_area: Rect,
}

impl App {
    pub(crate) fn new(config: ConsoleConfig, theme: Theme, completer: Arc<dyn Completer>) -> Self {
        let mut history = History::new(config.history_limit);
        let notice = config.history_file.as_ref().and_then(|path| {
            history
                .load(path)
                .err()
                .map(|error| (format!("history: {error}"), Instant::now()))
        });

        Self {
            show_players: config.show_players,
            min_level: config.min_level,
            records: VecDeque::with_capacity(config.scrollback.min(1024)),
            config,
            theme,
            status: ServerStatus::default(),
            players: Vec::new(),
            mspt_samples: VecDeque::new(),
            editor: LineEditor::default(),
            history,
            completer,
            clock: None,
            completion: CompletionState::default(),
            search: LineEditor::default(),
            mode: Mode::Normal,
            scroll: 0,
            should_quit: false,
            notice,
            log_area: Rect::default(),
        }
    }

    /// Pull everything the server has queued since the last frame.
    fn drain_backend(&mut self, backend: &mut impl ConsoleBackend) {
        // Bounded so a flood of log lines can never starve the input loop.
        for _ in 0..512 {
            let Some(event) = backend.poll_event() else {
                break;
            };
            match event {
                ConsoleEvent::Log(record) => self.push_record(record),
                ConsoleEvent::Status(status) => self.set_status(status),
                ConsoleEvent::Players(players) => self.players = players,
                ConsoleEvent::Close => self.should_quit = true,
            }
        }
    }

    pub fn push_record(&mut self, record: LogRecord) {
        if self.records.len() >= self.config.scrollback {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    fn set_status(&mut self, status: ServerStatus) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let sample = status.mspt.max(0.0).round() as u64;
        if self.mspt_samples.len() >= 120 {
            self.mspt_samples.pop_front();
        }
        self.mspt_samples.push_back(sample);
        self.status = status;
    }

    /// Records passing the level filter and the search query.
    pub fn visible_records(&self) -> impl DoubleEndedIterator<Item = &LogRecord> {
        let min = self.min_level;
        let query = self.search.text().to_lowercase();
        self.records.iter().filter(move |record| {
            record.level >= min
                && (query.is_empty() || record.searchable().to_lowercase().contains(&query))
        })
    }

    pub const fn is_following(&self) -> bool {
        self.scroll == 0
    }

    fn notify(&mut self, message: impl Into<String>) {
        self.notice = Some((message.into(), Instant::now()));
    }

    pub fn notice_text(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .and_then(|(text, at)| (at.elapsed() < Duration::from_secs(4)).then_some(text.as_str()))
    }

    fn handle_event(&mut self, event: &Event) -> Vec<ConsoleInput> {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(*key),
            Event::Mouse(mouse) => {
                let page = usize::from(self.log_area.height.max(1));
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_up(3.min(page)),
                    MouseEventKind::ScrollDown => self.scroll_down(3.min(page)),
                    _ => {}
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<ConsoleInput> {
        match self.mode {
            Mode::Help => {
                self.mode = Mode::Normal;
                Vec::new()
            }
            Mode::ConfirmQuit => self.on_confirm_key(key),
            Mode::Search => {
                self.on_search_key(key);
                Vec::new()
            }
            Mode::Normal => self.on_normal_key(key),
        }
    }

    fn on_confirm_key(&mut self, key: KeyEvent) -> Vec<ConsoleInput> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                self.should_quit = true;
                vec![ConsoleInput::Quit]
            }
            // Hammering Ctrl+C is the reflex for "get me out of here". Treating
            // the second press as "no" would leave an operator toggling the
            // prompt on and off with no way out.
            KeyCode::Char('c' | 'd') if ctrl => {
                self.should_quit = true;
                vec![ConsoleInput::Quit]
            }
            _ => {
                self.mode = Mode::Normal;
                Vec::new()
            }
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.search.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => self.search.backspace(),
            KeyCode::Delete => self.search.delete(),
            KeyCode::Left => self.search.move_left(),
            KeyCode::Right => self.search.move_right(),
            KeyCode::Home => self.search.move_home(),
            KeyCode::End => self.search.move_end(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search.kill_to_start();
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search.delete_word_left();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search.insert(c);
            }
            _ => {}
        }
        // Filtering changes what "the bottom" means; go back to following it.
        self.scroll = 0;
    }

    #[allow(clippy::too_many_lines)]
    fn on_normal_key(&mut self, key: KeyEvent) -> Vec<ConsoleInput> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = usize::from(self.log_area.height.max(1))
            .saturating_sub(1)
            .max(1);

        match key.code {
            // ---- session ----
            KeyCode::Char('c') if ctrl => {
                if self.editor.is_empty() {
                    self.mode = Mode::ConfirmQuit;
                } else {
                    self.editor.clear();
                    self.refresh_completions(false);
                }
            }
            KeyCode::Char('d') if ctrl && self.editor.is_empty() => self.mode = Mode::ConfirmQuit,

            // ---- panes and filters ----
            KeyCode::F(1) => self.mode = Mode::Help,
            KeyCode::F(2) => self.show_players = !self.show_players,
            KeyCode::F(3) => self.mode = Mode::Search,
            KeyCode::F(4) => self.cycle_level(),
            KeyCode::F(5) => {
                self.scroll = 0;
                self.notify("following log tail");
            }
            KeyCode::Char('l') if ctrl => {
                self.records.clear();
                self.scroll = 0;
            }

            // ---- scrolling ----
            KeyCode::PageUp => self.scroll_up(page),
            KeyCode::PageDown => self.scroll_down(page),
            KeyCode::Up if shift => self.scroll_up(1),
            KeyCode::Down if shift => self.scroll_down(1),
            KeyCode::Home if ctrl => self.scroll_up(usize::MAX / 2),
            KeyCode::End if ctrl => self.scroll = 0,

            // ---- completion ----
            KeyCode::Tab => self.complete_forward(),
            KeyCode::BackTab => self.complete_backward(),
            KeyCode::Esc => {
                if self.completion.open {
                    self.completion.open = false;
                } else {
                    self.notice = None;
                }
            }

            // ---- history / popup navigation ----
            KeyCode::Up => {
                if self.completion.open {
                    self.move_selection(-1);
                } else if let Some(entry) = self.history.older(self.editor.text()) {
                    let entry = entry.to_owned();
                    self.editor.set(entry);
                    self.refresh_completions(false);
                }
            }
            KeyCode::Down => {
                if self.completion.open {
                    self.move_selection(1);
                } else if let Some(entry) = self.history.newer() {
                    let entry = entry.to_owned();
                    self.editor.set(entry);
                    self.refresh_completions(false);
                }
            }

            // ---- line editing ----
            KeyCode::Enter => return self.submit(),
            KeyCode::Backspace => {
                self.editor.backspace();
                self.after_edit();
            }
            KeyCode::Delete => {
                self.editor.delete();
                self.after_edit();
            }
            KeyCode::Left if ctrl => self.editor.move_word_left(),
            KeyCode::Right if ctrl => self.editor.move_word_right(),
            KeyCode::Left => self.editor.move_left(),
            KeyCode::Right => {
                // At the end of the line, Right accepts the ghost hint.
                if self.editor.at_end()
                    && let Some(hint) = self.completion.hint.clone()
                {
                    self.editor.insert_str(&hint);
                    self.after_edit();
                } else {
                    self.editor.move_right();
                }
            }
            KeyCode::Home => self.editor.move_home(),
            KeyCode::End => {
                if self.editor.at_end()
                    && let Some(hint) = self.completion.hint.clone()
                {
                    self.editor.insert_str(&hint);
                    self.after_edit();
                } else {
                    self.editor.move_end();
                }
            }
            KeyCode::Char('a') if ctrl => self.editor.move_home(),
            KeyCode::Char('e') if ctrl => self.editor.move_end(),
            KeyCode::Char('u') if ctrl => {
                self.editor.kill_to_start();
                self.after_edit();
            }
            KeyCode::Char('k') if ctrl => {
                self.editor.kill_to_end();
                self.after_edit();
            }
            KeyCode::Char('w') if ctrl => {
                self.editor.delete_word_left();
                self.after_edit();
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.editor.insert(c);
                self.after_edit();
            }
            _ => {}
        }

        Vec::new()
    }

    fn after_edit(&mut self) {
        self.history.reset();
        self.refresh_completions(self.completion.open);
    }

    fn submit(&mut self) -> Vec<ConsoleInput> {
        if self.completion.open {
            self.accept_selected();
            return Vec::new();
        }

        let line = self.editor.take();
        let command = line.trim().trim_start_matches('/').to_owned();
        self.completion = CompletionState::default();
        if command.is_empty() {
            return Vec::new();
        }

        self.history.push(line.trim());
        if self.config.echo_commands {
            let mut record =
                LogRecord::new(Level::Info, format!("> {command}")).with_target("console");
            if let Some(clock) = &self.clock {
                record = record.with_time(clock());
            }
            self.push_record(record);
        }
        self.scroll = 0;
        vec![ConsoleInput::Command(command)]
    }

    fn cycle_level(&mut self) {
        let index = Level::ALL
            .iter()
            .position(|level| *level == self.min_level)
            .unwrap_or(0);
        self.min_level = Level::ALL[(index + 1) % Level::ALL.len()];
        self.scroll = 0;
        let level = self.min_level.label().trim();
        self.notify(format!("showing {level} and above"));
    }

    fn scroll_up(&mut self, amount: usize) {
        // The hard bound lives in the renderer, which knows how many wrapped
        // lines actually exist; this just avoids overflowing.
        self.scroll = self.scroll.saturating_add(amount).min(usize::MAX / 2);
    }

    const fn scroll_down(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_sub(amount);
    }

    // ---- completion plumbing ----

    /// Re-query the completer for the current line. `keep_open` decides whether
    /// the popup stays visible; the ghost hint is always refreshed.
    fn refresh_completions(&mut self, keep_open: bool) {
        let line = self.editor.text().to_owned();
        let cursor = self.editor.cursor();
        let items = self.completer.complete(CompletionRequest {
            line: &line,
            cursor,
        });

        let word = &line[items.start.min(line.len())..cursor.min(line.len())];
        self.completion.hint = if self.editor.at_end() {
            items
                .items
                .iter()
                .find(|item| item.insertable() && item.value.len() > word.len())
                .and_then(|item| item.value.strip_prefix(word))
                .map(ToOwned::to_owned)
        } else {
            None
        };

        self.completion.selected = 0;
        self.completion.open = keep_open && !items.is_empty();
        self.completion.items = items;
    }

    fn complete_forward(&mut self) {
        if self.completion.open {
            self.move_selection(1);
            return;
        }

        self.refresh_completions(false);
        let items = &self.completion.items;
        let insertable = items.items.iter().filter(|item| item.insertable()).count();

        match insertable {
            0 => {
                if let Some(placeholder) = items.items.first() {
                    let hint = placeholder.value.clone();
                    self.notify(format!("expects {hint}"));
                }
            }
            1 => self.accept_index(
                items
                    .items
                    .iter()
                    .position(crate::completion::Completion::insertable)
                    .unwrap_or(0),
            ),
            _ => {
                // Fill in as much as every candidate agrees on, then let the
                // operator pick from the popup.
                let start = items.start;
                let cursor = self.editor.cursor();
                if let Some(prefix) = items.common_prefix()
                    && prefix.len() > cursor.saturating_sub(start)
                {
                    self.editor.replace_range(start, cursor, &prefix);
                    self.refresh_completions(false);
                }
                self.completion.open = !self.completion.items.is_empty();
            }
        }
    }

    fn complete_backward(&mut self) {
        if self.completion.open {
            self.move_selection(-1);
        } else {
            self.refresh_completions(true);
            if self.completion.open {
                self.move_selection(-1);
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.completion.items.items.len();
        if count == 0 {
            return;
        }
        let count_i = isize::try_from(count).unwrap_or(isize::MAX);
        let current = isize::try_from(self.completion.selected).unwrap_or(0);
        let next = (current + delta).rem_euclid(count_i);
        self.completion.selected = usize::try_from(next).unwrap_or(0);
    }

    fn accept_selected(&mut self) {
        self.accept_index(self.completion.selected);
    }

    fn accept_index(&mut self, index: usize) {
        let Some(item) = self.completion.items.items.get(index) else {
            return;
        };
        if !item.insertable() {
            self.completion.open = false;
            return;
        }

        let value = item.value.clone();
        let start = self.completion.items.start;
        let cursor = self.editor.cursor();
        self.editor.replace_range(start, cursor, &value);
        // A completed word is nearly always followed by another argument.
        if self.editor.at_end() {
            self.editor.insert(' ');
        }
        self.refresh_completions(false);
    }
}

#[cfg(test)]
mod tests {
    use super::{App, ConsoleConfig, Mode};
    use crate::backend::{ConsoleInput, Level};
    use crate::completion::{CommandTree, Node};
    use crate::theme::Theme;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::Arc;

    fn app() -> App {
        let tree = CommandTree::new()
            .with(
                Node::literal("gamemode").then(Node::argument("<mode>").suggest([
                    "survival",
                    "creative",
                    "spectator",
                ])),
            )
            .with(Node::literal("gamerule"))
            .with(Node::literal("stop"));
        App::new(
            ConsoleConfig {
                history_file: None,
                ..ConsoleConfig::default()
            },
            Theme::default(),
            Arc::new(tree),
        )
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_normal_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    fn press(app: &mut App, code: KeyCode) -> Vec<ConsoleInput> {
        app.on_normal_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn tab_fills_the_common_prefix_and_opens_the_popup() {
        let mut app = app();
        type_text(&mut app, "game");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.editor.text(), "game");
        assert!(app.completion.open);
        assert_eq!(app.completion.items.items.len(), 2);
    }

    #[test]
    fn tab_completes_a_unique_match() {
        let mut app = app();
        type_text(&mut app, "sto");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.editor.text(), "stop ");
        assert!(!app.completion.open);
    }

    #[test]
    fn enter_accepts_the_selected_candidate_instead_of_submitting() {
        let mut app = app();
        type_text(&mut app, "gamemode ");
        press(&mut app, KeyCode::Tab);
        assert!(app.completion.open);
        press(&mut app, KeyCode::Down);
        let sent = press(&mut app, KeyCode::Enter);
        assert!(sent.is_empty());
        assert_eq!(app.editor.text(), "gamemode creative ");
    }

    #[test]
    fn enter_submits_and_echoes() {
        let mut app = app();
        type_text(&mut app, "/stop");
        let sent = press(&mut app, KeyCode::Enter);
        assert!(matches!(&sent[..], [ConsoleInput::Command(cmd)] if cmd == "stop"));
        assert!(app.editor.is_empty());
        assert_eq!(app.history.entries(), ["/stop"]);
        assert!(app.records.back().is_some_and(|r| r.message == "> stop"));
    }

    #[test]
    fn right_arrow_accepts_the_ghost_hint() {
        let mut app = app();
        type_text(&mut app, "sto");
        assert_eq!(app.completion.hint.as_deref(), Some("p"));
        press(&mut app, KeyCode::Right);
        assert_eq!(app.editor.text(), "stop");
    }

    #[test]
    fn filters_hide_records_below_the_level() {
        let mut app = app();
        app.push_record(crate::backend::LogRecord::new(Level::Debug, "chunk saved"));
        app.push_record(crate::backend::LogRecord::new(Level::Warn, "can't keep up"));
        assert_eq!(app.visible_records().count(), 1);
        app.min_level = Level::Trace;
        assert_eq!(app.visible_records().count(), 2);
        app.search.set("chunk");
        assert_eq!(app.visible_records().count(), 1);
    }

    #[test]
    fn ctrl_c_clears_the_line_then_asks_to_quit() {
        let mut app = app();
        type_text(&mut app, "say hi");
        app.on_normal_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.editor.is_empty());
        assert_eq!(app.mode, Mode::Normal);
        app.on_normal_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.mode, Mode::ConfirmQuit);
        let sent = app.on_confirm_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(matches!(&sent[..], [ConsoleInput::Quit]));
        assert!(app.should_quit);
    }

    #[test]
    fn a_second_ctrl_c_confirms_the_quit() {
        let mut app = app();
        app.on_normal_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.mode, Mode::ConfirmQuit);

        let sent = app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(&sent[..], [ConsoleInput::Quit]));
        assert!(app.should_quit);
    }

    #[test]
    fn any_other_key_cancels_the_quit() {
        let mut app = app();
        app.on_normal_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let sent = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(sent.is_empty());
        assert_eq!(app.mode, Mode::Normal);
        assert!(!app.should_quit);
    }
}
