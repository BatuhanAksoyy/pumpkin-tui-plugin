//! Colours. Everything the UI draws pulls from here, so a host can restyle the
//! console without touching layout code.

use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::BorderType;

use crate::backend::Level;
use crate::completion::CompletionKind;

/// The console's palette.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub accent: Color,
    pub accent_dim: Color,
    pub border: Color,
    /// The frame around whichever pane the operator is working in, and around
    /// popups. Drawn in the accent so attention lands there, the way
    /// pumpkinmc.org outlines its active cards.
    pub border_focus: Color,
    /// Frame style. The site's cards are square with 2–3px edges, so
    /// [`BorderType::Thick`] is the nearest thing a terminal offers.
    pub border_type: BorderType,
    pub text: Color,
    pub muted: Color,
    pub trace: Color,
    pub debug: Color,
    pub info: Color,
    pub warn: Color,
    pub error: Color,
    pub good: Color,
    pub bad: Color,
    pub popup_bg: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    pub match_bg: Color,
    pub match_fg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::pumpkin()
    }
}

impl Theme {
    /// The palette from pumpkinmc.org: pumpkin orange on near-black, square
    /// frames, semantic colours taken from the site's own tokens.
    ///
    /// | Site token | Value | Used for |
    /// | --- | --- | --- |
    /// | `--color-pumpkin` | `#ff6b2c` | accent, focused frames, selection |
    /// | `--color-surface` | `#1a1a1a` | popup background |
    /// | `--color-muted` | `#999999` | secondary text, `DEBUG` |
    /// | `--color-warning` | `#ffd93d` | `WARN`, search matches |
    /// | `--color-danger` | `#ff4757` | `ERROR` |
    /// | `--color-success` | `#00c853` | `INFO`, players online, healthy readings |
    ///
    /// Level colours go on the level *label*; message text stays on
    /// [`Theme::text`] so the pane does not turn into a rainbow. Only `WARN` and
    /// `ERROR` colour their message too, because those are the lines worth
    /// finding by eye.
    ///
    /// Body text stays on [`Color::Reset`] rather than the site's `#f0f0f0`: the
    /// console draws over whatever background the terminal already has, and a
    /// hardcoded near-white is unreadable on a light one.
    #[must_use]
    pub const fn pumpkin() -> Self {
        Self {
            accent: Color::Rgb(255, 107, 44),
            accent_dim: Color::Rgb(153, 64, 26),
            // The site's faint rule, `#fff3` over `--color-ink`.
            border: Color::Rgb(61, 61, 61),
            border_focus: Color::Rgb(255, 107, 44),
            border_type: BorderType::Thick,
            text: Color::Reset,
            muted: Color::Rgb(153, 153, 153),
            trace: Color::Rgb(102, 102, 102),
            debug: Color::Rgb(153, 153, 153),
            info: Color::Rgb(0, 200, 83),
            warn: Color::Rgb(255, 217, 61),
            error: Color::Rgb(255, 71, 87),
            good: Color::Rgb(0, 200, 83),
            bad: Color::Rgb(255, 71, 87),
            popup_bg: Color::Rgb(26, 26, 26),
            selection_bg: Color::Rgb(255, 107, 44),
            selection_fg: Color::Rgb(13, 13, 13),
            match_bg: Color::Rgb(255, 217, 61),
            match_fg: Color::Rgb(13, 13, 13),
        }
    }

    /// A palette that only uses the terminal's own 16 colours, for terminals
    /// (or ssh sessions) where truecolor is not available.
    #[must_use]
    pub const fn ansi() -> Self {
        Self {
            accent: Color::Yellow,
            accent_dim: Color::LightYellow,
            border: Color::DarkGray,
            border_focus: Color::Yellow,
            // Thick borders lean on box-drawing glyphs that a limited terminal
            // may not have; the single-line set is the safe one.
            border_type: BorderType::Plain,
            text: Color::Reset,
            muted: Color::DarkGray,
            trace: Color::DarkGray,
            debug: Color::Cyan,
            info: Color::Green,
            warn: Color::Yellow,
            error: Color::Red,
            good: Color::Green,
            bad: Color::Red,
            popup_bg: Color::Black,
            selection_bg: Color::Yellow,
            selection_fg: Color::Black,
            match_bg: Color::Blue,
            match_fg: Color::White,
        }
    }

    #[must_use]
    pub const fn level_color(&self, level: Level) -> Color {
        match level {
            Level::Trace => self.trace,
            Level::Debug => self.debug,
            Level::Info => self.info,
            Level::Warn => self.warn,
            Level::Error => self.error,
        }
    }

    #[must_use]
    pub fn level_style(&self, level: Level) -> Style {
        let style = Style::default().fg(self.level_color(level));
        if matches!(level, Level::Error | Level::Warn) {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }

    #[must_use]
    pub const fn completion_color(&self, kind: CompletionKind) -> Color {
        match kind {
            CompletionKind::Command => self.accent,
            CompletionKind::Literal => self.debug,
            CompletionKind::Argument => self.info,
            CompletionKind::Player => self.good,
            CompletionKind::Placeholder => self.muted,
        }
    }

    #[must_use]
    pub fn muted_style(&self) -> Style {
        Style::default().fg(self.muted)
    }

    #[must_use]
    pub fn accent_style(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    /// Colour for a "higher is worse" reading such as MSPT.
    #[must_use]
    pub const fn health_color(&self, value: f64, warn_above: f64, bad_above: f64) -> Color {
        if value > bad_above {
            self.bad
        } else if value > warn_above {
            self.warn
        } else {
            self.good
        }
    }
}
