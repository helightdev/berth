//! Markdown for the information box: headings, emphasis, code, lists and links
//! as styled lines. The box wraps them itself.

use crate::theme;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

pub fn render(text: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = vec![];
    let mut spans: Vec<Span<'static>> = vec![];
    // Styles of the elements that are open, innermost last.
    let mut styles = vec![Style::new().fg(theme::TEXT)];
    // The number of the next item of each open list; none for bullets.
    let mut lists: Vec<Option<u64>> = vec![];
    let mut link = String::new();
    let mut code_block = false;

    fn flush(lines: &mut Vec<Line<'static>>, spans: &mut Vec<Span<'static>>) {
        if !spans.is_empty() {
            lines.push(Line::from(std::mem::take(spans)));
        }
    }
    // One empty line between blocks, none at the top.
    fn gap(lines: &mut Vec<Line<'static>>) {
        if lines.last().is_some_and(|line| line.width() > 0) {
            lines.push(Line::default());
        }
    }

    for event in Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH) {
        let style = *styles.last().unwrap();
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if lists.is_empty() {
                        gap(&mut lines);
                    }
                }
                Tag::Heading { level, .. } => {
                    gap(&mut lines);
                    let heading = Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD);
                    styles.push(match level {
                        HeadingLevel::H1 | HeadingLevel::H2 => heading,
                        _ => heading.fg(theme::TEXT),
                    });
                }
                Tag::Strong => styles.push(style.add_modifier(Modifier::BOLD)),
                Tag::Emphasis => styles.push(style.add_modifier(Modifier::ITALIC)),
                Tag::Strikethrough => styles.push(style.add_modifier(Modifier::CROSSED_OUT)),
                Tag::Link { dest_url, .. } => {
                    link = dest_url.to_string();
                    styles.push(style.fg(theme::ACCENT).add_modifier(Modifier::UNDERLINED));
                }
                Tag::List(start) => {
                    flush(&mut lines, &mut spans);
                    if lists.is_empty() {
                        gap(&mut lines);
                    }
                    lists.push(start);
                }
                Tag::Item => {
                    flush(&mut lines, &mut spans);
                    let indent = "  ".repeat(lists.len().saturating_sub(1));
                    let mark = match lists.last_mut() {
                        Some(Some(number)) => {
                            *number += 1;
                            format!("{indent}{}. ", *number - 1)
                        }
                        _ => format!("{indent}• "),
                    };
                    spans.push(Span::styled(mark, Style::new().fg(theme::MUTED)));
                }
                Tag::CodeBlock(_) => {
                    gap(&mut lines);
                    code_block = true;
                }
                Tag::BlockQuote(_) => styles.push(style.fg(theme::MUTED)),
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Paragraph | TagEnd::Item => flush(&mut lines, &mut spans),
                TagEnd::Heading(_) => {
                    flush(&mut lines, &mut spans);
                    styles.pop();
                }
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::BlockQuote(_) => {
                    styles.pop();
                }
                TagEnd::Link => {
                    styles.pop();
                    // An address that is not already what the link says is shown after it.
                    let shown: String = spans.iter().map(|span| span.content.as_ref()).collect();
                    if !link.is_empty() && !shown.ends_with(link.as_str()) {
                        spans.push(Span::styled(format!(" ({link})"), Style::new().fg(theme::MUTED)));
                    }
                    link.clear();
                }
                TagEnd::List(_) => {
                    lists.pop();
                }
                TagEnd::CodeBlock => code_block = false,
                _ => {}
            },
            Event::Text(text) if code_block => {
                for line in text.lines() {
                    lines.push(Line::styled(
                        format!("  {line}"),
                        Style::new().fg(theme::TEXT).bg(theme::RAISED),
                    ));
                }
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                spans.push(Span::styled(text.to_string(), style));
            }
            Event::Code(code) => {
                spans.push(Span::styled(code.to_string(), style.fg(theme::TEXT).bg(theme::RAISED)));
            }
            Event::SoftBreak => spans.push(Span::styled(" ", style)),
            Event::HardBreak => flush(&mut lines, &mut spans),
            Event::Rule => {
                gap(&mut lines);
                lines.push(Line::styled("────────", Style::new().fg(theme::LINE)));
            }
            _ => {}
        }
    }
    flush(&mut lines, &mut spans);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_becomes_styled_lines() {
        let lines = render(
            "## Status\n\nRuns `devbox-smoke` as the **agent**,\nsee [the docs](https://example.com).\n\n- one\n- *two*\n\n1. first\n\n```\nmake install\n```\n\nPlain text with snake_case_names and <angle> brackets.",
        );
        let text: Vec<String> = lines.iter().map(Line::to_string).collect();
        assert_eq!(
            text,
            [
                "Status",
                "",
                "Runs devbox-smoke as the agent, see the docs (https://example.com).",
                "",
                "• one",
                "• two",
                "",
                "1. first",
                "",
                "  make install",
                "",
                "Plain text with snake_case_names and <angle> brackets.",
            ]
        );
        let bold = lines[2].spans.iter().find(|span| span.content == "agent").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = lines[2].spans.iter().find(|span| span.content == "devbox-smoke").unwrap();
        assert_eq!(code.style.bg, Some(theme::RAISED));
        assert_eq!(lines[0].spans[0].style.fg, Some(theme::ACCENT));
    }
}
