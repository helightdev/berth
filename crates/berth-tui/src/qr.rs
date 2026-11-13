//! Links as QR codes, for getting them onto a phone: the code in half blocks,
//! two modules to a row of cells, and the overlay that shows one with the
//! link beneath it. A code that does not fit is never cut off; the link is
//! shown as text instead.

use crate::{
    event::{Click, Code, Event, Key},
    screen::{Spots, note_hints},
    theme,
};
use qrcode::{EcLevel, QrCode};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Widget},
};

/// The key that shows a link as a QR code. Alt keeps it from the program in a
/// terminal panel, and a browser leaves it to the page.
pub const KEY: &str = "Alt+R";
/// Its hint in a footer.
pub const HINT: &str = "Alt+R QR code";

/// Modules of light around the code, as the standard asks.
const QUIET: usize = 4;
/// Phones read dark on light, whatever the colours of the screen around it.
const DARK: Color = Color::Rgb(0, 0, 0);
const LIGHT: Color = Color::Rgb(255, 255, 255);
/// The widest the text under a code gets.
const TEXT_WIDTH: u16 = 64;

/// Alt+R, with Shift or Caps Lock too, and with the Ctrl that AltGr adds.
pub fn is_key(key: &Key) -> bool {
    key.alt && matches!(key.code, Code::Char('r' | 'R'))
}

/// A QR code with its quiet zone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Qr {
    /// Modules on a side, quiet zone included.
    side: usize,
    /// Dark modules, row by row.
    modules: Vec<bool>,
}

impl Qr {
    /// The code of `text` at an error correction level, if it can hold it.
    pub fn new(text: &str, level: EcLevel) -> Option<Qr> {
        let code = QrCode::with_error_correction_level(text, level).ok()?;
        let width = code.width();
        let side = width + 2 * QUIET;
        let colours = code.to_colors();
        let mut modules = vec![false; side * side];
        for y in 0..width {
            for x in 0..width {
                modules[(y + QUIET) * side + x + QUIET] =
                    colours[y * width + x] == qrcode::Color::Dark;
            }
        }
        Some(Qr { side, modules })
    }

    /// The code of `text` that fits in `width` × `height` cells: medium error
    /// correction when there is room, low for a long link or a small space.
    pub fn fit(text: &str, width: u16, height: u16) -> Option<Qr> {
        [EcLevel::M, EcLevel::L]
            .into_iter()
            .filter_map(|level| Qr::new(text, level))
            .find(|code| {
                let (columns, rows) = code.size();
                columns <= width && rows <= height
            })
    }

    /// The smallest it can be, for saying how much room it needs.
    pub fn smallest(text: &str) -> Option<Qr> {
        Qr::new(text, EcLevel::L)
    }

    /// Columns and rows of cells it takes.
    pub fn size(&self) -> (u16, u16) {
        (self.side as u16, self.side.div_ceil(2) as u16)
    }

    fn dark(&self, x: usize, y: usize) -> bool {
        y < self.side && self.modules[y * self.side + x]
    }

    /// The rows of cells: an upper half block coloured by the module above
    /// and backed by the one below.
    pub fn lines(&self) -> Vec<Line<'static>> {
        let colour = |dark: bool| if dark { DARK } else { LIGHT };
        (0..self.side)
            .step_by(2)
            .map(|y| {
                let spans: Vec<Span> = (0..self.side)
                    .map(|x| {
                        Span::styled(
                            "▀",
                            Style::new()
                                .fg(colour(self.dark(x, y)))
                                .bg(colour(self.dark(x, y + 1))),
                        )
                    })
                    .collect();
                Line::from(spans)
            })
            .collect()
    }
}

/// Text cut into pieces of at most `width` columns, for a link with no spaces.
fn pieces(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut used = 0;
    for ch in text.chars() {
        let columns = Span::raw(ch.to_string()).width();
        if used + columns > width.max(1) && used > 0 {
            lines.push(String::new());
            used = 0;
        }
        lines.last_mut().expect("one line").push(ch);
        used += columns;
    }
    lines
}

fn too_small(text: &str) -> String {
    match Qr::smallest(text) {
        Some(code) => {
            let (columns, rows) = code.size();
            format!("The window is too small for the QR code, which needs {columns}×{rows}.")
        }
        None => "This is too long for a QR code.".into(),
    }
}

/// A link as a QR code, centred in its area, or as text with a note when the
/// code does not fit.
pub struct Link<'a>(pub &'a str);

impl Widget for Link<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if let Some(code) = Qr::fit(self.0, area.width, area.height) {
            let (columns, rows) = code.size();
            let place = Rect::new(
                area.x + (area.width - columns) / 2,
                area.y + (area.height - rows) / 2,
                columns,
                rows,
            );
            Paragraph::new(code.lines()).render(place, buf);
            return;
        }
        let mut lines = vec![Line::styled(
            too_small(self.0),
            Style::new().fg(theme::WARN),
        )];
        lines.extend(
            pieces(self.0, area.width as usize)
                .into_iter()
                .map(|piece| Line::styled(piece, Style::new().fg(theme::TEXT))),
        );
        Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .render(area, buf);
    }
}

/// A QR code over the screen: one link at a time, with the link beneath it.
#[derive(Clone, Debug, PartialEq)]
pub struct Overlay {
    pub title: String,
    pub links: Vec<String>,
    pub index: usize,
    /// A few words in the bottom border: what the code gives whoever scans it.
    pub note: Option<String>,
}

impl Overlay {
    /// The links of a panel, each once and in the order they appear.
    pub fn of(spots: &Spots) -> Option<Overlay> {
        let mut links: Vec<String> = vec![];
        for (_, link) in spots {
            if !links.contains(link) {
                links.push(link.clone());
            }
        }
        (!links.is_empty()).then(|| Overlay {
            title: "QR code".into(),
            links,
            index: 0,
            note: None,
        })
    }

    /// The link that opens a berth server's page, signed in.
    pub fn connect(link: String) -> Overlay {
        Overlay {
            title: "Open berth on another device".into(),
            links: vec![link],
            index: 0,
            note: Some("holds the server's token".into()),
        }
    }

    pub fn link(&self) -> &str {
        &self.links[self.index.min(self.links.len() - 1)]
    }

    /// Keys and clicks while it is shown. Returns whether it stays: the arrows
    /// and Tab go to the other links, anything else closes it.
    pub fn event(&mut self, event: &Event) -> bool {
        let count = self.links.len();
        match event {
            Event::Key(key) => match key.code {
                Code::Right | Code::Tab | Code::Down if count > 1 => {
                    self.index = (self.index + 1) % count;
                    true
                }
                Code::Left | Code::BackTab | Code::Up if count > 1 => {
                    self.index = (self.index + count - 1) % count;
                    true
                }
                _ => false,
            },
            Event::Mouse(mouse) => !matches!(mouse.click, Click::Left | Click::Right),
            Event::Paste(_) => true,
        }
    }

    pub fn draw(&self, frame: &mut Frame) {
        let screen = frame.area();
        let link = self.link();
        // A border and a column of padding on each side.
        let room = (
            screen.width.saturating_sub(4),
            screen.height.saturating_sub(2),
        );
        let note = |width: u16| -> Vec<Line<'static>> {
            let mut lines = vec![Line::raw("")];
            lines.extend(
                pieces(link, width as usize)
                    .into_iter()
                    .map(|piece| Line::styled(piece, Style::new().fg(theme::ACCENT))),
            );
            lines
        };
        let wanted = (Span::raw(link).width() as u16).clamp(24, TEXT_WIDTH);
        let mut fitted = None;
        for level in [EcLevel::M, EcLevel::L] {
            let Some(code) = Qr::new(link, level) else {
                continue;
            };
            let (columns, rows) = code.size();
            let width = columns.max(wanted).min(room.0);
            let text = note(width);
            if columns <= room.0 && rows + text.len() as u16 <= room.1 {
                fitted = Some((code, width, text));
                break;
            }
        }
        let (code, width, mut text) = match fitted {
            Some((code, width, text)) => (Some(code), width, text),
            None => {
                let width = wanted.min(room.0).max(1);
                let mut text: Vec<Line> = crate::screen::wrap(&too_small(link), width as usize)
                    .into_iter()
                    .map(|line| Line::styled(line, Style::new().fg(theme::MUTED)))
                    .collect();
                text.extend(note(width));
                (None, width, text)
            }
        };
        let rows = code.as_ref().map_or(0, |code| code.size().1);
        text.truncate(room.1.saturating_sub(rows) as usize);
        let height = rows + text.len() as u16 + 2;
        let area = Rect::new(
            screen.x + (screen.width.saturating_sub(width + 4)) / 2,
            screen.y + (screen.height.saturating_sub(height)) / 2,
            (width + 4).min(screen.width),
            height.min(screen.height),
        );
        let title = match self.links.len() {
            1 => self.title.clone(),
            count => format!("{} · {} of {count}", self.title, self.index + 1),
        };
        let keys = match self.links.len() {
            1 => "Esc close",
            _ => "←→ other links · Esc close",
        };
        note_hints(keys, area.x + 1, area.bottom().saturating_sub(1), true);
        let mut block = theme::popup(&title, theme::ACCENT).title_bottom(theme::hints(keys));
        if let Some(note) = &self.note {
            block = block.title_bottom(
                Line::styled(format!(" {note} "), Style::new().fg(theme::WARN)).right_aligned(),
            );
        }
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        if let Some(code) = &code {
            let (columns, rows) = code.size();
            frame.render_widget(
                Paragraph::new(code.lines()),
                Rect::new(
                    inner.x + inner.width.saturating_sub(columns) / 2,
                    inner.y,
                    columns.min(inner.width),
                    rows.min(inner.height),
                ),
            );
        }
        let below = Rect::new(
            inner.x,
            inner.y + rows,
            inner.width,
            inner.height.saturating_sub(rows),
        );
        frame.render_widget(Paragraph::new(text), below);
    }
}

/// What Alt+R and the overlay make of an event, over a panel whose links are
/// `links`. Returns whether the event was theirs, and sets `said` when there
/// is something to say.
pub fn take(
    shown: &mut Option<Overlay>,
    links: &Spots,
    event: &Event,
    said: &mut Option<String>,
) -> bool {
    if let Some(overlay) = shown {
        if !overlay.event(event) {
            *shown = None;
        }
        return true;
    }
    let Event::Key(key) = event else {
        return false;
    };
    if !is_key(key) {
        return false;
    }
    match Overlay::of(links) {
        Some(overlay) => *shown = Some(overlay),
        None => *said = Some("There is no link here to show as a QR code".into()),
    }
    true
}

/// Whether there is a web address in what a panel shows, to offer the key.
pub fn any_link<'a>(lines: impl IntoIterator<Item = &'a str>) -> bool {
    lines.into_iter().any(|line| {
        crate::screen::with_links(line)
            .iter()
            .any(|(part, link)| *link && !part.ends_with('…'))
    })
}

/// A value as it may stand in an address: letters, digits and `-._~` as they
/// are, and with `keep` also those characters; everything else
/// percent-encoded.
fn encoded(value: &str, keep: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        let ch = byte as char;
        if ch.is_ascii_alphanumeric() || "-._~".contains(ch) || keep.contains(ch) {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// An access token of an OAuth issuer is a JWT, of three parts in base64.
/// It runs out, and belongs to whoever it was issued to, so it is never handed on.
fn issued(token: &str) -> bool {
    token.starts_with("eyJ") && token.matches('.').count() == 2
}

/// The link that opens the browser page of a berth server, signed in with
/// `token`: `page` is where the page is. A page that is not the server's own,
/// as on the documentation site, is told the `server` as well. `None` when
/// there is no such link: no token, an access token of an issuer, or no address over the
/// network.
pub fn connect_link(page: &str, server: Option<&str>, token: &str) -> Option<String> {
    let token = token.trim();
    let web = |address: &str| address.starts_with("https://") || address.starts_with("http://");
    if token.is_empty() || issued(token) || !web(page) || server.is_some_and(|server| !web(server))
    {
        return None;
    }
    let query = match server {
        Some(server) => format!("?server={}", encoded(server.trim_end_matches('/'), ":/")),
        None => String::new(),
    };
    Some(format!("{page}{query}#token={}", encoded(token, "")))
}

/// The link to the page that `berth serve --web` gives at `server`.
pub fn served_link(server: &str, token: &str) -> Option<String> {
    connect_link(&format!("{}/", server.trim_end_matches('/')), None, token)
}

/// What the information box says of the row that shows the link to the
/// server's page. `served` when the link is to the page `berth serve --web`
/// gives, which a server without `--web` does not have.
pub fn connect_help(server: &str, served: bool) -> String {
    let mut help = format!(
        "A QR code and a link that open this berth server, {server}, in a browser on a phone or another computer, signed in.\n\n**Both hold the server's token**, which gives full control of its hosts. Show them to nobody who should not have it."
    );
    if served {
        help += &format!(
            "\n\nThe link is to the page of `berth serve --web`. Without `--web`, the page of the documentation site takes `/web/?server={server}#token=TOKEN`."
        );
    }
    if loopback(server) {
        help += "\n\nThat address is this machine's own, which a phone cannot reach. Use one of the server's addresses on your network or tailnet.";
    }
    help
}

/// Whether an address is this machine's own, which a phone cannot reach.
pub fn loopback(address: &str) -> bool {
    let rest = address
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    ["localhost", "127.", "[::1]", "0.0.0.0"]
        .iter()
        .any(|name| rest.starts_with(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Mouse;
    use ratatui::{Terminal, backend::TestBackend};

    fn screen(width: u16, height: u16, draw: impl FnOnce(&mut Frame)) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(draw).unwrap();
        terminal.backend().buffer().clone()
    }

    fn text(buffer: &Buffer) -> String {
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_code_has_its_quiet_zone_and_two_modules_to_a_row() {
        // 19 bytes take version 2 at medium correction: 25 modules, and 4 on each side.
        let code = Qr::new("https://example.com", EcLevel::M).unwrap();
        assert_eq!(code.size(), (33, 17));
        let lines = code.lines();
        assert_eq!(lines.len(), 17);
        assert!(lines.iter().all(|line| line.spans.len() == 33));
        let light = Style::new().fg(LIGHT).bg(LIGHT);
        // The top two rows of cells and the columns at the sides are light.
        for row in &lines[..2] {
            assert!(row.spans.iter().all(|span| span.style == light));
        }
        for row in &lines {
            assert!(row.spans[..4].iter().all(|span| span.style == light));
            assert!(row.spans[29..].iter().all(|span| span.style == light));
        }
        // Rows 4 and 5 of modules are the top of the finder pattern: dark on dark at its corner.
        assert_eq!(lines[2].spans[4].style, Style::new().fg(DARK).bg(DARK));
        // Its second row is light inside the frame.
        assert_eq!(lines[2].spans[5].style, Style::new().fg(DARK).bg(LIGHT));
        // An odd number of modules leaves the bottom half of the last row light.
        assert!(
            lines[16]
                .spans
                .iter()
                .all(|span| span.style.bg == Some(LIGHT))
        );
    }

    #[test]
    fn the_same_link_gives_the_same_code() {
        let link = "https://studio.example.ts.net:8443/pair#token=AbC123";
        let first = Qr::new(link, EcLevel::M).unwrap();
        assert_eq!(first, Qr::new(link, EcLevel::M).unwrap());
        assert_ne!(first, Qr::new("https://example.com", EcLevel::M).unwrap());
        let rows: Vec<String> = first
            .lines()
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| match (span.style.fg, span.style.bg) {
                        (Some(DARK), Some(DARK)) => '█',
                        (Some(DARK), _) => '▀',
                        (_, Some(DARK)) => '▄',
                        _ => ' ',
                    })
                    .collect()
            })
            .collect();
        // The finder pattern of the top left corner, below the quiet zone.
        let start = |row: &String| row.chars().take(12).collect::<String>();
        assert_eq!(start(&rows[2]), "    █▀▀▀▀▀█ ");
        assert_eq!(start(&rows[3]), "    █ ███ █ ");
    }

    #[test]
    fn long_links_get_low_correction_and_still_fit_a_large_window() {
        let token = "x".repeat(200);
        let link = format!("https://berth.example.ts.net/#token={token}");
        assert!(link.len() > 230);
        let medium = Qr::new(&link, EcLevel::M).unwrap();
        let low = Qr::fit(&link, 66, 34).unwrap();
        assert!(low.size().0 < medium.size().0);
        assert!(low.size().0 <= 66 && low.size().1 <= 34);
        // Short links keep medium correction.
        let short = Qr::fit("https://example.com", 80, 40).unwrap();
        assert_eq!(short, Qr::new("https://example.com", EcLevel::M).unwrap());
    }

    #[test]
    fn a_code_that_does_not_fit_is_shown_as_its_link() {
        let link = "https://studio.example.ts.net:8443/pair#token=AbC123";
        assert!(Qr::fit(link, 30, 12).is_none());
        let small = screen(30, 12, |frame| {
            frame.render_widget(Link(link), frame.area())
        });
        let shown = text(&small).replace('\n', "");
        assert!(shown.contains("too small"), "{shown}");
        assert!(!shown.contains('▀'));
        assert!(shown.replace(' ', "").contains(link));
        let large = screen(80, 40, |frame| {
            frame.render_widget(Link(link), frame.area())
        });
        assert!(text(&large).contains('▀') && !text(&large).contains("too small"));
    }

    #[test]
    fn the_overlay_steps_through_the_links_of_a_panel() {
        let at = |x| Rect::new(x, 0, 1, 1);
        let spots = vec![
            (at(0), "https://a.example".to_owned()),
            (at(1), "https://a.example".to_owned()),
            (at(2), "https://b.example".to_owned()),
        ];
        let mut shown = None;
        let mut said = None;
        let key = |code| Event::Key(Key::plain(code));
        let alt_r = Event::Key(Key {
            alt: true,
            ..Key::plain(Code::Char('r'))
        });
        assert!(!take(&mut shown, &spots, &key(Code::Char('r')), &mut said));
        assert!(take(&mut shown, &spots, &alt_r, &mut said));
        let overlay = shown.clone().unwrap();
        assert_eq!(overlay.links, ["https://a.example", "https://b.example"]);
        assert!(take(&mut shown, &spots, &key(Code::Right), &mut said));
        assert_eq!(shown.as_ref().unwrap().link(), "https://b.example");
        let buffer = screen(80, 40, |frame| shown.as_ref().unwrap().draw(frame));
        let drawn = text(&buffer);
        assert!(drawn.contains("QR code · 2 of 2") && drawn.contains("https://b.example"));
        // The wheel leaves it; a click or any other key closes it.
        let wheel = Event::Mouse(Mouse {
            click: Click::ScrollUp,
            column: 0,
            row: 0,
        });
        assert!(take(&mut shown, &spots, &wheel, &mut said) && shown.is_some());
        assert!(take(&mut shown, &spots, &key(Code::Char('x')), &mut said));
        assert!(shown.is_none());
        // Without links there is only something to say.
        assert!(take(&mut shown, &vec![], &alt_r, &mut said));
        assert!(shown.is_none() && said.is_some());
    }

    #[test]
    fn the_overlay_falls_back_to_text_in_a_small_window() {
        let overlay = Overlay::connect("https://berth.example.ts.net/#token=secret".into());
        let drawn = text(&screen(60, 12, |frame| overlay.draw(frame)));
        assert!(
            drawn.contains("too small") && !drawn.contains('▀'),
            "{drawn}"
        );
        assert!(drawn.contains("#token=secret") && drawn.contains("token"));
        let drawn = text(&screen(100, 40, |frame| overlay.draw(frame)));
        assert!(
            drawn.contains('▀') && drawn.contains("holds the server's token"),
            "{drawn}"
        );
    }

    #[test]
    fn connect_links_are_what_the_page_takes() {
        assert_eq!(
            served_link("https://berth.example.ts.net/", "a1b2").as_deref(),
            Some("https://berth.example.ts.net/#token=a1b2")
        );
        assert_eq!(
            connect_link(
                "https://docs.example.org/web/",
                Some("https://berth.example.ts.net"),
                "a+b/c"
            )
            .as_deref(),
            Some(
                "https://docs.example.org/web/?server=https://berth.example.ts.net#token=a%2Bb%2Fc"
            )
        );
        // No token, a login's token, or a socket: nothing to hand on.
        assert_eq!(served_link("https://berth.example.ts.net", ""), None);
        assert_eq!(
            served_link("https://berth.example.ts.net", "eyJhbGciOi.eyJzdWIi.c2ln"),
            None
        );
        assert_eq!(served_link("unix:/run/berth/serve.sock", "a1b2"), None);
        assert!(loopback("http://127.0.0.1:7777") && !loopback("https://berth.example.ts.net"));
    }
}
