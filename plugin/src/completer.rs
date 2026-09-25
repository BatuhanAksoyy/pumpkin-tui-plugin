//! Tab completion answered by the server's real command dispatcher.
//!
//! This is the same logic `PumpkinCommandCompleter` gives rustyline, reshaped
//! into the console's [`Completer`] trait — so the TUI completes every command
//! the server actually knows, vanilla and plugin-registered alike.

use std::collections::HashSet;
use std::sync::Arc;

use pumpkin::command::CommandSender;
use pumpkin::command::string_reader::StringReader;
use pumpkin::server::Server;
use pumpkin_tui::completion::{
    Completer, Completion, CompletionKind, CompletionRequest, Completions,
};

pub struct DispatcherCompleter {
    server: Arc<Server>,
}

impl DispatcherCompleter {
    pub const fn new(server: Arc<Server>) -> Self {
        Self { server }
    }

    /// Names of everyone online, so candidates can be coloured as players.
    fn online_names(&self) -> HashSet<String> {
        self.server
            .get_all_players()
            .iter()
            .map(|player| player.gameprofile.name.clone())
            .collect()
    }
}

impl Completer for DispatcherCompleter {
    fn complete(&self, request: CompletionRequest<'_>) -> Completions {
        let prefix = request.prefix();
        // The console accepts commands with or without a leading slash; the
        // dispatcher wants them without, so the offset has to be added back to
        // every range we hand out.
        let has_slash = usize::from(prefix.starts_with('/'));
        let command = &prefix[has_slash..];

        let dispatcher = self.server.command_dispatcher.load();

        if command.trim().is_empty() {
            let items = dispatcher
                .get_all_commands()
                .into_iter()
                .map(|(name, description)| {
                    Completion::new(name.to_owned(), CompletionKind::Command)
                        .with_detail(description.to_owned())
                })
                .collect();
            return Completions::new(request.word_start(), items);
        }

        let Some(cursor) = request.cursor.checked_sub(has_slash) else {
            return Completions::default();
        };

        let source = CommandSender::Console.into_source(&self.server);
        let mut reader = StringReader::new(command);
        if reader.peek() == Some('/') {
            reader.skip();
        }

        let parsed = dispatcher.parse(&mut reader, &source);
        let suggestions = dispatcher.get_completion_suggestions(parsed, cursor);
        if suggestions.is_empty() {
            return Completions::default();
        }

        let players = self.online_names();
        let items = suggestions
            .suggestions
            .into_iter()
            .map(|suggestion| {
                let text = suggestion.text.cached_text().clone();
                let kind = if players.contains(&text) {
                    CompletionKind::Player
                } else {
                    CompletionKind::Argument
                };
                Completion::new(text, kind)
            })
            .collect();

        Completions::new(suggestions.range.start + has_slash, items)
    }
}
