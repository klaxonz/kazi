//! Markdown rendering for the transcript.
//!
//! The model writes markdown; the terminal deserves better than raw `**` and
//! backticks. This module turns assistant text into styled, pre-wrapped
//! [`Line`]s: headings, emphasis, inline code, links, bullet/numbered lists,
//! blockquotes, tables, horizontal rules, and fenced code blocks with syntax
//! highlighting.
//!
//! Everything is wrapped here (not by `Paragraph`) so the transcript's scroll
//! arithmetic can count real screen rows. Unknown constructs degrade to plain
//! text — never dropped.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::highlight::{self, Lang};
use super::theme::Theme;

/// Render assistant markdown into terminal lines for a given body width.
pub fn render(text: &str, theme: Theme, width: usize) -> Vec<Line<'static>> {
    let width = width.max(12);
    // never let a stray control byte (ANSI colour from a pasted log, CRLF...)
    // reach the terminal unescaped
    let clean = super::theme::sanitize_terminal(text);
    let src: Vec<&str> = clean.split('\n').collect();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut i = 0usize;

    while i < src.len() {
        let raw = src[i];
        let trimmed = raw.trim_end();
        let t = trimmed.trim_start();

        // ---- fenced code -------------------------------------------------
        if let Some(fence) = fence_open(t) {
            i += 1;
            let mut code: Vec<&str> = Vec::new();
            while i < src.len() && !is_fence_close(src[i].trim(), fence.char) {
                code.push(src[i].trim_end_matches('\r'));
                i += 1;
            }
            if i < src.len() {
                i += 1; // closing fence
            }
            render_code(&mut out, &code, &fence.lang, theme, width);
            continue;
        }
        if trimmed.is_empty() {
            out.push(Line::default());
            i += 1;
            continue;
        }
        // ---- horizontal rule --------------------------------------------
        if is_rule(t) {
            out.push(Line::styled(
                " ".repeat(2) + &theme.glyphs.hbar.repeat(width.saturating_sub(2).max(1)),
                Style::default().fg(theme.border),
            ));
            i += 1;
            continue;
        }
        // ---- heading -----------------------------------------------------
        if let Some((level, title)) = heading(t) {
            let text = strip_inline_markers(title);
            let style = if level <= 2 {
                theme.heading_style()
            } else {
                Style::default()
                    .fg(theme.heading)
                    .add_modifier(Modifier::BOLD)
            };
            let prefix = if level <= 2 {
                vec![Span::styled(" ", Style::default())]
            } else {
                vec![Span::styled("  ", Style::default())]
            };
            out.extend(wrap_inline(
                &text,
                theme,
                width,
                style,
                prefix.clone(),
                prefix,
            ));
            i += 1;
            continue;
        }
        // ---- table -------------------------------------------------------
        if i + 1 < src.len() && is_table_row(trimmed) && is_table_rule(src[i + 1]) {
            let (rendered, consumed) = render_table(&src[i..], theme, width);
            out.extend(rendered);
            i += consumed;
            continue;
        }
        // ---- blockquote --------------------------------------------------
        if t.starts_with('>') {
            let mut body = String::new();
            while i < src.len() {
                let q = src[i].trim_start();
                let Some(rest) = q.strip_prefix('>') else {
                    break;
                };
                if !body.is_empty() {
                    body.push(' ');
                }
                body.push_str(rest.trim_start());
                i += 1;
            }
            let prefix = vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("{} ", theme.glyphs.vbar),
                    Style::default().fg(theme.quote),
                ),
            ];
            out.extend(wrap_inline(
                &body,
                theme,
                width,
                Style::default().fg(theme.quote),
                prefix.clone(),
                prefix,
            ));
            continue;
        }
        // ---- list item ---------------------------------------------------
        if let Some(item) = list_item(t) {
            let depth = leading_spaces(raw) / 2;
            let mut body = item.text;
            i += 1;
            // indented continuation lines belong to the same item
            while i < src.len()
                && !src[i].trim().is_empty()
                && leading_spaces(src[i]) >= item.indent + 2
                && list_item(src[i].trim_start()).is_none()
                && !src[i].trim_start().starts_with('#')
            {
                body.push(' ');
                body.push_str(src[i].trim());
                i += 1;
            }
            let indent = "  ".repeat(depth + 1);
            let bullet = match item.marker {
                Marker::Bullet => Span::styled(
                    format!("{indent}{} ", theme.glyphs.bullet),
                    theme.key_style(),
                ),
                Marker::Number(n) => {
                    Span::styled(format!("{indent}{n}. "), Style::default().fg(theme.accent))
                }
                Marker::Todo(done) => Span::styled(
                    format!("{indent}[{}] ", if done { 'x' } else { ' ' }),
                    if done {
                        Style::default().fg(theme.dim)
                    } else {
                        theme.key_style()
                    },
                ),
            };
            let pad = UnicodeWidthStr::width(bullet.content.as_ref());
            let cont_prefix = vec![Span::raw(" ".repeat(pad))];
            out.extend(wrap_inline(
                &body,
                theme,
                width,
                theme.body(),
                vec![bullet],
                cont_prefix,
            ));
            continue;
        }

        // ---- paragraph ---------------------------------------------------
        let mut body = String::new();
        while i < src.len() {
            let line = src[i];
            let t = line.trim();
            if t.is_empty()
                || fence_open(t).is_some()
                || is_rule(t)
                || heading(t).is_some()
                || list_item(t).is_some()
                || t.starts_with('>')
                || is_table_row(t)
            {
                break;
            }
            if !body.is_empty() {
                body.push(' ');
            }
            body.push_str(t);
            i += 1;
        }
        if body.is_empty() {
            // nothing consumed: render the raw line so text is never dropped
            body = t.to_string();
            i += 1;
        }
        out.extend(wrap_inline(
            &body,
            theme,
            width,
            theme.body(),
            Vec::new(),
            Vec::new(),
        ));
    }

    // collapse runs of blank lines to at most one
    let mut compact: Vec<Line<'static>> = Vec::with_capacity(out.len());
    for line in out {
        let blank = line.spans.iter().all(|s| s.content.trim().is_empty());
        let prev_blank = compact
            .last()
            .map(|l: &Line| l.spans.iter().all(|s| s.content.trim().is_empty()))
            .unwrap_or(true);
        if blank && prev_blank {
            continue;
        }
        compact.push(line);
    }
    while compact
        .last()
        .map(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
        .unwrap_or(false)
    {
        compact.pop();
    }
    compact
}

struct Fence {
    char: char,
    lang: String,
}

fn fence_open(t: &str) -> Option<Fence> {
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let run = t.chars().take_while(|ch| *ch == c).count();
    if run < 3 {
        return None;
    }
    let lang = t[run..].trim().to_string();
    Some(Fence { char: c, lang })
}

fn is_fence_close(t: &str, fence: char) -> bool {
    let run = t.chars().take_while(|c| *c == fence).count();
    run >= 3 && t.trim().chars().all(|c| c == fence)
}

fn leading_spaces(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ').count()
}

fn is_rule(t: &str) -> bool {
    let compact: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3 && compact.chars().all(|c| c == '-' || c == '*' || c == '_')
}

fn heading(t: &str) -> Option<(usize, &str)> {
    let level = t.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = t[level..].strip_prefix(' ')?;
    Some((level, rest.trim_end().trim_end_matches(['#']).trim_end()))
}

fn strip_inline_markers(s: &str) -> String {
    s.replace("**", "").replace("__", "").replace('`', "")
}

fn render_code(
    out: &mut Vec<Line<'static>>,
    code: &[&str],
    lang: &str,
    theme: Theme,
    width: usize,
) {
    let lang = Lang::parse(lang);
    // label row
    let label = if lang.is_known() {
        format!("  ╭─ {} ", lang_label(lang))
    } else {
        "  ╭─".to_string()
    };
    out.push(Line::styled(label, Style::default().fg(theme.border)));

    let inner = width.saturating_sub(4).max(4);
    if code.is_empty() {
        out.push(Line::styled(
            format!("  {} ", theme.glyphs.vbar),
            Style::default().fg(theme.border),
        ));
        return;
    }
    let mut hl = highlight::Highlighter::new(lang);
    for line in code {
        let spans = hl.line(line, &theme);
        let chunks = split_spans(spans, inner);
        for chunk in chunks {
            let mut row = vec![Span::styled(
                format!("  {} ", theme.glyphs.vbar),
                Style::default().fg(theme.border),
            )];
            row.extend(chunk);
            out.push(Line::from(row));
        }
    }
}

fn lang_label(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "rust",
        Lang::Python => "python",
        Lang::Js => "js",
        Lang::Json => "json",
        Lang::Toml => "toml",
        Lang::Yaml => "yaml",
        Lang::Shell => "sh",
        Lang::CLike => "c",
        Lang::Go => "go",
        Lang::Java => "java",
        Lang::Sql => "sql",
        Lang::Html => "html",
        Lang::Css => "css",
        Lang::Diff => "diff",
        Lang::Plain => "",
    }
}

/// Split styled spans into rows no wider than `width` (code lines wrap rather
/// than being clipped, so nothing silently disappears).
fn split_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut w = 0usize;
    let mut buf = String::new();
    let mut buf_style: Option<Style> = None;

    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                row.push(Span::styled(
                    std::mem::take(&mut buf),
                    buf_style.unwrap_or_default(),
                ));
            }
        };
    }
    macro_rules! newline {
        () => {{
            flush!();
            rows.push(std::mem::take(&mut row));
            w = 0;
        }};
    }

    for span in spans {
        for ch in span.content.chars() {
            let cw = ch.width().unwrap_or(0);
            if w + cw > width && w > 0 {
                newline!();
            }
            if buf_style != Some(span.style) {
                flush!();
                buf_style = Some(span.style);
            }
            buf.push(ch);
            w += cw;
        }
    }
    flush!();
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    Bullet,
    Number(usize),
    Todo(bool),
}

struct Item {
    marker: Marker,
    text: String,
    /// Leading whitespace of the source line.
    indent: usize,
}

fn list_item(t: &str) -> Option<Item> {
    let indent = 0; // caller passes already-trimmed text
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(m) {
            let rest = rest.trim_start();
            if let Some(box_rest) = rest.strip_prefix("[ ] ") {
                return Some(Item {
                    marker: Marker::Todo(false),
                    text: box_rest.to_string(),
                    indent,
                });
            }
            if let Some(box_rest) = rest
                .strip_prefix("[x] ")
                .or_else(|| rest.strip_prefix("[X] "))
            {
                return Some(Item {
                    marker: Marker::Todo(true),
                    text: box_rest.to_string(),
                    indent,
                });
            }
            return Some(Item {
                marker: Marker::Bullet,
                text: rest.to_string(),
                indent,
            });
        }
    }
    // numbered: `1. ` / `2) `
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() && digits.len() <= 3 {
        let rest = &t[digits.len()..];
        if let Some(body) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some(Item {
                marker: Marker::Number(digits.parse().unwrap_or(1)),
                text: body.trim_start().to_string(),
                indent,
            });
        }
    }
    None
}

fn is_table_row(t: &str) -> bool {
    t.starts_with('|') && t.ends_with('|') && t.matches('|').count() >= 2
}

fn is_table_rule(t: &str) -> bool {
    let t = t.trim();
    if !t.starts_with('|') || !t.ends_with('|') {
        return false;
    }
    t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '+')) && t.contains('-')
}

fn table_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

/// Render a markdown table; returns the lines and how many source rows were
/// consumed (header + rule + body rows).
fn render_table(src: &[&str], theme: Theme, width: usize) -> (Vec<Line<'static>>, usize) {
    let header = table_cells(src[0]);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut i = 2;
    while i < src.len() && is_table_row(src[i].trim()) {
        rows.push(table_cells(src[i]));
        i += 1;
    }
    let cols = header
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    let mut widths = vec![3usize; cols];
    for (c, cell) in header.iter().enumerate() {
        widths[c] = widths[c].max(UnicodeWidthStr::width(cell.as_str()));
    }
    for row in &rows {
        for (c, cell) in row.iter().enumerate() {
            widths[c] = widths[c].max(UnicodeWidthStr::width(cell.as_str()));
        }
    }
    // shrink the widest column until the row fits
    while widths.iter().sum::<usize>() + (cols.saturating_sub(1)) * 3 > width.saturating_sub(2) {
        let Some((idx, _)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
            break;
        };
        if widths[idx] <= 3 {
            break;
        }
        widths[idx] -= 1;
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(table_row(&header, &widths, cols, theme, true));
    let rule: String = widths
        .iter()
        .map(|w| theme.glyphs.hbar.repeat(*w))
        .collect::<Vec<_>>()
        .join("─┼─");
    out.push(Line::styled(
        format!("  {rule}"),
        Style::default().fg(theme.border),
    ));
    for row in &rows {
        out.push(table_row(row, &widths, cols, theme, false));
    }
    (out, i)
}

fn table_row(
    cells: &[String],
    widths: &[usize],
    cols: usize,
    theme: Theme,
    header: bool,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
    for (c, col_w) in widths.iter().enumerate().take(cols) {
        if c > 0 {
            spans.push(Span::styled(" │ ", Style::default().fg(theme.border)));
        }
        let cell = cells.get(c).map(String::as_str).unwrap_or("");
        let shown = pad_or_clip(cell, *col_w);
        let style = if header {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            theme.body()
        };
        spans.push(Span::styled(shown, style));
    }
    Line::from(spans)
}

fn pad_or_clip(s: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    if w > width {
        let mut out = String::new();
        let mut used = 0;
        for ch in s.chars() {
            let cw = ch.width().unwrap_or(0);
            if used + cw > width.saturating_sub(1) {
                break;
            }
            out.push(ch);
            used += cw;
        }
        out.push('…');
        out
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

// ---------------------------------------------------------------------------
// inline formatting + wrapping
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Token {
    text: String,
    style: Style,
    /// A space separated this token from the previous one in the source.
    space_before: bool,
}

/// Parse inline markdown into styled tokens (words).
fn inline_tokens(text: &str, base: Style, theme: &Theme) -> Vec<Token> {
    let spans = inline_spans(text, base, theme);
    let mut tokens: Vec<Token> = Vec::new();
    let mut pending_space = false;
    for span in spans {
        let content = span.content.into_owned();
        let mut first = true;
        for part in content.split(' ') {
            if !first {
                pending_space = true;
            }
            first = false;
            if part.is_empty() {
                continue;
            }
            tokens.push(Token {
                text: part.to_string(),
                style: span.style,
                space_before: pending_space,
            });
            pending_space = false;
        }
        // a trailing space in the span means the next token is separated
        if content.ends_with(' ') {
            pending_space = true;
        }
    }
    tokens
}

fn inline_spans(text: &str, base: Style, theme: &Theme) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut buf = String::new();
    let mut i = 0usize;

    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                out.push(Span::styled(std::mem::take(&mut buf), base));
            }
        };
    }

    while i < chars.len() {
        // inline code
        if chars[i] == '`' {
            if let Some(len) = find_closing(&chars, i, '`') {
                flush!();
                let code: String = chars[i + 1..len].iter().collect();
                out.push(Span::styled(code, base.fg(theme.code).bg(theme.code_bg)));
                i = len + 1;
                continue;
            }
        }
        // link [text](url)
        if chars[i] == '[' {
            if let Some((label, url, end)) = parse_link(&chars, i) {
                flush!();
                out.push(Span::styled(
                    label,
                    base.fg(theme.link).add_modifier(Modifier::UNDERLINED),
                ));
                if !url.is_empty() {
                    out.push(Span::styled(
                        format!(" ({url})"),
                        Style::default().fg(theme.faint),
                    ));
                }
                i = end;
                continue;
            }
        }
        // autolink <https://...>
        if chars[i] == '<' {
            if let Some(close) = chars[i + 1..].iter().position(|c| *c == '>') {
                let inside: String = chars[i + 1..i + 1 + close].iter().collect();
                if inside.starts_with("http://") || inside.starts_with("https://") {
                    flush!();
                    out.push(Span::styled(
                        inside,
                        base.fg(theme.link).add_modifier(Modifier::UNDERLINED),
                    ));
                    i += close + 2;
                    continue;
                }
            }
        }
        // bold `**` / `__`
        let mut bold_done = false;
        for marker in ["**", "__"] {
            if starts_at(&chars, i, marker) {
                if let Some(end) = find_marker(&chars, i + marker.len(), marker) {
                    flush!();
                    let inner: String = chars[i + marker.len()..end].iter().collect();
                    let bold = base.add_modifier(Modifier::BOLD);
                    let mut sub = inline_spans(&inner, bold, theme);
                    out.append(&mut sub);
                    i = end + marker.len();
                    bold_done = true;
                    break;
                }
            }
        }
        if bold_done {
            continue;
        }
        // strikethrough
        if starts_at(&chars, i, "~~") {
            if let Some(end) = find_marker(&chars, i + 2, "~~") {
                flush!();
                let inner: String = chars[i + 2..end].iter().collect();
                out.push(Span::styled(
                    inner,
                    base.add_modifier(Modifier::CROSSED_OUT),
                ));
                i = end + 2;
                continue;
            }
        }
        // italic `*` / `_`
        if chars[i] == '*' && !starts_at(&chars, i, "**") {
            if let Some(end) = find_marker(&chars, i + 1, "*") {
                flush!();
                let inner: String = chars[i + 1..end].iter().collect();
                out.push(Span::styled(inner, base.add_modifier(Modifier::ITALIC)));
                i = end + 1;
                continue;
            }
        }
        if chars[i] == '_'
            && !starts_at(&chars, i, "__")
            && (i == 0 || !chars[i - 1].is_alphanumeric())
        {
            if let Some(end) = find_marker(&chars, i + 1, "_") {
                if end + 1 >= chars.len() || !chars[end + 1].is_alphanumeric() {
                    flush!();
                    let inner: String = chars[i + 1..end].iter().collect();
                    out.push(Span::styled(inner, base.add_modifier(Modifier::ITALIC)));
                    i = end + 1;
                    continue;
                }
            }
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush!();
    if out.is_empty() {
        out.push(Span::styled(String::new(), base));
    }
    out
}

fn find_closing(chars: &[char], from: usize, marker: char) -> Option<usize> {
    (from + 1..chars.len()).find(|i| chars[*i] == marker)
}

fn starts_at(chars: &[char], at: usize, pat: &str) -> bool {
    pat.chars()
        .enumerate()
        .all(|(k, c)| chars.get(at + k) == Some(&c))
}

fn find_marker(chars: &[char], from: usize, marker: &str) -> Option<usize> {
    let m: Vec<char> = marker.chars().collect();
    let mut i = from;
    while i + m.len() <= chars.len() {
        if chars[i..i + m.len()] == m[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `[label](url)` at `start`; returns `(label, url, next_index)`.
fn parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let close = (start + 1..chars.len()).find(|i| chars[*i] == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..chars.len()).find(|i| chars[*i] == ')')?;
    let label: String = chars[start + 1..close].iter().collect();
    let url: String = chars[close + 2..end].iter().collect();
    Some((label, url, end + 1))
}

/// Build a wrapped, styled set of lines from inline markdown.
#[allow(unused_assignments)]
fn wrap_inline(
    text: &str,
    theme: Theme,
    width: usize,
    base: Style,
    first_prefix: Vec<Span<'static>>,
    cont_prefix: Vec<Span<'static>>,
) -> Vec<Line<'static>> {
    let tokens = inline_tokens(text, base, &theme);
    let first_w = spans_width(&first_prefix);
    let cont_w = spans_width(&cont_prefix);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut row: Vec<Span<'static>> = first_prefix;
    let mut w = first_w;
    let mut empty = true;

    macro_rules! newline {
        () => {{
            // drop trailing spaces
            if let Some(last) = row.last_mut() {
                let t = last.content.trim_end().to_string();
                last.content = t.into();
            }
            out.push(Line::from(std::mem::take(&mut row)));
            row = cont_prefix.clone();
            w = cont_w;
            empty = true;
        }};
    }

    for tok in tokens {
        let tw = UnicodeWidthStr::width(tok.text.as_str());
        let space_w = if tok.space_before && !empty { 1 } else { 0 };
        if !empty && w + space_w + tw > width {
            newline!();
        }
        if tok.space_before && !empty {
            row.push(Span::raw(" "));
            w += 1;
        }
        if tw > width {
            // hard-wrap an over-long word (URLs, CJK runs)
            for ch in tok.text.chars() {
                let cw = ch.width().unwrap_or(0);
                if w + cw > width && !empty {
                    newline!();
                }
                row.push(Span::styled(ch.to_string(), tok.style));
                w += cw;
                empty = false;
            }
        } else {
            row.push(Span::styled(tok.text, tok.style));
            w += tw;
            empty = false;
        }
    }

    if !empty || out.is_empty() {
        while row
            .last()
            .map(|s| s.content.trim().is_empty())
            .unwrap_or(false)
        {
            row.pop();
        }
        out.push(Line::from(row));
    }
    // remove an empty trailing line produced by a hard wrap landing exactly
    if out.len() > 1 && out.last().map(|l| l.spans.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn all_text(lines: &[Line<'static>]) -> String {
        plain(lines).join("\n")
    }

    #[test]
    fn heading_is_styled_and_marker_dropped() {
        let theme = Theme::default();
        let out = render("## Hello **world**", theme, 40);
        let text = all_text(&out);
        assert_eq!(text.trim(), "Hello world");
        assert!(out
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.fg == Some(theme.heading)));
        assert!(!text.contains("##"));
    }

    #[test]
    fn bold_and_inline_code_keep_text() {
        let theme = Theme::default();
        let out = render("call `foo()` then **stop** now", theme, 60);
        let text = all_text(&out);
        assert_eq!(text.trim(), "call foo() then stop now");
        assert!(out
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content == "foo()" && s.style.fg == Some(theme.code)));
        assert!(out
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content == "stop" && s.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn link_shows_label_and_url() {
        let theme = Theme::default();
        let out = render("see [docs](https://example.com/x) now", theme, 80);
        let text = all_text(&out);
        assert!(text.contains("docs"));
        assert!(text.contains("https://example.com/x"));
    }

    #[test]
    fn paragraph_wraps_to_width() {
        let theme = Theme::default();
        let out = render(&"word ".repeat(40), theme, 30);
        assert!(out.len() > 1);
        for line in &out {
            let w: usize = line
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(w <= 30, "line too wide: {w} {line:?}");
        }
        let text = all_text(&out).split_whitespace().count();
        assert_eq!(text, 40, "no words lost while wrapping");
    }

    #[test]
    fn cjk_wraps_by_display_width() {
        let theme = Theme::default();
        let out = render(&"中文字符".repeat(20), theme, 20);
        assert!(out.len() > 1);
        for line in &out {
            let w: usize = line
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(w <= 20, "wide line {w}");
        }
    }

    #[test]
    fn fenced_code_is_highlighted_and_wrapped() {
        let theme = Theme::default();
        let md = "text\n```rust\nfn main() { println!(\"hi\"); }\nlet x = 1;\n```\nafter";
        let out = render(md, theme, 30);
        let text = all_text(&out);
        assert!(text.contains("fn main()"));
        assert!(text.contains("let x = 1;"));
        assert!(text.contains("after"));
        // long code line wraps instead of being cut off
        assert!(text.contains("println!"));
        assert!(text.contains("\"hi\""));
        assert!(text.contains("; }"));
        assert!(out
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content == "fn" && s.style.fg == Some(theme.keyword)));
    }

    #[test]
    fn unclosed_fence_still_renders() {
        let theme = Theme::default();
        let out = render("```\nlet a = 1;", theme, 40);
        assert!(all_text(&out).contains("let a = 1;"));
    }

    #[test]
    fn bullets_and_numbers_get_prefixes() {
        let theme = Theme::default();
        let out = render("- one\n- two\n\n1. first\n2. second", theme, 40);
        let text = all_text(&out);
        assert!(text.contains("• one"));
        assert!(text.contains("• two"));
        assert!(text.contains("1. first"));
        assert!(text.contains("2. second"));
    }

    #[test]
    fn todo_checkboxes_render() {
        let theme = Theme::default();
        let out = render("- [ ] alpha\n- [x] beta", theme, 40);
        let text = all_text(&out);
        assert!(text.contains("[ ] alpha"));
        assert!(text.contains("[x] beta"));
    }

    #[test]
    fn blockquote_and_rule() {
        let theme = Theme::default();
        let out = render("> quoted line\n> more\n\n---", theme, 30);
        let text = all_text(&out);
        assert!(text.contains("quoted line more"));
        assert!(text.contains("─"));
    }

    #[test]
    fn table_renders_columns() {
        let theme = Theme::default();
        let md = "| name | value |\n|---|---|\n| a | 1 |\n| bb | 22 |";
        let out = render(md, theme, 40);
        let text = all_text(&out);
        assert!(text.contains("name"));
        assert!(text.contains("│"));
        assert!(text.contains("bb"));
        assert!(text.contains("22"));
    }

    #[test]
    fn blank_runs_collapse_and_trailing_blanks_drop() {
        let theme = Theme::default();
        let out = render("a\n\n\n\nb\n\n\n", theme, 20);
        let text = all_text(&out);
        assert_eq!(text, "a\n\nb");
    }

    #[test]
    fn control_bytes_are_dropped() {
        let theme = Theme::default();
        let out = render("a\u{1b}[31mb\u{1b}[0m\tc", theme, 40);
        let text = all_text(&out);
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains("[31m"), "escape body leaked: {text:?}");
        assert!(text.contains("ab"), "{text:?}");
    }

    #[test]
    fn unknown_syntax_is_kept_verbatim() {
        let theme = Theme::default();
        let out = render("a < b && c > d [not a link", theme, 80);
        let text = all_text(&out);
        assert!(text.contains("a < b && c > d [not a link"));
    }

    #[test]
    fn italic_markers_are_consumed() {
        let theme = Theme::default();
        let out = render("this is *very* important", theme, 80);
        assert_eq!(all_text(&out), "this is very important");
    }
}
