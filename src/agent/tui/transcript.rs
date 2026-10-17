//! Structured conversation transcript.
//!
//! The old TUI kept a flat `Vec<(kind, String)>` and printed tool calls as one
//! line each. This module keeps *entries* — a question, an answer, a tool card
//! with its arguments/output/diff, a notice — and renders them into styled,
//! pre-wrapped rows. Tool cards can be expanded, diffs are coloured, assistant
//! markdown is rendered, and every rendered row remembers which entry it came
//! from (mouse hit-testing).
//!
//! Rendering is cached per `(revision, width, theme, details)`, so scrolling or
//! resizing costs one pass, not one per frame. Streamed assistant text stays
//! outside the cache and is rendered by the caller each frame.

use std::cell::{Cell, RefCell};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::markdown;
use super::theme::{Theme, ThemeName};

/// At most this many entries are retained; old conversations fall off the top
/// so a marathon session cannot grow memory or redraw cost without bound.
pub const MAX_ENTRIES: usize = 800;
/// Rows an expanded tool card may print.
const CARD_ROWS_EXPANDED: usize = 240;
/// Rows a collapsed tool card prints.
const CARD_ROWS_COLLAPSED: usize = 4;
/// Rows an *erroring* tool card prints while collapsed.
const CARD_ROWS_ERROR: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Error,
}

#[derive(Debug, Clone)]
pub struct ToolCard {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub state: ToolState,
    pub output: String,
    pub diff: Option<String>,
    pub elapsed_ms: u64,
    /// User toggled this card (click or Ctrl+E).
    pub expanded: bool,
}

#[derive(Debug, Clone)]
pub enum Entry {
    User { text: String },
    Assistant { text: String },
    Tool(ToolCard),
    Notice { level: Level, text: String },
    Rule { label: String },
}

impl Entry {
    /// Plain text used by transcript search.
    fn plain(&self) -> String {
        match self {
            Entry::User { text } | Entry::Assistant { text } => text.clone(),
            Entry::Tool(card) => {
                let mut s = format!("{} {}", card.name, card.summary);
                if !card.output.is_empty() {
                    s.push(' ');
                    s.push_str(&card.output);
                }
                if let Some(d) = &card.diff {
                    s.push(' ');
                    s.push_str(d);
                }
                s
            }
            Entry::Notice { text, .. } => text.clone(),
            Entry::Rule { label } => label.clone(),
        }
    }
}

/// One search hit in the transcript.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub entry: usize,
    pub preview: String,
}

struct CacheKey {
    rev: u64,
    width: u16,
    theme: ThemeName,
    ascii: bool,
    details: bool,
}

impl PartialEq for CacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.rev == other.rev
            && self.width == other.width
            && self.theme == other.theme
            && self.ascii == other.ascii
            && self.details == other.details
    }
}

/// Rendered rows for the frozen part of the transcript.
pub(crate) struct Rendered {
    pub lines: Vec<Line<'static>>,
    /// Entry index for each row (`usize::MAX` = none).
    pub owner: Vec<usize>,
    /// First rendered row of each entry.
    pub starts: Vec<usize>,
}

pub struct Transcript {
    entries: Vec<Entry>,
    /// Assistant text currently being streamed (not yet an entry).
    streaming: String,
    rev: Cell<u64>,
    details: Cell<bool>,
    cache: RefCell<Option<(CacheKey, Rendered)>>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            streaming: String::new(),
            rev: Cell::new(0),
            details: Cell::new(false),
            cache: RefCell::new(None),
        }
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    fn bump(&self) {
        self.rev.set(self.rev.get().wrapping_add(1));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn details(&self) -> bool {
        self.details.get()
    }

    pub fn set_details(&self, on: bool) {
        if self.details.get() != on {
            self.details.set(on);
            self.bump();
        }
    }

    pub fn toggle_details(&self) -> bool {
        let next = !self.details.get();
        self.set_details(next);
        next
    }

    fn push_entry(&mut self, entry: Entry) {
        self.entries.push(entry);
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(0..excess);
        }
        self.bump();
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.push_entry(Entry::User { text: text.into() });
    }

    pub fn push_assistant(&mut self, text: impl Into<String>) {
        self.push_entry(Entry::Assistant { text: text.into() });
    }

    pub fn push_notice(&mut self, level: Level, text: impl Into<String>) {
        let text = text.into();
        // split multi-line notices so wrapping stays predictable
        for (i, line) in text.lines().enumerate() {
            if i > 0 && line.trim().is_empty() {
                continue;
            }
            self.push_entry(Entry::Notice {
                level,
                text: line.to_string(),
            });
        }
        if text.trim().is_empty() {
            self.bump();
        }
    }

    pub fn push_rule(&mut self, label: impl Into<String>) {
        self.push_entry(Entry::Rule {
            label: label.into(),
        });
    }

    /// A tool call started. If an identical pending card exists (same id) it is
    /// updated instead of duplicated.
    pub fn start_tool(
        &mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        summary: impl Into<String>,
    ) {
        let id = id.into();
        if let Some(idx) = self.tool_index(&id) {
            if let Entry::Tool(card) = &mut self.entries[idx] {
                card.state = ToolState::Running;
                card.summary = summary.into();
                self.bump();
                return;
            }
        }
        self.push_entry(Entry::Tool(ToolCard {
            id,
            name: name.into(),
            summary: summary.into(),
            state: ToolState::Running,
            output: String::new(),
            diff: None,
            elapsed_ms: 0,
            expanded: false,
        }));
    }

    /// A tool call finished: fill in its card (match by id, else by the most
    /// recent still-running card of that name).
    #[allow(clippy::too_many_arguments)]
    pub fn finish_tool(
        &mut self,
        id: &str,
        name: &str,
        is_error: bool,
        output: impl Into<String>,
        diff: Option<String>,
        elapsed_ms: u64,
    ) {
        let idx = self.tool_index(id).or_else(|| {
            self.entries.iter().rposition(
                |e| matches!(e, Entry::Tool(c) if c.name == name && c.state == ToolState::Running),
            )
        });
        if let Some(idx) = idx {
            if let Entry::Tool(card) = &mut self.entries[idx] {
                card.state = if is_error {
                    ToolState::Error
                } else {
                    ToolState::Ok
                };
                card.output = output.into();
                card.diff = diff;
                card.elapsed_ms = elapsed_ms;
                self.bump();
                return;
            }
        }
        // no start observed (e.g. replayed events): append a finished card
        self.push_entry(Entry::Tool(ToolCard {
            id: id.to_string(),
            name: name.to_string(),
            summary: String::new(),
            state: if is_error {
                ToolState::Error
            } else {
                ToolState::Ok
            },
            output: output.into(),
            diff,
            elapsed_ms,
            expanded: false,
        }));
    }

    fn tool_index(&self, id: &str) -> Option<usize> {
        self.entries
            .iter()
            .rposition(|e| matches!(e, Entry::Tool(c) if c.id == id))
    }

    /// Flip one entry's expansion. Returns `Some(new_state)` when the entry was
    /// a tool card.
    pub fn toggle_entry(&mut self, idx: usize) -> Option<bool> {
        if let Some(Entry::Tool(card)) = self.entries.get_mut(idx) {
            card.expanded = !card.expanded;
            let state = card.expanded;
            self.bump();
            return Some(state);
        }
        None
    }

    /// A task ended abnormally: nothing will ever finish these cards, so stop
    /// showing them as spinning forever.
    pub fn abandon_running(&mut self, reason: &str) {
        let mut touched = false;
        for entry in &mut self.entries {
            if let Entry::Tool(card) = entry {
                if card.state == ToolState::Running {
                    card.state = ToolState::Error;
                    card.output = reason.to_string();
                    touched = true;
                }
            }
        }
        if touched {
            self.bump();
        }
    }

    pub fn append_delta(&mut self, text: &str) {
        self.streaming.push_str(text);
    }

    pub fn streaming(&self) -> &str {
        &self.streaming
    }

    /// Move streamed text into a permanent Assistant entry.
    pub fn flush_streaming(&mut self) -> bool {
        if self.streaming.trim().is_empty() {
            self.streaming.clear();
            return false;
        }
        let text = std::mem::take(&mut self.streaming);
        self.push_assistant(text);
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.streaming.clear();
        self.bump();
    }

    /// Text of the newest assistant answer (for `/copy`).
    pub fn last_assistant(&self) -> Option<&str> {
        self.entries.iter().rev().find_map(|e| match e {
            Entry::Assistant { text } => Some(text.as_str()),
            _ => None,
        })
    }

    /// Index of the newest tool card (Ctrl+E expands it).
    pub fn last_tool(&self) -> Option<usize> {
        self.entries
            .iter()
            .rposition(|e| matches!(e, Entry::Tool(_)))
    }

    /// Whole transcript as plain text (tests, copy, debugging).
    pub fn plain_text(&self) -> String {
        let mut out = self
            .entries
            .iter()
            .map(Entry::plain)
            .collect::<Vec<_>>()
            .join("\n");
        if !self.streaming.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&self.streaming);
        }
        out
    }

    /// Case-insensitive transcript search; one hit per matching entry.
    pub fn search(&self, needle: &str) -> Vec<SearchHit> {
        let needle = needle.trim();
        if needle.is_empty() {
            return Vec::new();
        }
        let lower = needle.to_lowercase();
        let mut hits = Vec::new();
        for (i, entry) in self.entries.iter().enumerate() {
            let plain = entry.plain();
            if !plain.to_lowercase().contains(&lower) {
                continue;
            }
            let preview = plain
                .lines()
                .find(|l| l.to_lowercase().contains(&lower))
                .unwrap_or("")
                .trim();
            hits.push(SearchHit {
                entry: i,
                preview: clip(preview, 100),
            });
        }
        if self.streaming.to_lowercase().contains(&lower) {
            // the streaming answer is not an entry; point at the last entry
            let entry = self.entries.len().saturating_sub(1);
            hits.push(SearchHit {
                entry,
                preview: clip(self.streaming.trim(), 100),
            });
        }
        hits
    }

    /// Render (with caching) the frozen transcript.
    pub(crate) fn rendered(&self, theme: Theme, width: u16) -> std::cell::Ref<'_, Rendered> {
        let key = CacheKey {
            rev: self.rev.get(),
            width,
            theme: theme.name,
            ascii: theme.glyphs.prompt == ">",
            details: self.details.get(),
        };
        let stale = self
            .cache
            .borrow()
            .as_ref()
            .map(|(k, _)| k != &key)
            .unwrap_or(true);
        if stale {
            let rendered = self.render_all(theme, width as usize);
            *self.cache.borrow_mut() = Some((key, rendered));
        }
        std::cell::Ref::map(self.cache.borrow(), |c| {
            &c.as_ref().expect("cache just filled").1
        })
    }

    fn render_all(&self, theme: Theme, width: usize) -> Rendered {
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut owner: Vec<usize> = Vec::new();
        let mut starts: Vec<usize> = Vec::with_capacity(self.entries.len());
        let details = self.details.get();
        for (idx, entry) in self.entries.iter().enumerate() {
            starts.push(out.len());
            let before = out.len();
            render_entry(entry, theme, width, details, &mut out);
            owner.extend(std::iter::repeat_n(idx, out.len() - before));
        }
        Rendered {
            lines: out,
            owner,
            starts,
        }
    }
}

fn render_entry(
    entry: &Entry,
    theme: Theme,
    width: usize,
    details: bool,
    out: &mut Vec<Line<'static>>,
) {
    match entry {
        Entry::User { text } => {
            let prefix = vec![Span::styled(
                format!("{} ", theme.glyphs.prompt),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )];
            let cont = vec![Span::raw("  ")];
            out.extend(wrap_plain(
                text,
                Style::default().fg(theme.user),
                width,
                prefix,
                cont,
            ));
            out.push(Line::default());
        }
        Entry::Assistant { text } => {
            out.extend(markdown::render(text, theme, width));
            out.push(Line::default());
        }
        Entry::Tool(card) => render_tool(card, theme, width, details, out),
        Entry::Notice { level, text } => {
            let (mark, style) = match level {
                Level::Info => ("·", Style::default().fg(theme.dim)),
                Level::Success => (theme.glyphs.ok, Style::default().fg(theme.ok)),
                Level::Warn => ("!", Style::default().fg(theme.warn)),
                Level::Error => (theme.glyphs.err, Style::default().fg(theme.err)),
            };
            let prefix = vec![
                Span::styled("  ", Style::default()),
                Span::styled(format!("{mark} "), style.add_modifier(Modifier::BOLD)),
            ];
            let cont = vec![Span::raw("    ")];
            out.extend(wrap_plain(text, style, width, prefix, cont));
        }
        Entry::Rule { label } => {
            let label = format!(" {label} ");
            let side = width.saturating_sub(label.len() + 4) / 2;
            let line = format!(
                "  {}{}{}",
                theme.glyphs.hbar.repeat(side),
                label,
                theme.glyphs.hbar.repeat(side)
            );
            out.push(Line::styled(line, Style::default().fg(theme.border)));
            out.push(Line::default());
        }
    }
}

fn render_tool(
    card: &ToolCard,
    theme: Theme,
    width: usize,
    details: bool,
    out: &mut Vec<Line<'static>>,
) {
    let (glyph, gstyle) = match card.state {
        ToolState::Running => ("◦", Style::default().fg(theme.accent)),
        ToolState::Ok => (theme.glyphs.ok, Style::default().fg(theme.ok)),
        ToolState::Error => (theme.glyphs.err, Style::default().fg(theme.err)),
    };
    let mut head: Vec<Span<'static>> = vec![
        Span::raw("  "),
        Span::styled(format!("{glyph} "), gstyle.add_modifier(Modifier::BOLD)),
        Span::styled(
            card.name.clone(),
            Style::default().fg(theme.tool).add_modifier(Modifier::BOLD),
        ),
    ];
    if !card.summary.is_empty() {
        head.push(Span::styled(
            format!(" {}", card.summary),
            Style::default().fg(theme.dim),
        ));
    }
    if let Some(diff) = &card.diff {
        let (add, del) = crate::diff::UnifiedDiff::counts(diff);
        if add > 0 {
            head.push(Span::styled(
                format!(" +{add}"),
                Style::default().fg(theme.add),
            ));
        }
        if del > 0 {
            head.push(Span::styled(
                format!(" −{del}"),
                Style::default().fg(theme.del),
            ));
        }
    }
    if card.state != ToolState::Running {
        head.push(Span::styled(
            format!("  {:.1}s", card.elapsed_ms as f64 / 1000.0),
            Style::default().fg(theme.faint),
        ));
    }
    if !card.expanded
        && card.state != ToolState::Running
        && (card.diff.is_some() || !card.output.is_empty())
    {
        head.push(Span::styled(
            "  (click to expand)",
            Style::default().fg(theme.faint),
        ));
    }
    out.push(Line::from(clip_spans(head, width)));

    let expanded = details || card.expanded;
    let limit = if expanded {
        CARD_ROWS_EXPANDED
    } else if card.state == ToolState::Error {
        CARD_ROWS_ERROR
    } else {
        CARD_ROWS_COLLAPSED
    };

    if let Some(diff) = &card.diff {
        let rows: Vec<&str> = diff.lines().collect();
        for line in rows.iter().take(limit) {
            let style = theme.diff_style(line);
            out.push(diff_row(line, theme, style, width));
        }
        if rows.len() > limit {
            out.push(more_rows(rows.len() - limit, theme, width));
        }
    } else if !card.output.trim().is_empty() {
        let rows: Vec<&str> = card
            .output
            .lines()
            .skip_while(|l| l.trim().is_empty())
            .collect();
        let base = if card.state == ToolState::Error {
            Style::default().fg(theme.err)
        } else {
            Style::default().fg(theme.text)
        };
        for line in rows.iter().take(limit) {
            let prefix = vec![Span::styled(
                format!("  {} ", theme.glyphs.vbar),
                Style::default().fg(theme.border),
            )];
            let cont = vec![Span::styled(
                format!("  {} ", theme.glyphs.vbar),
                Style::default().fg(theme.border),
            )];
            let mut wrapped = wrap_plain(line, base, width, prefix, cont);
            out.append(&mut wrapped);
        }
        if rows.len() > limit {
            out.push(more_rows(rows.len() - limit, theme, width));
        }
    }
    if card.state == ToolState::Running && card.output.is_empty() && card.diff.is_none() {
        out.push(Line::styled(
            format!("  {} …", theme.glyphs.vbar),
            Style::default().fg(theme.border),
        ));
    }
    out.push(Line::default());
}

fn more_rows(rest: usize, theme: Theme, width: usize) -> Line<'static> {
    let text = format!(
        "  {} {} more line{} · ctrl+o for details",
        theme.glyphs.ellipsis,
        rest,
        if rest == 1 { "" } else { "s" }
    );
    Line::from(clip_spans(
        vec![Span::styled(text, Style::default().fg(theme.faint))],
        width,
    ))
}

fn diff_row(line: &str, theme: Theme, style: Style, width: usize) -> Line<'static> {
    let line = super::theme::sanitize_terminal(line);
    let prefix = Span::styled(
        format!("  {} ", theme.glyphs.vbar),
        Style::default().fg(theme.border),
    );
    let inner = width.saturating_sub(4);
    let shown = if line.chars().count() > inner {
        let mut s: String = line.chars().take(inner.saturating_sub(1)).collect();
        s.push('…');
        s
    } else {
        line
    };
    Line::from(vec![prefix, Span::styled(shown, style)])
}

/// Wrap plain (non-markdown) text preserving line breaks and indentation.
/// Terminal control characters from command output are stripped so an ANSI
/// escape sequence can never corrupt the screen.
pub fn wrap_plain(
    text: &str,
    style: Style,
    width: usize,
    first_prefix: Vec<Span<'static>>,
    cont_prefix: Vec<Span<'static>>,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut first = true;
    for source in text.split('\n') {
        let prefix = if first {
            first_prefix.clone()
        } else {
            cont_prefix.clone()
        };
        first = false;
        let prefix_w = spans_width(&prefix);
        let avail = width.saturating_sub(prefix_w).max(4);
        let source = super::theme::sanitize_terminal(source);
        for (k, row) in wrap_chars(&source, avail).into_iter().enumerate() {
            let mut spans = if k == 0 {
                prefix.clone()
            } else {
                cont_prefix.clone()
            };
            spans.push(Span::styled(row, style));
            out.push(Line::from(spans));
        }
    }
    if out.is_empty() {
        out.push(Line::from(first_prefix));
    }
    out
}

/// Greedy char wrap that prefers breaking at a space.
fn wrap_chars(line: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let remaining = chars.len() - start;
        if remaining <= width {
            rows.push(chars[start..].iter().collect());
            break;
        }
        let hard = start + width;
        let end = (start..hard)
            .rev()
            .find(|&i| chars[i] == ' ')
            .unwrap_or(hard);
        let end = if end == start { hard } else { end };
        rows.push(chars[start..end].iter().collect());
        start = end;
        while start < chars.len() && chars[start] == ' ' {
            start += 1;
        }
        if start >= chars.len() {
            break;
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

/// Trim a span list to `width` display columns, adding `…` when clipped.
fn clip_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    for span in spans {
        if used >= width {
            break;
        }
        let mut text = String::new();
        let mut clipped = false;
        for ch in span.content.chars() {
            let cw = ch.width().unwrap_or(0);
            if used + cw > width {
                clipped = true;
                break;
            }
            text.push(ch);
            used += cw;
        }
        if clipped {
            if !text.is_empty() {
                if used < width {
                    text.push('…');
                }
                out.push(Span::styled(text, span.style));
            }
            break;
        }
        out.push(Span::styled(text, span.style));
    }
    out
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn user_and_assistant_render_markdown() {
        let theme = Theme::default();
        let mut t = Transcript::new();
        t.push_user("hello **there**");
        t.push_assistant("## Answer\n\n- one\n- two");
        let r = t.rendered(theme, 60);
        let text = text_of(&r.lines);
        assert!(text.contains("hello **there**"), "got: {text}");
        assert!(text.contains("Answer"));
        assert!(text.contains("• one"));
    }

    #[test]
    fn tool_card_matches_start_and_end_by_id() {
        let theme = Theme::default();
        let mut t = Transcript::new();
        t.start_tool("a", "read_file", "src/main.rs");
        t.start_tool("b", "search_files", "needle");
        t.finish_tool("a", "read_file", false, "1: fn main() {}", None, 12);
        t.finish_tool("b", "search_files", true, "no matches", None, 3);
        let r = t.rendered(theme, 80);
        let text = text_of(&r.lines);
        assert!(text.contains("read_file src/main.rs"), "got: {text}");
        assert!(text.contains("search_files needle"));
        assert_eq!(text.matches("read_file").count(), 1);
    }

    #[test]
    fn tool_diffs_are_rendered_with_counts() {
        let theme = Theme::default();
        let mut t = Transcript::new();
        t.start_tool("a", "write_file", "src/x.rs (10B)");
        t.finish_tool(
            "a",
            "write_file",
            false,
            "wrote 10 bytes",
            Some("@@ -1,2 +1,2 @@\n old\n-new\n+new2".to_string()),
            5,
        );
        let r = t.rendered(theme, 80);
        let text = text_of(&r.lines);
        assert!(text.contains("+1"), "added count: {text}");
        assert!(text.contains("−1"), "removed count: {text}");
        assert!(text.contains("-new"));
        assert!(text.contains("+new2"));
        assert!(r
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.fg == Some(theme.add)));
    }

    #[test]
    fn cards_expand_and_collapse() {
        let theme = Theme::default();
        let mut t = Transcript::new();
        t.start_tool("a", "run_command", "cargo test");
        let output: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        t.finish_tool("a", "run_command", false, output, None, 1200);
        let collapsed = {
            let r = t.rendered(theme, 80);
            text_of(&r.lines).clone()
        };
        assert!(collapsed.contains("more lines · ctrl+o"));
        assert!(!collapsed.contains("line 30"));
        let idx = t.entries.len() - 1;
        assert_eq!(t.toggle_entry(idx), Some(true));
        let expanded = {
            let r = t.rendered(theme, 80);
            text_of(&r.lines).clone()
        };
        assert!(expanded.contains("line 30"));
    }

    #[test]
    fn details_toggle_expands_everything() {
        let theme = Theme::default();
        let mut t = Transcript::new();
        t.start_tool("a", "run_command", "x");
        let output: String = (1..=20).map(|i| format!("row {i}\n")).collect();
        t.finish_tool("a", "run_command", false, output, None, 10);
        t.set_details(true);
        let r = t.rendered(theme, 80);
        assert!(text_of(&r.lines).contains("row 20"));
    }

    #[test]
    fn streaming_flushes_into_an_assistant_entry() {
        let theme = Theme::default();
        let mut t = Transcript::new();
        t.append_delta("hel");
        t.append_delta("lo");
        assert_eq!(t.streaming(), "hello");
        assert!(t.flush_streaming());
        assert!(t.streaming().is_empty());
        {
            let r = t.rendered(theme, 40);
            assert_eq!(text_of(&r.lines).trim(), "hello");
        }
        assert!(!t.flush_streaming());
    }

    #[test]
    fn owner_map_points_rows_at_entries() {
        let theme = Theme::default();
        let mut t = Transcript::new();
        t.push_user("question");
        t.push_assistant("answer");
        let r = t.rendered(theme, 40);
        assert_eq!(r.owner.len(), r.lines.len());
        assert_eq!(r.owner[0], 0);
        assert_eq!(r.owner[1], 0, "blank row belongs to the user entry");
        assert!(r.owner.contains(&1));
        assert_eq!(r.starts.len(), 2);
    }

    #[test]
    fn entries_are_capped() {
        let mut t = Transcript::new();
        for i in 0..(MAX_ENTRIES + 20) {
            t.push_notice(Level::Info, format!("n{i}"));
        }
        assert_eq!(t.len(), MAX_ENTRIES);
    }

    #[test]
    fn search_finds_entries_and_previews() {
        let mut t = Transcript::new();
        t.push_user("find the needle here");
        t.push_assistant("nothing");
        t.start_tool("a", "read_file", "src/needle.rs");
        let hits = t.search("NEEDLE");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].entry, 0);
        assert!(hits[0].preview.to_lowercase().contains("needle"));
        assert!(t.search("  ").is_empty());
    }

    #[test]
    fn abandoned_running_cards_stop_spinning() {
        let theme = Theme::default();
        let mut t = Transcript::new();
        t.start_tool("a", "run_command", "cargo test");
        t.abandon_running("(interrupted)");
        let r = t.rendered(theme, 80);
        let text = text_of(&r.lines);
        assert!(text.contains("(interrupted)"), "got: {text}");
    }

    #[test]
    fn control_bytes_are_stripped_from_tool_output() {
        let theme = Theme::default();
        let mut t = Transcript::new();
        t.start_tool("a", "run_command", "printf");
        t.finish_tool(
            "a",
            "run_command",
            false,
            "\u{1b}[31mred\u{1b}[0m\ttext\nnext".to_string(),
            None,
            1,
        );
        let r = t.rendered(theme, 80);
        let text = text_of(&r.lines);
        assert!(!text.contains('\u{1b}'), "escape leaked: {text:?}");
        assert!(text.contains("red"));
        assert!(text.contains("text"));
    }

    #[test]
    fn last_assistant_returns_the_latest_answer() {
        let mut t = Transcript::new();
        assert!(t.last_assistant().is_none());
        t.push_assistant("one");
        t.push_assistant("two");
        assert_eq!(t.last_assistant(), Some("two"));
    }

    #[test]
    fn long_output_wraps_within_width() {
        let theme = Theme::default();
        let mut t = Transcript::new();
        t.start_tool("a", "read_file", "a.txt");
        t.finish_tool("a", "read_file", false, "x".repeat(200), None, 1);
        let r = t.rendered(theme, 40);
        for line in &r.lines {
            let w: usize = line
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(w <= 40, "row too wide ({w}): {line:?}");
        }
    }

    #[test]
    fn errors_show_more_rows_while_collapsed() {
        let theme = Theme::default();
        let mut t = Transcript::new();
        t.start_tool("a", "run_command", "false");
        let output: String = (1..=12).map(|i| format!("err {i}\n")).collect();
        t.finish_tool("a", "run_command", true, output, None, 4);
        let r = t.rendered(theme, 80);
        let text = text_of(&r.lines);
        assert!(text.contains("err 8"), "errors show 8 rows: {text}");
        assert!(!text.contains("err 9"));
    }
}
