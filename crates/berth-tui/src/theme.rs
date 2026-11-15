//! Colours and the logo.
//!
//! The mark is a machine moored in a berth, between two quay walls;
//! `docs/public/logo.svg` is the same drawing.
//! berth paints its own background, so the colours below are chosen against
//! `BASE` and not against whatever the terminal happens to use.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Padding},
};

/// What a colour is for. The screens, the command line's prompts, its
/// progress, its tables and its help all take their colours from here, so
/// that the two look like one program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Body text.
    Text,
    /// Secondary text: hints, help, what was already said.
    Muted,
    /// Values, keys, commands and what is selected.
    Accent,
    Good,
    Warn,
    Bad,
    /// The background of the selected row.
    Selection,
}

impl Role {
    /// The colour in a terminal that shows any colour.
    pub const fn rgb(self) -> (u8, u8, u8) {
        match self {
            Role::Text => (205, 214, 244),
            Role::Muted => (124, 137, 176),
            Role::Accent => (116, 178, 255),
            Role::Good => (158, 216, 140),
            Role::Warn => (240, 198, 116),
            Role::Bad => (242, 128, 150),
            Role::Selection => (24, 31, 48),
        }
    }

    const fn colour(self) -> Color {
        let (r, g, b) = self.rgb();
        Color::Rgb(r, g, b)
    }

    /// The nearest of the sixteen colours every terminal has; `None` where
    /// the terminal's own text colour or dim text serves better.
    const fn basic(self) -> Option<Color> {
        match self {
            Role::Text | Role::Muted | Role::Selection => None,
            Role::Accent => Some(Color::LightBlue),
            Role::Good => Some(Color::Green),
            Role::Warn => Some(Color::Yellow),
            Role::Bad => Some(Color::LightRed),
        }
    }
}

/// How many colours the terminal shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Depth {
    /// None: `--color never`, `NO_COLOR` or a dumb terminal.
    None,
    /// The sixteen colours of any terminal.
    #[default]
    Basic,
    /// The 256 colours of xterm.
    Indexed,
    /// Any colour.
    True,
}

impl Depth {
    /// The depth the terminal says it has, from `TERM` and `COLORTERM`
    /// (and Windows Terminal's `WT_SESSION`), given that colours are on.
    pub fn of_terminal(var: impl Fn(&str) -> Option<String>) -> Depth {
        let colorterm = var("COLORTERM").unwrap_or_default().to_lowercase();
        let term = var("TERM").unwrap_or_default().to_lowercase();
        if colorterm == "truecolor" || colorterm == "24bit" || var("WT_SESSION").is_some() {
            Depth::True
        } else if term == "dumb" {
            Depth::None
        } else if term.contains("256color") || term.contains("direct") {
            Depth::Indexed
        } else {
            Depth::Basic
        }
    }
}

/// The colour of `role` at `depth`, if it has one there.
pub fn colour(role: Role, depth: Depth) -> Option<Color> {
    match depth {
        Depth::None => None,
        Depth::Basic => role.basic(),
        Depth::Indexed => {
            let (r, g, b) = role.rgb();
            Some(Color::Indexed(indexed(r, g, b)))
        }
        Depth::True => Some(role.colour()),
    }
}

/// Text in the colour of `role` at `depth`. Secondary text is dim where
/// there are only a few colours, as a grey of sixteen is often unreadable.
pub fn style(role: Role, depth: Depth) -> Style {
    let style = match colour(role, depth) {
        Some(colour) if role == Role::Selection => Style::new().bg(colour),
        Some(colour) => Style::new().fg(colour),
        None => Style::new(),
    };
    match (role, depth) {
        (Role::Muted, Depth::Basic) => style.dim(),
        _ => style,
    }
}

/// The parameters of an SGR escape that writes text in `role`'s colour,
/// such as `38;2;116;178;255`; empty without a colour.
pub fn sgr(role: Role, depth: Depth) -> String {
    let ground = if role == Role::Selection { 48 } else { 38 };
    match (colour(role, depth), role, depth) {
        (_, Role::Muted, Depth::Basic) => "2".into(),
        (Some(Color::Rgb(r, g, b)), ..) => format!("{ground};2;{r};{g};{b}"),
        (Some(Color::Indexed(index)), ..) => format!("{ground};5;{index}"),
        (Some(Color::LightBlue), ..) => format!("{}", ground + 56),
        (Some(Color::Green), ..) => format!("{}", ground - 6),
        (Some(Color::Yellow), ..) => format!("{}", ground - 5),
        (Some(Color::LightRed), ..) => format!("{}", ground + 53),
        _ => String::new(),
    }
}

/// The nearest colour of xterm's 6×6×6 cube or its grey ramp.
fn indexed(r: u8, g: u8, b: u8) -> u8 {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let near = |value: u8| {
        (0..6)
            .min_by_key(|&step| (i32::from(STEPS[step]) - i32::from(value)).abs())
            .unwrap_or(0)
    };
    let (cr, cg, cb) = (near(r), near(g), near(b));
    let cube = 16 + 36 * cr + 6 * cg + cb;
    let distance = |x: (u8, u8, u8)| {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
        d(x.0, r) + d(x.1, g) + d(x.2, b)
    };
    let average = ((u32::from(r) + u32::from(g) + u32::from(b)) / 3) as u8;
    let grey_step = (u32::from(average.saturating_sub(8)) / 10).min(23) as u8;
    let grey = 8 + 10 * grey_step;
    if distance((grey, grey, grey)) < distance((STEPS[cr], STEPS[cg], STEPS[cb])) {
        232 + grey_step
    } else {
        cube as u8
    }
}

/// The screen.
pub const BASE: Color = Color::Rgb(13, 17, 28);
/// Pop-ups and the selected row.
pub const SURFACE: Color = Role::Selection.colour();
pub const RAISED: Color = Color::Rgb(33, 44, 70);
/// Panel borders.
pub const LINE: Color = Color::Rgb(52, 66, 100);
/// Body text.
pub const TEXT: Color = Role::Text.colour();
/// Secondary text.
pub const MUTED: Color = Role::Muted.colour();
/// Selection, values and keys.
pub const ACCENT: Color = Role::Accent.colour();
/// The quay of the logo.
pub const STEEL: Color = Color::Rgb(92, 118, 170);
/// Text on an accent background.
pub const INK: Color = Color::Rgb(13, 17, 28);
pub const GOOD: Color = Role::Good.colour();
pub const WARN: Color = Role::Warn.colour();
pub const BAD: Color = Role::Bad.colour();

/// The mark in front of the selected row, in the screens and in a prompt.
pub const CURSOR: &str = "▌ ";

/// The current tab.
pub fn plate() -> Style {
    Style::new().fg(INK).bg(ACCENT).bold()
}

/// What everything else is drawn on.
pub fn backdrop() -> Block<'static> {
    Block::new().style(Style::new().bg(BASE).fg(TEXT))
}

/// A titled panel; `focused` when it takes the keys.
pub fn panel(title: &str, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { ACCENT } else { LINE }))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(if focused { ACCENT } else { MUTED }).bold(),
        ))
        .padding(Padding::horizontal(1))
}

/// A panel floating over the screen.
pub fn popup(title: &str, colour: Color) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(colour))
        .style(Style::new().bg(SURFACE).fg(TEXT))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(colour).bold(),
        ))
        .padding(Padding::horizontal(1))
}

/// The mark in front of the selected row.
pub fn cursor() -> Line<'static> {
    Line::styled(CURSOR, Style::new().fg(ACCENT))
}

/// Key hints such as `↑↓ select · Enter open`: the key stands out, the rest recedes.
pub fn hints(text: &str) -> Line<'static> {
    let mut spans = vec![];
    for (index, hint) in text.split(" · ").enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", Style::new()));
        }
        let (key, action) = hint.split_once(' ').unwrap_or((hint, ""));
        spans.push(Span::styled(key.to_owned(), Style::new().fg(ACCENT).bold()));
        spans.push(Span::styled(format!(" {action}"), Style::new().fg(MUTED)));
    }
    Line::from(spans)
}

/// A point on the gradient from deep water (0) to surf (1).
fn sea(position: f32) -> Color {
    let mix = |from: u8, to: u8| (from as f32 + (to as f32 - from as f32) * position) as u8;
    Color::Rgb(mix(58, 150), mix(124, 204), mix(232, 255))
}

/// The logo, three rows high, with the name beside it.
pub fn logo() -> Vec<Line<'static>> {
    let quay = Style::new().fg(STEEL);
    let muted = Style::new().fg(MUTED);
    let row = |left: &'static str, middle: &'static str, right: &'static str, depth: f32| {
        vec![
            Span::styled(left, quay),
            Span::styled(middle, Style::new().fg(sea(depth))),
            Span::styled(right, quay),
            Span::raw("   "),
        ]
    };
    let mut name = row("█ ", "▄▄", " █", 1.0);
    name.push(Span::styled("berth", Style::new().fg(ACCENT).bold()));
    let mut about = row("█ ", "██", " █", 0.6);
    about.push(Span::styled("set up and look after agent machines", muted));
    let mut version = row("▀▀", "▀▀", "▀▀", 0.0);
    version.push(Span::styled(format!("v{}", berth_api::VERSION), muted));
    vec![Line::from(name), Line::from(about), Line::from(version)]
}

/// Water two rows deep and `width` cells wide, `seconds` into its motion.
///
/// Three swells of different length travel at different speeds, two one way
/// and one the other, so crests build, overtake each other and flatten again.
/// The colour follows the surface: light on the crests, darker in the troughs.
pub fn swell(width: u16, seconds: f32) -> Vec<Line<'static>> {
    const LEVELS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let (mut crests, mut body) = (vec![], vec![]);
    for column in 0..width {
        let x = f32::from(column);
        let height = 0.5 * (x * 0.17 - seconds * 1.5).sin()
            + 0.3 * (x * 0.065 + seconds * 0.6 + 1.3).sin()
            + 0.2 * (x * 0.43 - seconds * 2.7).sin();
        // Crests are narrower than troughs.
        let height = ((height + 1.0) / 2.0).clamp(0.0, 1.0).powf(1.5);
        let eighths = 2 + (height * 13.0) as usize;
        // Light drifting slowly through the water.
        let shimmer = 0.06 * (x * 0.05 + seconds * 0.4).sin();
        // One tone per column, following the surface, so neighbours blend;
        // the water under a crest is only slightly deeper in colour.
        let tone = 0.25 + 0.6 * height + shimmer;
        let at = |depth: f32| sea((tone - depth).clamp(0.0, 1.0));
        let surface = Style::new().fg(at(0.0));
        crests.push(Span::styled(LEVELS[eighths.saturating_sub(8)], surface));
        body.push(if eighths > 8 {
            Span::styled("▀", Style::new().fg(at(0.05)).bg(at(0.11)))
        } else {
            Span::styled(LEVELS[eighths], surface)
        });
    }
    vec![Line::from(crests), Line::from(body)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_depth_has_a_colour_for_the_accent_and_the_states() {
        for role in [Role::Accent, Role::Good, Role::Warn, Role::Bad] {
            for depth in [Depth::Basic, Depth::Indexed, Depth::True] {
                assert!(colour(role, depth).is_some(), "{role:?} at {depth:?}");
                assert!(!sgr(role, depth).is_empty(), "{role:?} at {depth:?}");
            }
            assert_eq!(colour(role, Depth::None), None);
            assert_eq!(sgr(role, Depth::None), "");
        }
    }

    #[test]
    fn the_screens_and_the_command_line_share_the_accent() {
        assert_eq!(colour(Role::Accent, Depth::True), Some(ACCENT));
        assert_eq!(sgr(Role::Accent, Depth::True), "38;2;116;178;255");
        assert_eq!(sgr(Role::Accent, Depth::Basic), "94");
        assert_eq!(sgr(Role::Bad, Depth::Basic), "91");
        assert_eq!(sgr(Role::Good, Depth::Basic), "32");
        assert_eq!(sgr(Role::Warn, Depth::Basic), "33");
        assert_eq!(sgr(Role::Muted, Depth::Basic), "2");
        assert_eq!(sgr(Role::Selection, Depth::True), "48;2;24;31;48");
    }

    #[test]
    fn colours_come_near_in_256() {
        assert_eq!(indexed(0, 0, 0), 16);
        assert_eq!(indexed(255, 255, 255), 231);
        // The accent is a light blue of the cube.
        assert_eq!(indexed(116, 178, 255), 111);
        assert_eq!(indexed(128, 128, 128), 244);
    }

    #[test]
    fn the_depth_follows_the_terminal() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            }
        };
        assert_eq!(
            Depth::of_terminal(env(&[("COLORTERM", "truecolor")])),
            Depth::True
        );
        assert_eq!(
            Depth::of_terminal(env(&[("TERM", "xterm-256color")])),
            Depth::Indexed
        );
        assert_eq!(Depth::of_terminal(env(&[("TERM", "linux")])), Depth::Basic);
        assert_eq!(Depth::of_terminal(env(&[("TERM", "dumb")])), Depth::None);
    }
}
