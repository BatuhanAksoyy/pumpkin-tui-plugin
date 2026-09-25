//! Taking the server's log output off the terminal and into the console.
//!
//! The `tracing` subscriber is installed in `init_logger` before any plugin is
//! loaded, and Pumpkin builds it with `.init()` rather than a reload layer, so
//! there is no way to swap in a layer of our own after the fact. What we can do
//! is move the file descriptor it writes to: point fd 1 and 2 at a pipe, read
//! the pipe, and keep a dup of the original fd to draw the UI on.
//!
//! This catches everything the process prints, not just `tracing` — `println!`,
//! other plugins, and panic messages all arrive on the same pipe.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

use pumpkin_tui::{ConsoleHandle, Level, LogRecord};

/// The descriptors fd 1 and 2 pointed at before the redirect.
///
/// Kept in statics as well as in [`Capture`] so a panic hook — which cannot
/// borrow the plugin — can still hand the terminal back on the way down.
static SAVED_STDOUT: AtomicI32 = AtomicI32::new(-1);
static SAVED_STDERR: AtomicI32 = AtomicI32::new(-1);

/// Point fd 1 and 2 back at the terminal, if they were ever redirected.
///
/// Idempotent, and safe to call from a panic hook.
pub fn restore_descriptors() {
    let _ = io::stdout().flush();
    let saved_stdout = SAVED_STDOUT.swap(-1, Ordering::SeqCst);
    let saved_stderr = SAVED_STDERR.swap(-1, Ordering::SeqCst);

    // SAFETY: each value is either -1, meaning there is nothing to restore, or
    // a descriptor this module opened with `dup` and has not yet closed.
    unsafe {
        if saved_stdout >= 0 {
            libc::dup2(saved_stdout, libc::STDOUT_FILENO);
            libc::close(saved_stdout);
        }
        if saved_stderr >= 0 {
            libc::dup2(saved_stderr, libc::STDERR_FILENO);
            libc::close(saved_stderr);
        }
    }
}

/// Ownership of the redirected descriptors, so they can be put back.
pub struct Capture {
    terminal: File,
}

impl Capture {
    /// Point fd 1 and 2 at a fresh pipe.
    ///
    /// Returns the capture handle and the read end of that pipe. Everything the
    /// process writes to stdout or stderr from here on shows up there.
    pub fn redirect() -> io::Result<(Self, File)> {
        // Anything already buffered belongs on the real terminal, not in the pipe.
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();

        // SAFETY: `dup` on the two standard descriptors, which are open for the
        // lifetime of the process. Each call either returns a new owned
        // descriptor or -1, checked below.
        let (saved_stdout, saved_stderr, terminal_fd) = unsafe {
            (
                libc::dup(libc::STDOUT_FILENO),
                libc::dup(libc::STDERR_FILENO),
                libc::dup(libc::STDOUT_FILENO),
            )
        };
        if saved_stdout < 0 || saved_stderr < 0 || terminal_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut fds: [RawFd; 2] = [0; 2];
        // SAFETY: `fds` is a two-element array, the size `pipe` requires.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let [read_fd, write_fd] = fds;

        // SAFETY: both arguments are open descriptors we own. `dup2` closes the
        // old fd 1 / fd 2 and makes them aliases of the pipe's write end, which
        // is then no longer needed under its own number.
        let redirected = unsafe {
            let out = libc::dup2(write_fd, libc::STDOUT_FILENO);
            let err = libc::dup2(write_fd, libc::STDERR_FILENO);
            libc::close(write_fd);
            out >= 0 && err >= 0
        };
        if !redirected {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: `terminal_fd` and `read_fd` are owned descriptors that nothing
        // else refers to, so the `File`s become their sole owners.
        let (terminal, pipe) = unsafe {
            (
                File::from_raw_fd(terminal_fd),
                File::from_raw_fd(read_fd),
            )
        };

        SAVED_STDOUT.store(saved_stdout, Ordering::SeqCst);
        SAVED_STDERR.store(saved_stderr, Ordering::SeqCst);

        Ok((Self { terminal }, pipe))
    }

    /// A handle on the real terminal, for drawing or for terminal teardown.
    ///
    /// Every clone refers to the same descriptor, so handing one to the console
    /// and keeping another for a panic hook is fine.
    pub fn terminal(&self) -> io::Result<File> {
        self.terminal.try_clone()
    }

    /// Put fd 1 and 2 back, so log output returns to the terminal.
    pub fn restore(self) {
        // Putting fd 1 and 2 back drops the pipe's last write end, which is
        // what ends the reader thread.
        restore_descriptors();
        drop(self.terminal);
    }
}

/// What the server's console format looks like, so a line can be taken apart.
///
/// Read straight off the server's own `LoggingConfig`, so the two stay in step
/// when an operator changes the format.
#[derive(Clone, Copy)]
pub struct LogFormat {
    pub timestamp: bool,
    pub target: bool,
}

/// Read captured output until the pipe closes, forwarding each line.
///
/// Runs on its own thread: this blocks.
pub fn pump(pipe: File, handle: &ConsoleHandle, format: LogFormat) {
    let mut reader = BufReader::new(pipe);
    let mut line = Vec::new();
    // Continuations inherit the level of the record they belong to, so a level
    // filter in the console keeps a multi-line message whole.
    let mut level = Level::Info;

    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\n', '\r']);
        if text.is_empty() {
            continue;
        }
        let record = parse_line(&strip_ansi(text), format, level);
        if !record.continuation {
            level = record.level;
        }
        if !handle.log(record) {
            // The console is gone; nothing left to forward to.
            break;
        }
    }
}

/// Split one formatted log line back into a record.
///
/// The level word is the anchor: `tracing`'s formatter always writes it between
/// the timestamp and the target, and it is the only field with a fixed
/// vocabulary. Anything without one — a panic, a bare `println!`, a wrapped
/// continuation line — becomes an unlabelled `Info` record rather than being
/// dropped or mangled.
fn parse_line(line: &str, format: LogFormat, previous_level: Level) -> LogRecord {
    let Some((level, start, end)) = find_level(line) else {
        // No level word: a further line of the message above — a stack trace, a
        // wrapped command error, the body of `help` — rather than a record of
        // its own. Giving it a timestamp and an `INFO` label of its own is what
        // makes multi-line output look shuffled.
        return LogRecord::new(previous_level, line.trim_end()).continued();
    };

    let mut record = LogRecord::new(level, "");
    if format.timestamp {
        let time = line[..start].trim();
        if !time.is_empty() {
            record = record.with_time(time);
        }
    }

    let rest = line[end..].trim_start();
    let (target, message) = if format.target {
        split_target(rest)
    } else {
        (None, rest)
    };
    if let Some(target) = target {
        record = record.with_target(target);
    }
    message.trim().clone_into(&mut record.message);
    record
}

/// Find the level word, returning it with the byte range it occupies.
fn find_level(line: &str) -> Option<(Level, usize, usize)> {
    const LEVELS: [(&str, Level); 5] = [
        ("TRACE", Level::Trace),
        ("DEBUG", Level::Debug),
        ("INFO", Level::Info),
        ("WARN", Level::Warn),
        ("ERROR", Level::Error),
    ];

    LEVELS
        .iter()
        .filter_map(|&(word, level)| {
            let at = line.find(word)?;
            let before_is_boundary = line[..at]
                .chars()
                .next_back()
                .is_none_or(|char| !char.is_alphanumeric());
            let end = at + word.len();
            let after_is_boundary = line[end..]
                .chars()
                .next()
                .is_none_or(|char| !char.is_alphanumeric());
            (before_is_boundary && after_is_boundary).then_some((level, at, end))
        })
        .min_by_key(|&(_, at, _)| at)
}

/// Peel `target:` off the front of a message, if that is what it is.
///
/// A target is a module path, so it never contains whitespace. Requiring that
/// keeps a message like `Saved 46 chunks in 11ms: done` intact.
fn split_target(rest: &str) -> (Option<&str>, &str) {
    let Some(colon) = rest.find(':') else {
        return (None, rest);
    };
    let candidate = &rest[..colon];
    if candidate.is_empty() || candidate.chars().any(char::is_whitespace) {
        return (None, rest);
    }
    (Some(candidate), rest[colon + 1..].trim_start())
}

/// Drop ANSI escape sequences.
///
/// The console colours by level itself, and Pumpkin's chat components come
/// through `to_pretty_console()` carrying escapes that ratatui would otherwise
/// draw literally.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();

    while let Some(char) = chars.next() {
        if char != '\x1b' {
            out.push(char);
            continue;
        }
        // CSI sequences run until a byte in @..~; anything else is a short
        // two-character escape whose second character we drop with it.
        if chars.next() == Some('[') {
            for inner in chars.by_ref() {
                if ('@'..='~').contains(&inner) {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: LogFormat = LogFormat {
        timestamp: true,
        target: true,
    };

    #[test]
    fn parses_a_full_line() {
        let record = parse_line("08:06:36  INFO net: Listening on 0.0.0.0:25565", FULL, Level::Info);
        assert_eq!(record.level, Level::Info);
        assert_eq!(record.time, "08:06:36");
        assert_eq!(record.target.as_deref(), Some("net"));
        assert_eq!(record.message, "Listening on 0.0.0.0:25565");
    }

    #[test]
    fn parses_without_timestamp_or_target() {
        let format = LogFormat {
            timestamp: false,
            target: false,
        };
        let record = parse_line("WARN Something looks off", format, Level::Info);
        assert_eq!(record.level, Level::Warn);
        assert!(record.time.is_empty());
        assert_eq!(record.target, None);
        assert_eq!(record.message, "Something looks off");
    }

    #[test]
    fn a_line_without_a_level_continues_the_one_above() {
        let record = parse_line("  say<--[HERE]", FULL, Level::Warn);
        assert!(record.continuation);
        // It belongs to the record above, so it keeps that record's level and
        // grows no timestamp of its own.
        assert_eq!(record.level, Level::Warn);
        assert!(record.time.is_empty());
        assert_eq!(record.message, "  say<--[HERE]");
    }

    #[test]
    fn a_message_with_a_colon_is_not_a_target() {
        let record = parse_line("08:06:39 DEBUG Saved 46 chunks: done", FULL, Level::Info);
        assert_eq!(record.target, None);
        assert_eq!(record.message, "Saved 46 chunks: done");
    }

    #[test]
    fn level_words_inside_other_words_do_not_count() {
        let record = parse_line("12:00:00  INFO chat: TRACEROUTE finished", FULL, Level::Info);
        assert_eq!(record.level, Level::Info);
        assert_eq!(record.message, "TRACEROUTE finished");
    }

    #[test]
    fn strips_escape_sequences() {
        assert_eq!(strip_ansi("\x1b[38;5;208mhello\x1b[0m"), "hello");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
