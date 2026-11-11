//! Colours and the logo.
//!
//! The mark is a machine moored in a berth, between two quay walls;
//! `assets/logo.svg` is the same drawing.
//! berth paints its own background, so the colours below are chosen against
//! `BASE` and not against whatever the terminal happens to use.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Padding},
};

/// The screen.
pub const BASE: Color = Color::Rgb(13, 17, 28);
/// Pop-ups and the selected row.
pub const SURFACE: Color = Color::Rgb(24, 31, 48);
pub const RAISED: Color = Color::Rgb(33, 44, 70);
/// Panel borders.
pub const LINE: Color = Color::Rgb(52, 66, 100);
/// Body text.
pub const TEXT: Color = Color::Rgb(205, 214, 244);
/// Secondary text.
pub const MUTED: Color = Color::Rgb(124, 137, 176);
/// Selection, values and keys.
pub const ACCENT: Color = Color::Rgb(116, 178, 255);
/// The quay of the logo.
pub const STEEL: Color = Color::Rgb(92, 118, 170);
/// Text on an accent background.
pub const INK: Color = Color::Rgb(13, 17, 28);
pub const GOOD: Color = Color::Rgb(158, 216, 140);
pub const WARN: Color = Color::Rgb(240, 198, 116);
pub const BAD: Color = Color::Rgb(242, 128, 150);

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
    Line::styled("▌ ", Style::new().fg(ACCENT))
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
    version.push(Span::styled(concat!("v", env!("CARGO_PKG_VERSION")), muted));
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
