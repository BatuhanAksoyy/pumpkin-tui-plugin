# pumpkin-tui-plugin

`pumpkin-tui` as a **native Pumpkin plugin** — no fork of the server, no patch
upstream, no second process to launch. Drop one file into `plugins/`, turn the
stock console off, and the server boots into the full-screen UI.

## Why native and not WASM

Pumpkin loads two kinds of plugin (`crates/pumpkin/src/plugin/mod.rs`):
`NativePluginLoader` for `.so`/`.dll`/`.dylib`, and `WasmPluginLoader` for
`.wasm` components. A console cannot be a WASM plugin:

| What a console needs | In the WASM sandbox |
| --- | --- |
| read keystrokes | no stdin — the host inherits stdout and stderr only |
| raw mode, alternate screen | no termios/ioctl in WASI p2 |
| the server's log stream | `logging` is plugin → host only; no log event exists |
| completion from the dispatcher | no command enumeration in the WIT |

A native plugin is a dynamic library inside the server process, so all four are
just ordinary function calls.

## Install

```sh
cargo build --release
cp target/release/libpumpkin_tui_plugin.so /path/to/server/plugins/
```

Then switch the built-in console off, so two consoles aren't fighting over one
terminal:

```toml
# pumpkin.toml
[commands]
use_console = false
use_tty = false
```

Start the server as usual. If `use_console` is left on, the plugin refuses to
load and says so — the server keeps its normal console rather than ending up
with a corrupted screen.

## What it does on load

1. **Takes the rustyline editor** out of the shared logger, so the server's own
   console never claims the terminal and log records stop being routed through
   rustyline's external printer.
2. **Redirects fd 1 and 2 to a pipe** and reads it on a thread. The `tracing`
   subscriber is installed with `.init()` before any plugin loads and has no
   reload layer, so moving the file descriptor is the way in. It catches
   everything the process prints: `tracing`, `println!`, other plugins, panics.
3. **Draws on a dup of the original fd 1**, which is why the UI and the captured
   log output don't collide.
4. **Answers completion from the real `CommandDispatcher`** — every command the
   server knows, vanilla and plugin-registered, with online players coloured as
   players.
5. **Pushes status once a second**: tps, mspt, player list with ping, memory,
   uptime.
6. **Dispatches commands** straight to `handle_command`, synchronously, on the
   thread that read them. Handing a future to `Server::spawn_task` would mean
   this library's tokio interpreting the server's `Handle`, whose layout depends
   on feature resolution that differs between the two builds — the failure mode
   is tasks that silently never run. One thing is given up for that: console
   commands do not fire `ServerCommandEvent`, so a plugin that cancels console
   commands will not see them. Player commands are unaffected.

On unload it restores the descriptors and the terminal, then waits for the
console thread, so shutdown messages land on a normal screen.

Quitting the console (Ctrl+C, then again to confirm) raises `SIGINT` rather than
calling `stop_server()`, which would set the flag in this library's copy of the
crate. The signal handler belongs to the kernel and the binary, so it reaches the
real shutdown — and a second interrupt still force-quits a wedged one.

## Log parsing

Captured output is text, so it has to be parsed back into records. The level
word is the anchor — it is the only field with a fixed vocabulary — and the
timestamp and target flags come from the server's own `LoggingConfig`, so the
parser follows whatever format the operator configured. A line with no level
word (a panic, a bare `println!`, a wrapped continuation) is kept whole as an
`Info` record rather than being dropped or mangled.

## Control panels: don't

Pterodactyl, Pelican and the like read the server's **stdout as lines of text**
and write commands to its **stdin**. A full-screen UI is the one thing that
breaks both. Pumpkin's own egg shows exactly why:

```json
"startup": { "done": "Server is now running." },
"stop": "stop"
```

* **Startup detection.** Wings watches stdout for `Server is now running.`
  That line is printed *after* plugins load, so this plugin has already
  captured it — it ends up drawn inside the log pane instead of appearing on
  stdout. The panel never sees it, leaves the server in *starting* forever, and
  eventually kills it.
* **Graceful stop.** The panel stops the server by writing `stop` to stdin. A
  panel that cannot stop a server cleanly ends up sending `SIGKILL`, which is
  how worlds get corrupted.
* **Console logs.** What the panel stores and replays becomes a stream of
  cursor-positioning escapes.

Note that the second one is not really about this plugin: **`use_console =
false` on its own** already stops Pumpkin reading stdin, so a panel loses its
stop command whether or not the UI is running. Do not set it on a panel.

Two guards make the accident unlikely rather than easy. The plugin refuses to
load unless `use_console = false`, so a default panel install is untouched, and
it refuses unless stdin and stdout are both terminals with a usable `TERM`,
which rules out headless containers, systemd units and CI. Neither is
bulletproof: a panel that allocates a pty and sets `TERM` can still satisfy
them, so the rule is simply not to turn this on under one.

This plugin is for a terminal a person is sitting at — SSH, tmux, a local
console.

## The library boundary

The server binary exports no dynamic symbols, so this `.so` carries its own
compiled copy of `pumpkin`, `tokio` and `std`. Anything that arrives by pointer
— `Arc<Context>`, `Arc<Server>` — is the real thing, and so is anything reached
through it whose vtables the binary built, which is why the command dispatcher
works normally. **Statics and thread-locals are not shared**, and that shapes
three things:

* **No tokio primitives of our own.** A future handed to `spawn_task` runs on
  the binary's runtime, where our copy of tokio has no registered driver, so a
  `tokio::time` interval panics with *there is no reactor running*. Timing uses
  plain threads.
* **`stop_server()` is not callable from here** — it would set the flag in our
  copy of the crate. Stopping runs the dispatcher's own `stop` command, whose
  executor the binary registered.
* **`tracing` needs `Context::init_log()`**, which installs a subscriber into
  our copy. It writes to stderr, which this plugin captures, so plugin log lines
  land in the console with everything else.

One consequence is unfixable from here: `std`'s panic hook is a static too, so
the hook this plugin installs covers panics in its own threads but not panics
elsewhere in the server. Those can leave the terminal in raw mode.

## The catch: ABI

Rust has no stable ABI. This plugin links the `pumpkin` crate itself, so the
`.so` must be built:

* against **the same Pumpkin source** the server binary was built from, and
* with a **compatible rustc**.

`PLUGIN_API_VERSION` (currently `2`) catches gross mismatches at load time, but
not subtle ones. In practice that means a rebuild per Pumpkin release, per
platform. Point the `pumpkin` path dependency in `Cargo.toml` at your server's
source tree before building.

Native plugins also skip signature verification — that check is `.wasm`-only
(`plugin/mod.rs`, the `allow_unsigned` gate) — which is worth knowing if you
plan to distribute through a marketplace.

## Platform

Unix only for now: the capture step uses `dup2`. Windows needs the equivalent
`SetStdHandle` dance, which is not written yet.
