//! A terminal console for Minecraft servers: log pane, live status header,
//! player sidebar, and a command line with completion, history, and filtering.
//!
//! The crate knows nothing about any particular server. A host feeds it log
//! records and status through a [`ConsoleHandle`], answers completion queries
//! through a [`Completer`], and reads back operator input:
//!
//! ```no_run
//! use pumpkin_tui::{Console, ConsoleConfig, ConsoleInput, Level};
//! use pumpkin_tui::channel::console_channel;
//! use pumpkin_tui::completion::{CommandTree, Node};
//! use std::sync::Arc;
//!
//! let (handle, backend) = console_channel(4096);
//!
//! // The server side: react to what the operator types.
//! let server = handle.clone();
//! std::thread::spawn(move || {
//!     while let Some(input) = server.next_input() {
//!         match input {
//!             ConsoleInput::Command(command) => {
//!                 server.log_message(Level::Info, format!("ran {command}"));
//!             }
//!             ConsoleInput::Quit => {
//!                 server.close();
//!                 break;
//!             }
//!         }
//!     }
//! });
//!
//! let commands = CommandTree::new()
//!     .with(Node::literal("stop").detail("Stop the server"))
//!     .with(Node::literal("say").then(Node::argument("<message>")));
//!
//! Console::new(backend)
//!     .with_completer(Arc::new(commands))
//!     .with_config(ConsoleConfig { title: "Pumpkin".into(), ..Default::default() })
//!     .run()
//!     .unwrap();
//! ```
//!
//! The `plugin` crate alongside this one is the Pumpkin host: it captures the
//! server's log output, answers completion from the real command dispatcher,
//! and hands the console the terminal.

pub mod app;
pub mod backend;
pub mod channel;
pub mod completion;
pub mod history;
pub mod input;
pub mod theme;
pub(crate) mod ui;
mod wrap;

pub use app::{Clock, Console, ConsoleConfig};

/// Hand the terminal back: leave the alternate screen and disable raw mode.
///
/// [`Console::run`] already does this on the way out, including on panic. Call
/// it directly only from a host's own panic or crash handler, where the process
/// is about to print to a terminal the console may still own. It is safe to
/// call when no console is running.
pub fn restore_terminal() {
    ratatui::restore();
}

/// Hand a specific terminal back, for a console started with
/// [`Console::run_on`](app::Console::run_on).
///
/// [`restore_terminal`] writes its escape sequences to stdout, which is the
/// wrong place when the host redirected stdout to capture its own log output.
/// This writes them to `writer` instead. Safe to call more than once.
pub fn restore_on<W: std::io::Write>(mut writer: W) {
    use ratatui::crossterm::event::DisableMouseCapture;
    use ratatui::crossterm::execute;
    use ratatui::crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};

    let _ = execute!(writer, DisableMouseCapture, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

pub use backend::{
    ConsoleBackend, ConsoleEvent, ConsoleInput, Level, LogRecord, PlayerInfo, ServerStatus,
};
pub use channel::{ChannelBackend, ConsoleHandle, console_channel};
pub use completion::{Completer, Completion, CompletionKind, CompletionRequest, Completions};
pub use history::History;
pub use input::LineEditor;
pub use theme::Theme;
