//! A streaming Markdown highlighter for the terminal.
//!
//! The subset is line oriented: ATX headings, fenced and inline code, links, bold and italic,
//! list markers, block quotes and horizontal rules. Tables, reflow and nested emphasis are not
//! handled. The highlighter holds the incomplete line, so a delimiter split across two deltas
//! is still parsed when the line completes.

use std::io::Write;

use crate::theme::{Role, Theme};

const BOLD: &str = "\x1b[1m";
const BOLD_OFF: &str = "\x1b[22m";
const ITALIC: &str = "\x1b[3m";
const ITALIC_OFF: &str = "\x1b[23m";

pub struct Highlighter {
    line: String,
    in_fence: bool,
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            line: String::new(),
            in_fence: false,
        }
    }

    /// Feed a text delta. Complete lines are rendered; the trailing partial line is kept.
    pub fn feed<W: Write>(&mut self, text: &str, theme: Theme, out: &mut W) {
        for character in text.chars() {
            if character == '\n' {
                let line = std::mem::take(&mut self.line);
                self.render_line(&line, theme, out);
            } else {
                self.line.push(character);
            }
        }
        let _ = out.flush();
    }

    /// Render the trailing partial line. Called at the turn's terminal event.
    pub fn finish<W: Write>(&mut self, theme: Theme, out: &mut W) {
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.render_line(&line, theme, out);
        }
        let _ = out.flush();
    }

    fn render_line<W: Write>(&mut self, line: &str, theme: Theme, out: &mut W) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if self.in_fence {
            if line.trim_start().starts_with("```") {
                self.in_fence = false;
                return;
            }
            write_role(out, theme, Role::CodeBlock, line);
            let _ = out.write_all(b"\n");
            return;
        }
        if line.trim_start().starts_with("```") {
            self.in_fence = true;
            return;
        }
        if let Some(text) = heading(line) {
            let _ = out.write_all(theme.open(Role::Heading).as_bytes());
            render_inline(text, theme, Some(Role::Heading), out);
            let _ = out.write_all(theme.close(Role::Heading).as_bytes());
            let _ = out.write_all(b"\n");
            return;
        }
        if let Some((marker, rest)) = marker(line) {
            write_role(out, theme, Role::Dim, marker);
            render_inline(rest, theme, None, out);
            let _ = out.write_all(b"\n");
            return;
        }
        render_inline(line, theme, None, out);
        let _ = out.write_all(b"\n");
    }
}

fn heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 || !trimmed[hashes..].starts_with(' ') {
        return None;
    }
    let text = trimmed[hashes..].trim_start();
    Some(text.trim_end_matches('#').trim_end())
}

/// The list or quote marker, including its following space, and the rest of the line.
fn marker(line: &str) -> Option<(&str, &str)> {
    let leading = line.len() - line.trim_start().len();
    let rest = &line[leading..];
    for prefix in ["> ", "- ", "* ", "+ "] {
        if let Some(after) = rest.strip_prefix(prefix) {
            return Some((&line[..leading + prefix.len()], after));
        }
    }
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let after = &rest[digits..];
        if after.starts_with(". ") || after.starts_with(") ") {
            let length = leading + digits + 2;
            return Some((&line[..length], &line[length..]));
        }
    }
    let trimmed = line.trim();
    let rule = trimmed.len() >= 3
        && (trimmed.chars().all(|c| c == '-')
            || trimmed.chars().all(|c| c == '*')
            || trimmed.chars().all(|c| c == '_'));
    if rule {
        return Some((line, ""));
    }
    None
}

fn render_inline<W: Write>(text: &str, theme: Theme, base: Option<Role>, out: &mut W) {
    let mut rest = text;
    while !rest.is_empty() {
        if let Some((inner, after)) = delimited(rest, "`") {
            write_role(out, theme, Role::Code, inner);
            reopen(out, theme, base);
            rest = after;
            continue;
        }
        if let Some((inner, after)) = delimited(rest, "**").or_else(|| delimited(rest, "__")) {
            let _ = out.write_all(BOLD.as_bytes());
            let _ = out.write_all(inner.as_bytes());
            let _ = out.write_all(BOLD_OFF.as_bytes());
            reopen(out, theme, base);
            rest = after;
            continue;
        }
        if let Some((inner, after)) = delimited(rest, "*").or_else(|| delimited(rest, "_")) {
            let _ = out.write_all(ITALIC.as_bytes());
            let _ = out.write_all(inner.as_bytes());
            let _ = out.write_all(ITALIC_OFF.as_bytes());
            reopen(out, theme, base);
            rest = after;
            continue;
        }
        if let Some((label, url, after)) = link(rest) {
            let _ = out.write_all(theme.open(Role::Link).as_bytes());
            let _ = out.write_all(label.as_bytes());
            let _ = out.write_all(theme.close(Role::Link).as_bytes());
            let _ = url;
            reopen(out, theme, base);
            rest = after;
            continue;
        }
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let _ = out.write_all(theme.open(Role::Link).as_bytes());
            let _ = out.write_all(&rest.as_bytes()[..end]);
            let _ = out.write_all(theme.close(Role::Link).as_bytes());
            reopen(out, theme, base);
            rest = &rest[end..];
            continue;
        }
        let next = rest
            .char_indices()
            .skip(1)
            .find(|(_, c)| matches!(c, '`' | '*' | '_' | '[' | 'h'))
            .map(|(index, _)| index)
            .unwrap_or(rest.len());
        let _ = out.write_all(&rest.as_bytes()[..next]);
        rest = &rest[next..];
        if rest.starts_with('[') || rest.starts_with('h') {
            if !rest.starts_with("http://")
                && !rest.starts_with("https://")
                && rest.starts_with('[')
            {
                let _ = out.write_all(b"[");
                rest = &rest[1..];
            } else if !rest.starts_with("http://") && !rest.starts_with("https://") {
                let _ = out.write_all(b"h");
                rest = &rest[1..];
            }
        }
    }
}

fn delimited<'a>(text: &'a str, delimiter: &str) -> Option<(&'a str, &'a str)> {
    let rest = text.strip_prefix(delimiter)?;
    let end = rest.find(delimiter)?;
    Some((&rest[..end], &rest[end + delimiter.len()..]))
}

fn link(text: &str) -> Option<(&str, &str, &str)> {
    let rest = text.strip_prefix('[')?;
    let label_end = rest.find(']')?;
    let after_label = &rest[label_end + 1..];
    let url = after_label.strip_prefix('(')?;
    let url_end = url.find(')')?;
    Some((&rest[..label_end], &url[..url_end], &url[url_end + 1..]))
}

fn reopen<W: Write>(out: &mut W, theme: Theme, base: Option<Role>) {
    if let Some(role) = base {
        let _ = out.write_all(theme.open(role).as_bytes());
    }
}

fn write_role<W: Write>(out: &mut W, theme: Theme, role: Role, text: &str) {
    let _ = out.write_all(theme.open(role).as_bytes());
    let _ = out.write_all(text.as_bytes());
    let _ = out.write_all(theme.close(role).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use smith_core::config::ThemeName;

    fn render(input: &str, theme: Theme) -> String {
        let mut highlighter = Highlighter::new();
        let mut out = Vec::new();
        highlighter.feed(input, theme, &mut out);
        highlighter.finish(theme, &mut out);
        String::from_utf8(out).unwrap()
    }

    fn off() -> Theme {
        Theme::off()
    }

    #[test]
    fn headings_lose_the_marker_and_keep_bold_without_colour() {
        assert_eq!(
            render("# Title\n", off()),
            "\x1b[1mTitle\x1b[22m\n".to_string()
        );
        assert_eq!(
            render("## Sub ##\n", off()),
            "\x1b[1mSub\x1b[22m\n".to_string()
        );
        assert_eq!(render("#not a heading\n", off()), "#not a heading\n");
    }

    #[test]
    fn inline_code_links_and_emphasis_lose_their_markers() {
        assert_eq!(
            render("a **b** c\n", off()),
            "a \x1b[1mb\x1b[22m c\n".to_string()
        );
        assert_eq!(
            render("a *b* c\n", off()),
            "a \x1b[3mb\x1b[23m c\n".to_string()
        );
        // The off theme keeps code fences dim but leaves inline code default, like hax.
        assert_eq!(render("use `code` now\n", off()), "use code now\n");
        assert_eq!(
            render("[docs](https://example.com) here\n", off()),
            "\x1b[4mdocs\x1b[24m here\n".to_string()
        );
        assert_eq!(
            render("see https://example.com now\n", off()),
            "see \x1b[4mhttps://example.com\x1b[24m now\n".to_string()
        );
    }

    #[test]
    fn fenced_code_hides_the_fences_and_styles_the_block() {
        assert_eq!(
            render("```rust\nlet x = 1;\n```\n", off()),
            "\x1b[2mlet x = 1;\x1b[22m\n".to_string()
        );
        assert_eq!(
            render("before\n```\ncode\n", off()),
            "before\n\x1b[2mcode\x1b[22m\n"
        );
    }

    #[test]
    fn list_quote_and_rule_markers_are_dim() {
        assert_eq!(
            render("- item\n", off()),
            "\x1b[2m- \x1b[0mitem\n".to_string()
        );
        assert_eq!(
            render("> quote\n", off()),
            "\x1b[2m> \x1b[0mquote\n".to_string()
        );
        assert_eq!(
            render("1. item\n", off()),
            "\x1b[2m1. \x1b[0mitem\n".to_string()
        );
        assert_eq!(render("---\n", off()), "\x1b[2m---\x1b[0m\n".to_string());
    }

    #[test]
    fn a_delimiter_split_across_deltas_is_still_parsed() {
        let theme = off();
        let mut highlighter = Highlighter::new();
        let mut out = Vec::new();
        highlighter.feed("**bo", theme, &mut out);
        highlighter.feed("ld**\n", theme, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[1mbold\x1b[22m\n".to_string()
        );
    }

    #[test]
    fn the_dark_theme_adds_colour_and_restores_a_heading_after_inline_code() {
        let dark = Theme::from_name(ThemeName::Dark);
        assert_eq!(
            render("# Head `code` tail\n", dark),
            "\x1b[1m\x1b[38;5;141mHead \x1b[38;5;117mcode\x1b[0m\x1b[1m\x1b[38;5;141m tail\x1b[0m\n"
                .to_string()
        );
    }

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(render("plain text\n", off()), "plain text\n");
    }
}
