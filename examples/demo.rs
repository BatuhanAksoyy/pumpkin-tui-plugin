//! A fake Minecraft server driving the console, so the UI can be tried without
//! building a real one:
//!
//! ```sh
//! cargo run --example demo
//! ```
//!
//! It ticks in the background, produces log traffic, keeps a player list, and
//! answers a handful of commands. Everything it does is what a real host would
//! do through [`pumpkin_tui::ConsoleHandle`].

// A demo host: the command dispatch is deliberately one flat match, and the
// toy RNG casts on purpose.
#![allow(clippy::too_many_lines, clippy::cast_possible_truncation)]

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pumpkin_tui::completion::{CommandTree, Node};
use pumpkin_tui::{
    Console, ConsoleConfig, ConsoleHandle, ConsoleInput, Level, LogRecord, PlayerInfo,
    ServerStatus, console_channel,
};

const WORLDS: [&str; 3] = ["overworld", "the_nether", "the_end"];

fn main() -> std::io::Result<()> {
    let (handle, backend) = console_channel(4096);
    let world = Arc::new(Mutex::new(World::new()));

    let names = Arc::clone(&world);
    let commands =
        CommandTree::new()
            .with(Node::literal("help").detail("List the demo commands"))
            .with(Node::literal("list").detail("Show online players"))
            .with(
                Node::literal("say")
                    .detail("Broadcast a message")
                    .then(Node::argument("<message>")),
            )
            .with(
                Node::literal("gamemode")
                    .detail("Change a player's game mode")
                    .then(
                        Node::argument("<mode>")
                            .suggest(["survival", "creative", "adventure", "spectator"])
                            .then(Node::argument("<player>").suggest_with({
                                let names = Arc::clone(&names);
                                move || names.lock().map(|world| world.names()).unwrap_or_default()
                            })),
                    ),
            )
            .with(
                Node::literal("kick")
                    .detail("Remove a player from the server")
                    .then(Node::argument("<player>").suggest_with({
                        let names = Arc::clone(&names);
                        move || names.lock().map(|world| world.names()).unwrap_or_default()
                    })),
            )
            .with(
                Node::literal("teleport")
                    .detail("Move a player to a world")
                    .then(
                        Node::argument("<player>")
                            .suggest_with({
                                let names = Arc::clone(&names);
                                move || names.lock().map(|world| world.names()).unwrap_or_default()
                            })
                            .then(Node::argument("<world>").suggest(WORLDS)),
                    ),
            )
            .with(
                Node::literal("weather")
                    .detail("Set the weather")
                    .then(Node::argument("<kind>").suggest(["clear", "rain", "thunder"])),
            )
            .with(
                Node::literal("time")
                    .detail("Change the world time")
                    .then(Node::literal("set").then(
                        Node::argument("<value>").suggest(["day", "noon", "night", "midnight"]),
                    ))
                    .then(Node::literal("add").then(Node::argument("<ticks>"))),
            )
            .with(Node::literal("seed").detail("Print the world seed"))
            .with(Node::literal("stop").detail("Shut the server down"));

    spawn_ticker(handle.clone(), Arc::clone(&world));
    spawn_command_loop(handle, world);

    Console::new(backend)
        .with_completer(Arc::new(commands))
        .with_config(ConsoleConfig {
            title: "Pumpkin (demo)".to_owned(),
            history_file: Some(std::env::temp_dir().join("pumpkin-tui-demo.history")),
            min_level: Level::Debug,
            ..ConsoleConfig::default()
        })
        .run()
}

/// The background "server": ticks, logs, and publishes status.
fn spawn_ticker(handle: ConsoleHandle, world: Arc<Mutex<World>>) {
    thread::spawn(move || {
        let started = Instant::now();
        let mut rng = Rng::from_clock();
        let mut tick = 0u64;

        handle.log(record(
            Level::Info,
            "server",
            "Starting Pumpkin 0.1.0 (demo)",
        ));
        handle.log(record(Level::Info, "world", "Preparing spawn area: 0%"));
        handle.log(record(
            Level::Info,
            "net",
            "Listening on 0.0.0.0:25565 (Java) and 0.0.0.0:19132 (Bedrock)",
        ));
        handle.log(record(
            Level::Info,
            "console",
            "Press F1 for keys, Tab to complete, F3 to filter the log",
        ));

        loop {
            thread::sleep(Duration::from_millis(500));
            tick += 1;

            let mspt = 12.0 + f64::from(rng.next_u8() % 25) / 2.0;
            let (online, players) = {
                let mut world = world.lock().expect("world poisoned");
                world.tick(&mut rng, &handle);
                (world.players.len(), world.snapshot())
            };

            let status = ServerStatus {
                brand: "Pumpkin".to_owned(),
                version: "1.21.9".to_owned(),
                tps: (1000.0 / mspt).min(20.0),
                mspt,
                players_online: online as u32,
                players_max: 20,
                memory_used_mb: 420 + u64::from(rng.next_u8() % 40),
                memory_total_mb: 4096,
                chunks_loaded: 1_024 + tick * 3,
                uptime: started.elapsed(),
            };
            if !handle.set_status(status) || !handle.set_players(players) {
                break; // console is gone
            }

            if tick.is_multiple_of(6) {
                handle.log(record(
                    Level::Debug,
                    "world",
                    format!(
                        "Saved {} chunks in {}ms",
                        40 + rng.next_u8() % 30,
                        8 + rng.next_u8() % 12
                    ),
                ));
            }
            if tick.is_multiple_of(37) {
                handle.log(record(
                    Level::Warn,
                    "server",
                    "Can't keep up! Is the server overloaded? Running 2340ms behind",
                ));
            }
            if tick.is_multiple_of(91) {
                handle.log(record(
                    Level::Error,
                    "plugin",
                    "example-plugin: tick handler panicked: index out of bounds: the len is 3 but the index is 7",
                ));
            }
        }
    });
}

/// The other half of a host: read operator input and act on it.
fn spawn_command_loop(handle: ConsoleHandle, world: Arc<Mutex<World>>) {
    thread::spawn(move || {
        while let Some(input) = handle.next_input() {
            let command = match input {
                ConsoleInput::Command(command) => command,
                ConsoleInput::Quit => {
                    handle.log(record(Level::Info, "server", "Stopping server"));
                    handle.close();
                    break;
                }
            };

            let mut parts = command.split_whitespace();
            let verb = parts.next().unwrap_or_default();
            let rest: Vec<&str> = parts.collect();

            match verb {
                "help" => {
                    for line in [
                        "help                      this message",
                        "list                      online players",
                        "say <message>             broadcast to chat",
                        "gamemode <mode> <player>  change a game mode",
                        "kick <player>             disconnect a player",
                        "teleport <player> <world> move a player",
                        "weather <kind>            set the weather",
                        "time set <value>          change the time",
                        "seed                      print the world seed",
                        "stop                      shut down",
                    ] {
                        handle.log(record(Level::Info, "help", line));
                    }
                }
                "list" => {
                    let names = world.lock().expect("world poisoned").names();
                    handle.log(record(
                        Level::Info,
                        "server",
                        format!("{} player(s) online: {}", names.len(), names.join(", ")),
                    ));
                }
                "say" if !rest.is_empty() => {
                    handle.log(record(
                        Level::Info,
                        "chat",
                        format!("[Server] {}", rest.join(" ")),
                    ));
                }
                "gamemode" if rest.len() == 2 => {
                    handle.log(record(
                        Level::Info,
                        "server",
                        format!("Set {}'s game mode to {}", rest[1], rest[0]),
                    ));
                }
                "kick" if rest.len() == 1 => {
                    let mut world = world.lock().expect("world poisoned");
                    if world.remove(rest[0]) {
                        handle.log(record(
                            Level::Info,
                            "server",
                            format!("Kicked {} from the game", rest[0]),
                        ));
                    } else {
                        handle.log(record(
                            Level::Error,
                            "server",
                            format!("No player was found named {}", rest[0]),
                        ));
                    }
                }
                "teleport" if rest.len() == 2 => {
                    let mut world = world.lock().expect("world poisoned");
                    if world.move_to(rest[0], rest[1]) {
                        handle.log(record(
                            Level::Info,
                            "server",
                            format!("Teleported {} to {}", rest[0], rest[1]),
                        ));
                    } else {
                        handle.log(record(
                            Level::Error,
                            "server",
                            format!("No player was found named {}", rest[0]),
                        ));
                    }
                }
                "weather" if rest.len() == 1 => {
                    handle.log(record(
                        Level::Info,
                        "world",
                        format!("Set the weather to {}", rest[0]),
                    ));
                }
                "time" if rest.len() == 2 => {
                    handle.log(record(
                        Level::Info,
                        "world",
                        format!("Set the time to {}", rest[1]),
                    ));
                }
                "seed" => {
                    handle.log(record(Level::Info, "world", "Seed: [-4707922695560669492]"));
                }
                "stop" => {
                    handle.log(record(Level::Info, "server", "Stopping server"));
                    thread::sleep(Duration::from_millis(250));
                    handle.close();
                    break;
                }
                "" => {}
                other => {
                    handle.log(record(
                        Level::Error,
                        "command",
                        format!("Unknown or incomplete command: {other}"),
                    ));
                }
            }
        }
    });
}

fn record(level: Level, target: &str, message: impl Into<String>) -> LogRecord {
    LogRecord::new(level, message)
        .with_time(clock())
        .with_target(target)
}

/// `HH:MM:SS` in UTC — a real host would use its own formatter.
fn clock() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}

struct World {
    players: Vec<PlayerInfo>,
    pool: Vec<&'static str>,
}

impl World {
    fn new() -> Self {
        Self {
            players: Vec::new(),
            pool: vec![
                "Notch",
                "jeb_",
                "Dinnerbone",
                "Grumm",
                "Steve",
                "Alex",
                "Herobrine",
            ],
        }
    }

    fn names(&self) -> Vec<String> {
        self.players
            .iter()
            .map(|player| player.name.clone())
            .collect()
    }

    fn snapshot(&self) -> Vec<PlayerInfo> {
        self.players.clone()
    }

    fn remove(&mut self, name: &str) -> bool {
        let before = self.players.len();
        self.players.retain(|player| player.name != name);
        self.players.len() != before
    }

    fn move_to(&mut self, name: &str, world: &str) -> bool {
        for player in &mut self.players {
            if player.name == name {
                world.clone_into(&mut player.world);
                return true;
            }
        }
        false
    }

    /// Randomly connect, disconnect, and jitter pings.
    fn tick(&mut self, rng: &mut Rng, handle: &ConsoleHandle) {
        for player in &mut self.players {
            let drift = i32::from(rng.next_u8() % 21) - 10;
            player.ping_ms = player.ping_ms.saturating_add_signed(drift).clamp(8, 320);
        }

        let roll = rng.next_u8();
        if roll < 12 && self.players.len() < self.pool.len() {
            let candidates: Vec<&&str> = self
                .pool
                .iter()
                .filter(|name| self.players.iter().all(|player| player.name != ***name))
                .collect();
            if let Some(name) = candidates.get(usize::from(rng.next_u8()) % candidates.len()) {
                let name = (**name).to_owned();
                self.players.push(PlayerInfo {
                    name: name.clone(),
                    world: WORLDS[usize::from(rng.next_u8()) % WORLDS.len()].to_owned(),
                    ping_ms: 20 + u32::from(rng.next_u8() % 60),
                });
                handle.log(record(
                    Level::Info,
                    "net",
                    format!("{name} joined the game"),
                ));
            }
        } else if roll > 246 && !self.players.is_empty() {
            let index = usize::from(rng.next_u8()) % self.players.len();
            let player = self.players.remove(index);
            handle.log(record(
                Level::Info,
                "net",
                format!("{} left the game", player.name),
            ));
        }
    }
}

/// xorshift64* — enough randomness for a demo, no dependency needed.
struct Rng(u64);

impl Rng {
    fn from_clock() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Self(seed | 1)
    }

    const fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    const fn next_u8(&mut self) -> u8 {
        (self.next_u64() >> 33) as u8
    }
}
