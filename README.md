# pumpkin-tui

A terminal console for Minecraft servers, built on [ratatui](https://ratatui.rs).
It replaces the "plain line of stdin plus a wall of log output" console with a
proper UI: a scrollable, filterable log pane, a live status header, a player
sidebar, and a command line with completion, inline hints and history.

The crate is **server-agnostic**. It has no dependency on Pumpkin (or any other
server): a host pushes log records and status through a handle, answers
completion queries through a trait, and reads back what the operator typed.
[`plugin/`](plugin/README.md) is the Pumpkin host, and the only one that ships
here.

## Try it

```sh
cargo run --example demo
```

The example is a fake server — it ticks, logs, gains and loses players, and
answers `help`, `list`, `say`, `gamemode`, `kick`, `teleport`, `weather`,
`time`, `seed` and `stop`.

```
┏ Pumpkin (demo) ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
┃ 1.21.9  │  up 01:02:05  │  players 1/20  │  tps 20.00  │  mspt 17.5  │  m  ▂▂▂▂▃▃▂       ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
┏ log ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ ≥DEBUG ┓┏ players 1 ━━━━━━━━━━━━━━━━━┓
┃12:03:41 INFO  net: Notch joined the game                   ┃┃● Notch overworld 42ms      ┃
┃08:06:36 INFO  net: Listening on 0.0.0.0:25565 (Java)       ┃┃                            ┃
┃08:06:37 INFO  console: > say                               ┃┃                            ┃
┃08:06:37 INFO  Unknown command: say.                        ┃┃                            ┃
┃say<--[HERE]                                                ┃┃                            ┃
┃08:06:39 DEBUG world: Saved 46 chunks in 11ms               ┃┃                            ┃
┃08:06:40 IN┏━━━━━━━━━━┓ > say hello everyone                ┃┃                            ┃
┃08:06:44 WA┃▌survival ┃Can't keep up! Did the system time   ┃┃                            ┃
┃           ┃ creative ┃                                     ┃┃                            ┃
┗━━━━━━━━━━━┗━━━━━━━━━━┛━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
┏ command ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
┃❯ gamemode                                                                                ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
 Tab complete   ↑↓ history   F1 help   F2 players   F3 search   F4 level
```

(Not a sketch — that is the real frame, dumped from the renderer. The completion
popup floats above the prompt, anchored to the word being completed, so it
overlaps the log while it is open.)

Lines a host cannot attribute to a record of their own — the rest of a
multi-line message, the body of a `help` page, a stack trace — are marked as
continuations and drawn flush left across the full width, as a block belonging
to the record above rather than a row of records each with its own stamp.

## Keys

| Key | Action |
| --- | --- |
| `Enter` | run the command, or accept the highlighted candidate |
| `Tab` / `Shift+Tab` | complete, then cycle candidates |
| `→` / `End` | accept the dimmed inline suggestion |
| `↑` / `↓` | command history (popup navigation while it is open) |
| `Ctrl+A` `Ctrl+E` `Ctrl+W` `Ctrl+U` `Ctrl+K` | readline-style line editing |
| `PgUp` / `PgDn`, `Shift+↑` / `Shift+↓`, wheel | scroll the log |
| `Ctrl+End` | jump back to the newest line |
| `Ctrl+L` | clear the log pane |
| `F1` – `F5` | help, player sidebar, search, level filter, follow tail |
| `Ctrl+C` / `Ctrl+D` | quit — asks first, press again to confirm (a non-empty line is cleared instead) |

Scrolling away from the tail shows `PAUSED` in the log title; new lines keep
arriving but the view stays put until `F5` or `Ctrl+End`.

## Running it on Pumpkin

[`plugin/`](plugin/README.md) is a **native Pumpkin plugin**: build the `.so`,
drop it in `plugins/`, turn the stock console off, and the server boots into the
UI. It takes the terminal, captures the log stream off file descriptor 1, and
completes from the real command dispatcher.

Two things to know before you install it. Rust has no stable ABI, so the plugin
has to be built against the same Pumpkin source as the server. And it is for a
terminal a person is sitting at — not a control panel, which reads the server's
stdout as lines of text. The plugin README covers both.

## Using it from a host

```rust
use pumpkin_tui::{Console, ConsoleConfig, ConsoleInput, Level, console_channel};
use pumpkin_tui::completion::{CommandTree, Node};
use std::sync::Arc;

let (handle, backend) = console_channel(4096);

// Anything that logs: a tracing layer, a worker thread, a tick loop.
let server = handle.clone();
std::thread::spawn(move || {
    while let Some(input) = server.next_input() {
        match input {
            ConsoleInput::Command(command) => { server.log_message(Level::Info, command); }
            ConsoleInput::Quit => { server.close(); break; }
        }
    }
});

let commands = CommandTree::new()
    .with(Node::literal("stop").detail("Stop the server"))
    .with(Node::literal("say").then(Node::argument("<message>")));

Console::new(backend)
    .with_completer(Arc::new(commands))
    .with_config(ConsoleConfig { title: "Pumpkin".into(), ..Default::default() })
    .run()?;
```

`Console::run` takes over the terminal and blocks, so give it its own OS thread
(or `main`) and talk to it through the handle.

Commands the operator types are echoed into the log pane. The console never
formats a date itself — the host owns that format, and guessing at it only
produces stamps that disagree with the surrounding log — so pass
`Console::with_clock(...)` a closure and the echo is stamped exactly like the
records around it:

```rust
Console::new(backend).with_clock(Arc::new(|| now_in_the_hosts_format()))
```

Leave it out and the echo carries no timestamp; the log pane pads every record
to a shared column, so the pane still lines up either way.

### Completion

Two ways to supply candidates:

* **`CommandTree`** — declare literals and arguments, with static or dynamic
  suggestion sources:

  ```rust
  Node::literal("kick").then(
      Node::argument("<player>").suggest_with(move || online_player_names()),
  )
  ```

* **`Completer`** — implement the trait over a dispatcher you already have.
  It gets the line and caret offset, and returns candidates plus the byte
  offset they replace from. Pumpkin's own `CommandDispatcher` answers this
  directly; `plugin/src/completer.rs` is that implementation.

Candidates come back tagged (`Command`, `Literal`, `Argument`, `Player`,
`Placeholder`) purely so the popup can colour them. A `Placeholder` such as
`<player>` is shown as guidance and never inserted.

### Theming

`Theme::pumpkin()` (default, truecolor) takes its palette and its frames straight
from [pumpkinmc.org](https://pumpkinmc.org): pumpkin `#ff6b2c` on near-black,
square corners, and the site's own semantic tokens.

| Site token | Value | Used for |
| --- | --- | --- |
| `--color-pumpkin` | `#ff6b2c` | accent, the focused frame, selection |
| `--color-surface` | `#1a1a1a` | popup background |
| `--color-muted` | `#999999` | secondary text, `DEBUG` |
| `--color-warning` | `#ffd93d` | `WARN`, search matches |
| `--color-danger` | `#ff4757` | `ERROR` |
| `--color-success` | `#00c853` | `INFO`, players online, healthy readings |

The site's cards are square with 2–3px edges, so the frames use
`BorderType::Thick` — `theme.border_type`, if you want single-line or double
instead. Borders sit on a faint rule (the site's `#fff3`) and the pane you are
working in gets `theme.border_focus` in pumpkin, mirroring how the site outlines
an active card.

Level colours go on the level *label* — `TRACE` dim, `DEBUG` muted, `INFO`
green, `WARN` yellow, `ERROR` red. Message text stays on `theme.text` so the
pane does not turn into a rainbow; only `WARN` and `ERROR` colour their message
too, because those are the lines worth spotting by eye.

Body text stays on the terminal's own foreground rather than the site's
`#f0f0f0`: the console draws over whatever background you already have, and a
hardcoded near-white is unreadable on a light one.

`Theme::ansi()` (16-colour, single-line borders) is there for terminals without
truecolor or the heavier box-drawing glyphs. The struct is public, so a host can
hand `Console::with_theme` any palette it likes.

## Layout of the crate

| Module | Responsibility |
| --- | --- |
| `backend` | the data crossing the boundary: `LogRecord`, `ServerStatus`, `PlayerInfo`, `ConsoleEvent`, `ConsoleInput`, and the `ConsoleBackend` trait |
| `channel` | the channel-based backend: `ConsoleHandle` for the server, `ChannelBackend` for the console |
| `completion` | `Completer`, `Completions`, and the `CommandTree` builder |
| `app` | console state, key handling, the terminal event loop |
| `ui` | rendering — header, log, sidebar, prompt, popups |
| `input`, `history`, `wrap`, `theme` | line editing, command history, word wrapping, colours |

A host that has redirected its own stdout — as the plugin does, to capture log
output — draws with `Console::run_on(writer)` instead of `Console::run()`, and
hands the terminal back with `restore_on(writer)`.

## Tests

```sh
cargo test        # unit tests plus TestBackend render smoke tests
cargo clippy --all-targets
```

The render tests draw the full layout at sizes from 1×1 up, so a resized
terminal cannot panic the console.

## License

MIT
