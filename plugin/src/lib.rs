//! A full-screen console for Pumpkin, loaded as a native plugin.
//!
//! Pumpkin's WASM plugins are sandboxed: no stdin, no terminal control, no view
//! of the server's log stream. A native plugin has none of those limits — it is
//! a dynamic library running inside the server process, so it can take the
//! terminal, read the log output, and talk to the command dispatcher directly.
//!
//! What happens on load:
//!
//! 1. The rustyline editor is taken out of the shared logger, so the server's
//!    own console never starts reading the terminal.
//! 2. fd 1 and 2 are pointed at a pipe, and a reader thread turns everything
//!    the server prints into console log records. See [`capture`].
//! 3. The UI runs on a dedicated thread, drawing to a dup of the real terminal.
//! 4. Commands typed into it go straight to the command dispatcher, on the
//!    thread that read them.
//!
//! The server's built-in console has to be off for this to work — see the
//! README for the two lines of configuration.
//!
//! # The dynamic library boundary
//!
//! The server binary exports no dynamic symbols, so this library carries its
//! own compiled copy of `pumpkin` and of `tokio`. Values that arrive by pointer
//! — `Arc<Context>`, `Arc<Server>` — are the real ones, and so is anything
//! reached through them whose vtables were built by the binary. Statics and
//! thread-locals are *not* shared. Two consequences shape the code below:
//!
//! * **No tokio primitives of our own.** A future spawned with `spawn_task`
//!   runs on the binary's runtime, where our copy of tokio has no registered
//!   driver, so `tokio::time::interval` panics with "there is no reactor
//!   running". Timing is done with plain threads instead.
//! * **No tokio *types* across the boundary either.** `Server::runtime` is a
//!   `tokio::runtime::Handle`, and `Handle`'s internals are feature-gated. This
//!   library's tokio features resolve independently of the server's, so the two
//!   layouts need not agree — which makes `Server::spawn_task` unsound from
//!   here, in the quiet way where tasks simply never run. Commands are
//!   dispatched synchronously on our own thread instead: `handle_command` is a
//!   sync function, and the executors it reaches are the binary's own.
//! * **`pumpkin::stop_server()` does nothing from here.** It would set the
//!   `SHOULD_STOP` flag in *this* library's copy of the crate, which the server
//!   never reads. Stopping goes through the dispatcher's own `stop` command,
//!   whose executor the binary registered, so the flag that gets set is the
//!   real one.
//!
//! The same applies to `tracing`: our copy has no subscriber until
//! `Context::init_log` installs one, which is what that method is for. It writes
//! to stderr, which we capture, so the plugin's own log lines arrive in the
//! console alongside the server's.
//!
//! One limitation follows from it that is worth knowing about: `std`'s panic
//! hook is a static too, so the hook installed below covers panics raised in
//! this library's threads but not panics elsewhere in the server. Those print
//! Pumpkin's crash report — into the capture pipe — and can leave the terminal
//! in raw mode.

// `#[plugin_method]` expands to `block_on(async { Box::pin(async { .. }) })`,
// which builds the future without polling it. The shape is the macro's, not
// ours, and the lint has no way to see that.
#![allow(clippy::async_yields_async)]
// Same reason: the macro re-parses our method bodies at the `#[plugin_impl]`
// site, so this fires on an `Arc<Server>` clone it cannot attribute properly.
#![allow(clippy::significant_drop_tightening)]

mod capture;
mod completer;

use std::io::IsTerminal;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pumpkin::command::CommandSender;
use pumpkin::plugin::Context;
use pumpkin::server::Server;
use pumpkin_api_macros::{plugin_impl, plugin_method};
use pumpkin_tui::{
    Console, ConsoleConfig, ConsoleHandle, ConsoleInput, Level, PlayerInfo, ServerStatus,
    console_channel,
};
use pumpkin_world::CURRENT_MC_VERSION;

use crate::capture::{Capture, LogFormat};
use crate::completer::DispatcherCompleter;

/// How often the header and the player sidebar are refreshed.
const STATUS_INTERVAL: Duration = Duration::from_secs(1);

/// Records the console keeps in scrollback.
const SCROLLBACK: usize = 8_192;

#[plugin_method]
async fn on_load(&self, context: Arc<Context>) -> Result<(), String> {
    // Our copy of `tracing` has no subscriber of its own; this gives it one,
    // writing to stderr, which the capture below picks up.
    context.init_log();

    // Refuse anywhere there is no operator at a terminal: a control panel, a
    // systemd unit, a container started without a pty, a CI run. Drawing a
    // full-screen UI into a pipe that something else is parsing breaks that
    // something else — see the README on control panels.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(
            "pumpkin-tui needs a terminal on stdin and stdout, and this process has none.              Leaving the console alone."
                .to_owned(),
        );
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term.is_empty() || term == "dumb" {
        return Err(format!(
            "pumpkin-tui needs a capable terminal, but TERM is {}. Leaving the console alone.",
            if term.is_empty() { "unset" } else { "`dumb`" }
        ));
    }

    let server = context.server.clone();

    // Two consoles cannot share one terminal. Failing the load here is the safe
    // outcome: the server keeps its normal console and says why.
    if server.advanced_config.commands.use_console {
        return Err(
            "pumpkin-tui needs the built-in console switched off. Set `use_console = false` \
             and `use_tty = false` under [commands] in pumpkin.toml, then restart."
                .to_owned(),
        );
    }

    // Taking the editor stops rustyline's external printer from being used, so
    // log records go straight to fd 1 where we can capture them. The logging
    // config tells us how those lines are formatted.
    let (format, clock) = if let Some(Some((wrapper, _, logging))) = context.logger.get() {
        drop(wrapper.take_readline());
        (
            LogFormat {
                timestamp: logging.timestamp,
                target: logging.target,
            },
            // The struct behind `LOGGER_IMPL` carries only the flags; the
            // format string itself lives in the full config.
            logging
                .timestamp
                .then(|| {
                    clock_matching(&server.advanced_config.logging.timestamp_format)
                })
                .flatten(),
        )
    } else {
        (
            LogFormat {
                timestamp: true,
                target: false,
            },
            None,
        )
    };

    let (capture, pipe) = Capture::redirect().map_err(|error| {
        format!("pumpkin-tui could not take over the terminal: {error}")
    })?;
    let terminal = capture
        .terminal()
        .map_err(|error| format!("pumpkin-tui could not open the terminal: {error}"))?;

    // A panic anywhere in the server would otherwise leave the terminal in raw
    // mode with the alternate screen still up, and send the panic message into
    // a pipe nobody is reading.
    install_panic_hook(&terminal);

    let (handle, backend) = console_channel(SCROLLBACK);

    let log_handle = handle.clone();
    spawn_named("pumpkin-tui-log", move || {
        capture::pump(pipe, &log_handle, format);
    })?;

    spawn_status_thread(&server, &handle)?;
    spawn_input_thread(&server, &handle)?;

    let completer = Arc::new(DispatcherCompleter::new(server.clone()));
    let config = ConsoleConfig {
        title: "Pumpkin".to_owned(),
        scrollback: SCROLLBACK,
        history_file: Some("logs/console_history".into()),
        min_level: Level::Info,
        ..Default::default()
    };

    let console = spawn_named("pumpkin-tui", move || {
        let mut console = Console::new(backend)
            .with_completer(completer)
            .with_config(config);
        if let Some(clock) = clock {
            console = console.with_clock(clock);
        }
        let result = console.run_on(terminal);

        // Whichever way the console ended, the terminal belongs to the server
        // again before anything else is printed.
        capture.restore();

        // Nothing to stop here: a console the operator quit has already sent
        // `ConsoleInput::Quit`, and one closed by `on_unload` is being torn down
        // by a shutdown that is already under way. An error leaves the server
        // running with no console, which beats killing it.
        if let Err(error) = result {
            tracing::error!("pumpkin-tui stopped: {error}; the server keeps running headless");
        }
    })?;

    let mut slot = self
        .console
        .lock()
        .map_err(|_| "pumpkin-tui state is poisoned".to_owned())?;
    *slot = Some(Running { console, handle });
    drop(slot);

    Ok(())
}

#[plugin_method]
async fn on_unload(&self, _context: Arc<Context>) -> Result<(), String> {
    let mut slot = self
        .console
        .lock()
        .map_err(|_| "pumpkin-tui state is poisoned".to_owned())?;
    let running = slot.take();
    drop(slot);

    let Some(running) = running else {
        return Ok(());
    };

    // Closing the channel is what makes `Console::run_on` return. Without it a
    // shutdown the operator did not start — `/stop`, or a signal — would leave
    // the console thread blocked on the next keystroke, and the join below
    // would hang the server's shutdown with it.
    running.handle.close();

    // The console thread restores the terminal on its way out, so waiting for
    // it means the remaining shutdown log lines land on a normal screen.
    let _ = running.console.join();
    Ok(())
}

/// Push the header numbers and the player sidebar on a fixed interval.
///
/// A plain thread, not a task: a `tokio::time` interval created here would look
/// for a driver in this library's copy of tokio and find none.
fn spawn_status_thread(server: &Arc<Server>, handle: &ConsoleHandle) -> Result<(), String> {
    let handle = handle.clone();
    let server = server.clone();

    spawn_named("pumpkin-tui-status", move || {
        let started = Instant::now();
        let mut system = sysinfo::System::new();

        loop {
            system.refresh_memory();

            let players = server.get_all_players();
            let status = ServerStatus {
                brand: "Pumpkin".to_owned(),
                version: CURRENT_MC_VERSION.to_owned(),
                tps: effective_tps(&server),
                mspt: server.get_mspt(),
                players_online: u32::try_from(players.len()).unwrap_or(u32::MAX),
                players_max: server.advanced_config.networking.java.max_players,
                memory_used_mb: system.used_memory() / 1_048_576,
                memory_total_mb: system.total_memory() / 1_048_576,
                chunks_loaded: 0,
                uptime: started.elapsed(),
            };

            let sidebar = players
                .iter()
                .map(|player| PlayerInfo {
                    name: player.gameprofile.name.clone(),
                    world: player.world().level_info.load().level_name.clone(),
                    ping_ms: player.ping.load(Ordering::Relaxed),
                })
                .collect();

            // Either call failing means the console is gone.
            if !handle.set_status(status) || !handle.set_players(sidebar) {
                break;
            }
            std::thread::sleep(STATUS_INTERVAL);
        }
    })?;

    Ok(())
}

/// Ticks per second as an operator reads it: how fast the server *is* running.
///
/// `Server::get_tps` is `1000 / mspt`, and mspt measures the work in a tick
/// rather than the interval between ticks. On an idle server a tick's work takes
/// tens of microseconds, so the raw figure comes out in the thousands — it is
/// throughput the server could sustain, not the rate it runs at. The server
/// sleeps out the rest of each tick, so the real rate is capped by the
/// configured tick rate, which is what `/tps` means everywhere else.
fn effective_tps(server: &Server) -> f64 {
    let target = f64::from(server.tick_rate_manager.tickrate());
    server.get_tps().min(target)
}

/// Feed what the operator types back into the server.
///
/// `next_input` blocks, so it gets a thread, and each command runs to completion
/// on that same thread before the next line is read — which is also the ordering
/// the stock console has.
fn spawn_input_thread(server: &Arc<Server>, handle: &ConsoleHandle) -> Result<(), String> {
    let handle = handle.clone();
    let server = server.clone();

    spawn_named("pumpkin-tui-input", move || {
        while let Some(input) = handle.next_input() {
            match input {
                ConsoleInput::Command(line) => dispatch(&server, &line),
                // The operator confirmed Ctrl+C or Ctrl+D. This is a request to
                // stop, so the server runs its normal shutdown; the console is
                // closed by `on_unload` on the way down.
                ConsoleInput::Quit => {
                    request_stop();
                    break;
                }
            }
        }
    })?;

    Ok(())
}

/// Run one console command.
///
/// Deliberately synchronous. Handing a future to `Server::spawn_task` would mean
/// this library's tokio interpreting the server's `Handle`, and the two layouts
/// are not guaranteed to match — see the note at the top of this file. Walking
/// the dispatcher touches only plain data and the binary's own executors.
///
/// The cost is that `ServerCommandEvent` is not fired for console commands,
/// because firing it is `async`: a plugin that cancels console commands will not
/// see these. Commands from players are unaffected.
fn dispatch(server: &Arc<Server>, line: &str) {
    server
        .command_dispatcher
        .load()
        .handle_command(&CommandSender::Console.into_source(server), line);
}

/// Ask the server to shut down, the way Ctrl+C always has.
///
/// `pumpkin::stop_server()` is not callable from here — it would set the flag in
/// this library's copy of the crate. Raising `SIGINT` sidesteps the boundary
/// entirely: the handler is registered with the *kernel* by the binary, so it is
/// process-global, and it runs the binary's `stop_or_exit_server()` with the
/// statics that matter.
///
/// It also restores the escalation the stock console has. Pumpkin's handler task
/// services one signal and returns, so if a graceful shutdown wedges, a second
/// interrupt — from here or from the shell, since the terminal is back to normal
/// by then — hits the default disposition and ends the process.
fn request_stop() {
    // SAFETY: `kill` with our own pid and a standard signal number. Sending
    // SIGINT to ourselves is exactly what the terminal does on Ctrl+C.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGINT);
    }
}

/// Restore the terminal before any other panic handling runs.
fn install_panic_hook(terminal: &std::fs::File) {
    let Ok(terminal) = terminal.try_clone() else {
        return;
    };
    let terminal = Mutex::new(terminal);
    let previous = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        if let Ok(mut terminal) = terminal.lock() {
            pumpkin_tui::restore_on(&mut *terminal);
        }
        // Put stdout and stderr back too, so Pumpkin's own hook — which writes
        // the crash report — is not shouting into a closed pipe.
        capture::restore_descriptors();
        previous(info);
    }));
}

/// A clock that stamps the way the server's console does.
///
/// Commands the operator types are echoed into the log pane, and an echo with no
/// timestamp leaves a hole in a column where every other line has one. The
/// console will not format a date itself, so the format comes from here — parsed
/// from the same `logging.timestamp_format` the `fmt` layer was built with, at
/// the same local offset, so the two are indistinguishable.
///
/// Returns `None` if the format does not parse; the echo then carries no stamp,
/// which is tidier than inventing a format that disagrees with the log.
fn clock_matching(format: &str) -> Option<pumpkin_tui::Clock> {
    let description = time::format_description::parse_owned::<2>(format).ok()?;
    // Resolved once: the offset lookup is not something to repeat per keystroke,
    // and it is what the server itself captured at startup.
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);

    Some(Arc::new(move || {
        time::OffsetDateTime::now_utc()
            .to_offset(offset)
            .format(&description)
            .unwrap_or_default()
    }))
}

/// Spawn a named thread, turning a spawn failure into a plugin load error.
fn spawn_named<F>(name: &str, body: F) -> Result<JoinHandle<()>, String>
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .map_err(|error| format!("pumpkin-tui could not start the {name} thread: {error}"))
}

/// A console that is up: the thread drawing it, and the handle that stops it.
struct Running {
    console: JoinHandle<()>,
    handle: ConsoleHandle,
}

/// The plugin itself.
///
/// `#[plugin_impl]` builds the `Plugin` impl out of every `#[plugin_method]`
/// above it, so this has to stay at the bottom of the file: the methods are
/// collected as the macros expand, in source order.
#[plugin_impl]
pub struct TuiConsolePlugin {
    /// Closed and joined on unload, so the terminal is back to normal before
    /// the server finishes shutting down.
    console: Mutex<Option<Running>>,
}

impl TuiConsolePlugin {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            console: Mutex::new(None),
        }
    }
}

impl Default for TuiConsolePlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::clock_matching;

    #[test]
    fn stamps_in_the_servers_default_format() {
        let clock = clock_matching("[hour]:[minute]:[second]").expect("format parses");
        let stamp = clock();
        assert_eq!(stamp.len(), 8, "expected HH:MM:SS, got {stamp:?}");
        assert_eq!(stamp.as_bytes()[2], b':');
        assert_eq!(stamp.as_bytes()[5], b':');
    }

    #[test]
    fn a_broken_format_leaves_the_echo_unstamped() {
        assert!(clock_matching("[not-a-field]").is_none());
    }
}
