# pumpkin-tui — plugin page copy

Draft copy for the plugin listing. Not part of the repository; nothing here is
committed.

---

## Short description (one line)

A full-screen terminal console for your Pumpkin server: live log, status header,
player list, and a command line that completes from the real dispatcher.

## Tagline alternatives

- Your server console, with a UI.
- Stop scrolling a wall of log output.
- A terminal UI for your Pumpkin server.

---

## Long description

Pumpkin's console is a line of stdin and a wall of log output. `pumpkin-tui`
replaces it with a proper terminal UI, without changing anything about how you
run the server.

**A log pane you can actually use.** Scrollback, level filtering, and search.
Scroll away from the tail and it holds position — new lines keep arriving, the
view stays where you left it, and the title says `PAUSED` until you jump back.
Multi-line output such as a `help` page or a stack trace stays as one block
instead of being shredded into separate records.

**A status header.** Version, uptime, players online, TPS, MSPT with a
sparkline, and memory — refreshed every second, read straight from the running
server.

**A player sidebar.** Who is online, which world they are in, and their ping.

**A command line that knows your commands.** Completion comes from the server's
own `CommandDispatcher`, so every command the server actually has is completed —
vanilla and plugin-registered alike, with arguments. Online players are coloured
as players. There is inline history, readline-style editing (`Ctrl+A`, `Ctrl+E`,
`Ctrl+W`, `Ctrl+U`, `Ctrl+K`), and a persistent command history across restarts.

### Keys

| Key | Action |
| --- | --- |
| `Enter` | run the command, or accept the highlighted candidate |
| `Tab` / `Shift+Tab` | complete, then cycle candidates |
| `→` / `End` | accept the dimmed inline suggestion |
| `↑` / `↓` | command history |
| `PgUp` / `PgDn`, `Shift+↑` / `Shift+↓`, wheel | scroll the log |
| `Ctrl+End` | jump back to the newest line |
| `Ctrl+L` | clear the log pane |
| `F1` – `F5` | help, player sidebar, search, level filter, follow tail |
| `Ctrl+C` / `Ctrl+D` | quit — asks first, press again to confirm |

---

## Installation

1. Build the plugin against **the same Pumpkin source your server was built
   from** (see *Requirements* below), then copy the resulting
   `libpumpkin_tui_plugin.so` into `plugins/`.
2. Turn the built-in console off, so two consoles are not fighting over one
   terminal:

   ```toml
   # pumpkin.toml
   [commands]
   use_console = false
   use_tty = false
   ```

3. Start the server as usual.

If `use_console` is left on, the plugin refuses to load and says why — the
server keeps its normal console rather than ending up with a corrupted screen.

---

## Requirements and limits

Read this part before installing. It is short and all of it matters.

**A real terminal.** This is for a console a person is sitting at: SSH, tmux, a
local terminal. The plugin refuses to load unless stdin and stdout are both
terminals with a usable `TERM`, which rules out headless containers, systemd
units and CI.

**Not for control panels.** Pterodactyl, Pelican and the like read the server's
stdout as lines of text and write commands to its stdin. A full-screen UI breaks
both: the panel never sees its startup string, so it leaves the server in
*starting* and eventually kills it, and it loses the `stop` command it uses to
shut the server down cleanly. Do not install this on a panel. Note that
`use_console = false` on its own already costs a panel its stop command, with or
without this plugin.

**Built per server version.** This is a native plugin, not a WASM one. Rust has
no stable ABI, so the `.so` must be built against the same Pumpkin source as the
server, with a compatible compiler. A mismatch is a failed load at best. In
practice that means a rebuild for each Pumpkin release, per platform.

**Unix only.** The log capture uses `dup2`. Windows needs the `SetStdHandle`
equivalent, which is not written yet.

**One behaviour difference.** Console commands do not fire `ServerCommandEvent`,
so a plugin that cancels console commands will not see them. Player commands are
unaffected.

---

## Why native and not WASM

Worth stating plainly, because the documented way to write a Pumpkin plugin is
WebAssembly and this one is not.

A console cannot be a WASM plugin. The sandbox gives a guest no stdin — the host
inherits stdout and stderr only — and WASI has no terminal control at all: no
raw mode, no alternate screen, no resize signal. Even with stdin piped through,
a guest would get cooked, line-buffered input: no arrow keys, no Tab, no
`Ctrl+A`. On top of that, `logging` in the WIT runs plugin to host only and no
log event exists, so the guest cannot see the server's log output — which is the
whole point of a console — and there is no way to enumerate the dispatcher for
completion.

Two of those are missing wiring that upstream could add. Raw mode is not: it
does not exist in the WASI version Pumpkin targets, so no permission grants it.

A native plugin is a dynamic library inside the server process, so all of it is
ordinary function calls. The cost is the ABI coupling described above, and it is
a real cost.

---

## Artwork

Both live in the repository, so the listing and the README stay in step:

| File | Size | Use |
| --- | --- | --- |
| `assets/banner.png` | 2560x1440 | listing header, README, social preview |
| `assets/icon-64.png` | 64x64 | listing icon, favicon |

The banner is 16:9, which is what most listings and GitHub's social preview
expect. If the listing wants a larger icon than 64x64, it needs re-exporting
from the source artwork rather than upscaling.

## Links

- Source: https://github.com/BatuhanAksoyy/pumpkin-tui-plugin
- Licence: MIT
