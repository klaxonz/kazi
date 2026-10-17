//! Modal overlays: command palette, list pickers, help, sessions, search, and
//! the approval prompt.
//!
//! Overlays are a separate layer from the transcript on purpose: they float
//! over the screen, own the keyboard while open, and never steal layout rows.
//! Every list shares one renderer, so filtering/scrolling/selection behave the
//! same everywhere.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::theme::Theme;
use super::transcript::SearchHit;
use crate::agent::console;
use crate::agent::session;

/// Key bindings shown by the help overlay and the footer hint.
pub const KEY_BINDINGS: &[(&str, &str)] = &[
    ("Enter", "send the task (queues while the agent is busy)"),
    ("Ctrl+J", "insert a newline"),
    ("/", "open the command palette"),
    ("Ctrl+K", "open the command palette"),
    ("Tab", "complete a command or @path"),
    ("Ctrl+O", "toggle full tool output"),
    ("Ctrl+F", "search the transcript"),
    ("Ctrl+R", "browse saved sessions"),
    ("Ctrl+T", "cycle the colour theme"),
    ("Ctrl+E", "expand the newest tool card"),
    ("Shift+Tab", "cycle permission mode"),
    ("PgUp/PgDn", "scroll (Ctrl+Home/End jump)"),
    ("Mouse", "wheel scrolls · click a tool card to expand it"),
    ("Esc", "close overlay · interrupt a run · quit when idle"),
    ("Ctrl+C", "quit"),
    ("y / a / n", "approval: once / always this command / deny"),
    ("r", "approval: deny and type a reason for the model"),
];

/// Which list a [`Picker`] is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Provider,
    Model,
}

/// A filtered list of choices drawn as a centred modal.
#[derive(Debug, Clone)]
pub struct Picker {
    pub kind: PickerKind,
    pub title: String,
    pub all: Vec<String>,
    pub filter: String,
    pub cursor: usize,
    pub current: Option<String>,
    /// Live discovery still running (model lists): shown in the footer.
    pub loading: bool,
}

impl Picker {
    pub fn new(
        kind: PickerKind,
        title: impl Into<String>,
        all: Vec<String>,
        current: Option<&str>,
    ) -> Self {
        let cursor = current
            .and_then(|c| all.iter().position(|m| m == c))
            .unwrap_or(0);
        Self {
            kind,
            title: title.into(),
            all,
            filter: String::new(),
            cursor,
            current: current.map(str::to_string),
            loading: false,
        }
    }

    pub fn filtered(&self) -> Vec<&String> {
        if self.filter.trim().is_empty() {
            return self.all.iter().collect();
        }
        let needle = self.filter.to_lowercase();
        let mut exact: Vec<&String> = self
            .all
            .iter()
            .filter(|m| m.to_lowercase().contains(&needle))
            .collect();
        exact.sort_by_key(|m| {
            let lower = m.to_lowercase();
            (
                !lower.starts_with(&needle),
                lower.find(&needle).unwrap_or(usize::MAX),
            )
        });
        exact
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.filtered().len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let cur = self.cursor.min(len - 1) as isize;
        self.cursor = (cur + delta).clamp(0, len as isize - 1) as usize;
    }

    pub fn selected(&self) -> Option<String> {
        self.filtered().get(self.cursor).map(|m| (*m).clone())
    }

    pub fn push_filter(&mut self, c: char) {
        self.filter.push(c);
        self.cursor = 0;
    }

    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.cursor = 0;
    }

    /// Merge late-arriving candidates into the open list.
    pub fn merge(&mut self, rows: Vec<String>, current: Option<String>) -> usize {
        let mut added = 0;
        for row in rows {
            if !self.all.contains(&row) {
                self.all.push(row);
                added += 1;
            }
        }
        if let Some(cur) = current {
            if self.all.contains(&cur) {
                self.current = Some(cur);
            }
        }
        added
    }
}

/// Type-to-filter command list (`/` or Ctrl+K).
#[derive(Debug, Clone, Default)]
pub struct Palette {
    pub query: String,
    pub cursor: usize,
}

impl Palette {
    pub fn rows(&self) -> Vec<(String, String)> {
        let all = console::command_rows("/");
        if self.query.trim().is_empty() {
            return all;
        }
        let needle = self.query.trim().trim_start_matches('/');
        let mut scored: Vec<(i32, (String, String))> = all
            .into_iter()
            .filter_map(|(cmd, help)| {
                let hay = cmd.trim_start_matches('/');
                fuzzy_score(needle, hay)
                    .or_else(|| fuzzy_score(&needle.to_lowercase(), &help.to_lowercase()))
                    .map(|s| (s, (cmd, help)))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1 .0.cmp(&b.1 .0)));
        scored.into_iter().map(|(_, row)| row).collect()
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.rows().len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let cur = self.cursor.min(len - 1) as isize;
        self.cursor = (cur + delta).clamp(0, len as isize - 1) as usize;
    }

    pub fn selected(&self) -> Option<String> {
        self.rows().get(self.cursor).map(|(cmd, _)| cmd.clone())
    }

    /// Only the command name (no `<args>` placeholders) for the input line.
    pub fn selected_command(&self) -> Option<String> {
        self.selected()
            .and_then(|row| row.split_whitespace().next().map(str::to_string))
    }

    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.cursor = 0;
    }

    pub fn pop(&mut self) {
        self.query.pop();
        self.cursor = 0;
    }
}

/// Fuzzy subsequence score: higher is better, `None` when not a subsequence.
/// Consecutive matches and word starts score extra; case is ignored.
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.chars().collect();
    let mut score = 0i32;
    let mut h = 0usize;
    let mut last_hit: Option<usize> = None;
    for n in needle.chars() {
        let lower = n.to_ascii_lowercase();
        let mut found = false;
        while h < hay.len() {
            let c = hay[h];
            if c.to_ascii_lowercase() == lower {
                score += 1;
                if last_hit == Some(h.wrapping_sub(1)) {
                    score += 3; // consecutive run
                }
                if h == 0 || !hay[h - 1].is_alphanumeric() {
                    score += 4; // word start
                }
                last_hit = Some(h);
                h += 1;
                found = true;
                break;
            }
            h += 1;
        }
        if !found {
            return None;
        }
    }
    // prefer short haystacks when the score ties
    Some(score * 10 - haystack.chars().count() as i32)
}

/// Full-screen scrollable help.
#[derive(Debug, Clone, Default)]
pub struct Help {
    pub scroll: usize,
}

/// Saved-session browser (`Ctrl+R`, bare `/resume`).
#[derive(Debug, Clone, Default)]
pub struct Sessions {
    pub rows: Vec<session::SessionSummary>,
    pub filter: String,
    pub cursor: usize,
    /// Set after the first `d`: pressing `d` again deletes the selection.
    pub confirm_delete: Option<usize>,
}

impl Sessions {
    pub fn load() -> Self {
        Self {
            rows: session::list_summaries(),
            filter: String::new(),
            cursor: 0,
            confirm_delete: None,
        }
    }

    pub fn filtered(&self) -> Vec<&session::SessionSummary> {
        if self.filter.trim().is_empty() {
            return self.rows.iter().collect();
        }
        let needle = self.filter.to_lowercase();
        self.rows
            .iter()
            .filter(|s| {
                s.name.to_lowercase().contains(&needle)
                    || s.topic
                        .as_deref()
                        .map(|t| t.to_lowercase().contains(&needle))
                        .unwrap_or(false)
            })
            .collect()
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.filtered().len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let cur = self.cursor.min(len - 1) as isize;
        self.cursor = (cur + delta).clamp(0, len as isize - 1) as usize;
        self.confirm_delete = None;
    }

    pub fn selected(&self) -> Option<&session::SessionSummary> {
        self.filtered().get(self.cursor).copied()
    }

    pub fn push(&mut self, c: char) {
        self.filter.push(c);
        self.cursor = 0;
        self.confirm_delete = None;
    }

    pub fn pop(&mut self) {
        self.filter.pop();
        self.cursor = 0;
        self.confirm_delete = None;
    }
}

/// Incremental transcript search (`Ctrl+F`).
#[derive(Debug, Clone, Default)]
pub struct Search {
    pub query: String,
    pub cursor: usize,
    pub hits: Vec<SearchHit>,
}

impl Search {
    pub fn move_cursor(&mut self, delta: isize) {
        if self.hits.is_empty() {
            self.cursor = 0;
            return;
        }
        let cur = self.cursor.min(self.hits.len() - 1) as isize;
        self.cursor = (cur + delta).clamp(0, self.hits.len() as isize - 1) as usize;
    }

    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.cursor = 0;
    }

    pub fn pop(&mut self) {
        self.query.pop();
        self.cursor = 0;
    }
}

/// The open modal, if any. Keyboard routing checks this before the editor.
#[derive(Debug, Clone)]
pub enum Overlay {
    Palette(Palette),
    Picker(Picker),
    Help(Help),
    Sessions(Sessions),
    Search(Search),
}

/// Draw the open overlay centred over `screen`.
pub fn draw(f: &mut Frame, screen: Rect, theme: Theme, overlay: &Overlay) {
    match overlay {
        Overlay::Palette(p) => {
            let rows = p.rows();
            draw_list(
                f,
                screen,
                theme,
                " commands ",
                &rows,
                p.cursor,
                &format!(
                    "› {}_    {} match{} · ↑/↓ · Enter insert · Esc close",
                    p.query,
                    rows.len(),
                    if rows.len() == 1 { "" } else { "es" }
                ),
            );
        }
        Overlay::Picker(p) => {
            let visible = p.filtered();
            let rows: Vec<(String, String)> = visible
                .iter()
                .map(|m| {
                    let current = Some(m.as_str()) == p.current.as_deref();
                    (
                        (*m).clone(),
                        if current {
                            "current".to_string()
                        } else {
                            String::new()
                        },
                    )
                })
                .collect();
            let footer = format!(
                "filter: {}_   {} choice{} · ↑/↓ · Enter select · Esc cancel{}",
                p.filter,
                rows.len(),
                if rows.len() == 1 { "" } else { "s" },
                if p.loading {
                    " · loading live models…"
                } else {
                    ""
                }
            );
            draw_list(
                f,
                screen,
                theme,
                &format!(" {} ", p.title),
                &rows,
                p.cursor,
                &footer,
            );
        }
        Overlay::Help(h) => draw_help(f, screen, theme, h),
        Overlay::Sessions(s) => {
            let visible = s.filtered();
            let rows: Vec<(String, String)> = visible
                .iter()
                .map(|entry| {
                    let meta = format!(
                        "{} msg{} · {} · {}",
                        entry.messages,
                        if entry.messages == 1 { "" } else { "s" },
                        age(entry.saved_at),
                        entry.topic.as_deref().unwrap_or("(no messages)")
                    );
                    (entry.name.clone(), meta)
                })
                .collect();
            let mut footer = format!(
                "filter: {}_   {} session{} · ↑/↓ · Enter open · d delete · Esc close",
                s.filter,
                rows.len(),
                if rows.len() == 1 { "" } else { "s" }
            );
            if s.confirm_delete.is_some() {
                footer = "press d again to DELETE this session, any other key cancels".to_string();
            }
            draw_list(f, screen, theme, " sessions ", &rows, s.cursor, &footer);
        }
        Overlay::Search(s) => {
            let rows: Vec<(String, String)> = s
                .hits
                .iter()
                .enumerate()
                .map(|(i, hit)| (hit.preview.clone(), format!("#{}", i + 1)))
                .collect();
            let footer = if s.query.trim().is_empty() {
                "type to search the transcript · Esc close".to_string()
            } else {
                format!(
                    "find: {}_   {} match{} · Enter jump · Esc close",
                    s.query,
                    rows.len(),
                    if rows.len() == 1 { "" } else { "es" }
                )
            };
            draw_list(f, screen, theme, " search ", &rows, s.cursor, &footer);
        }
    }
}

fn draw_list(
    f: &mut Frame,
    screen: Rect,
    theme: Theme,
    title: &str,
    rows: &[(String, String)],
    cursor: usize,
    footer: &str,
) {
    let height = (rows.len() as u16 + 4).clamp(6, screen.height.saturating_sub(2).max(6));
    let rect = centred(screen, 92, height);
    clear_block(f, rect, theme, title);

    let inner = Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    };
    let visible = inner.height.saturating_sub(1) as usize; // footer row
    let (start, shown) = window(cursor, rows.len(), visible);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(shown);
    for (i, (left, right)) in rows.iter().enumerate().skip(start).take(shown) {
        let selected = i == cursor;
        lines.push(list_row(left, right, selected, inner.width as usize, theme));
    }
    if rows.is_empty() {
        lines.push(Line::styled(
            "  (nothing matches)",
            Style::default().fg(theme.warn),
        ));
    }
    let footer_line = Line::styled(
        format!(" {}", clip(footer, inner.width as usize)),
        Style::default().fg(theme.dim),
    );
    f.render_widget(Paragraph::new(lines), inner);
    if inner.height > 0 {
        f.render_widget(
            Paragraph::new(footer_line),
            Rect {
                x: inner.x,
                y: inner.bottom().saturating_sub(1),
                width: inner.width,
                height: 1,
            },
        );
    }
}

fn list_row(left: &str, right: &str, selected: bool, width: usize, theme: Theme) -> Line<'static> {
    let marker = if selected { theme.glyphs.prompt } else { " " };
    let right_w = UnicodeWidthStr::width(right);
    let budget = width.saturating_sub(4).saturating_sub(right_w + 2);
    let left = clip(left, budget);
    let left_w = UnicodeWidthStr::width(left.as_str());
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(
            format!(" {marker} "),
            Style::default()
                .fg(if selected { theme.accent } else { theme.dim })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            left,
            if selected {
                Style::default()
            } else {
                Style::default().fg(theme.text)
            },
        ),
    ];
    let mut used = left_w + 3;
    if !right.is_empty() && used + right_w < width {
        let pad = width.saturating_sub(used + right_w + 1);
        spans.push(Span::raw(" ".repeat(pad)));
        used += pad;
        spans.push(Span::styled(
            right.to_string(),
            if selected {
                Style::default()
            } else {
                Style::default().fg(theme.dim)
            },
        ));
        used += right_w;
    }
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    let line = Line::from(spans);
    if selected {
        line.style(theme.selection())
    } else {
        line
    }
}

fn draw_help(f: &mut Frame, screen: Rect, theme: Theme, help: &Help) {
    let rect = centred(screen, 100, screen.height.saturating_sub(4).max(8));
    clear_block(f, rect, theme, " help ");
    let inner = Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    };
    let body = help_lines(theme);
    let max = body.len().saturating_sub(inner.height as usize);
    let off = help.scroll.min(max);
    let visible: Vec<Line<'static>> = body.into_iter().skip(off).collect();
    f.render_widget(
        Paragraph::new(visible)
            .wrap(Wrap { trim: false })
            .scroll((0, 0)),
        inner,
    );
}

/// Everything the help overlay prints (commands + key bindings + notes).
pub fn help_lines(theme: Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::styled(
        " commands",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ));
    for (cmd, help) in console::command_rows("/") {
        out.push(Line::from(vec![
            Span::styled(format!("  {cmd:<26}"), Style::default().fg(theme.key)),
            Span::styled(format!(" {help}"), Style::default().fg(theme.text)),
        ]));
    }
    out.push(Line::default());
    out.push(Line::styled(
        " keys",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ));
    for (key, what) in KEY_BINDINGS {
        out.push(Line::from(vec![
            Span::styled(format!("  {key:<14}"), Style::default().fg(theme.key)),
            Span::styled(format!(" {what}"), Style::default().fg(theme.text)),
        ]));
    }
    out.push(Line::default());
    out.push(Line::styled(
        "  @path attaches a file's contents · /permission or Shift+Tab controls what runs unattended",
        Style::default().fg(theme.dim),
    ));
    out.push(Line::styled(
        "  Esc closes this help · PgUp/PgDn scroll",
        Style::default().fg(theme.dim),
    ));
    out
}

/// Approval prompt. Returns nothing; the caller answers by keystroke.
pub fn draw_approval(
    f: &mut Frame,
    screen: Rect,
    theme: Theme,
    tool: &str,
    summary: &str,
    reason_mode: bool,
    reason: &str,
) {
    let cap = screen.width.max(1);
    let width = screen
        .width
        .saturating_sub(4)
        .clamp(1, 110)
        .clamp(16.min(cap), cap);
    // estimate wrapped body height (the box must fit the key hints)
    let inner_w = width.saturating_sub(4) as usize;
    let body_lines: usize = summary
        .lines()
        .map(|l| UnicodeWidthStr::width(l).max(1).div_ceil(inner_w.max(1)))
        .sum::<usize>()
        .max(1);
    let height = (body_lines as u16 + 6)
        .min(screen.height.saturating_sub(1).max(3))
        .min(screen.height.max(1));
    let rect = Rect {
        x: (screen.width.saturating_sub(width)) / 2,
        y: (screen.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warn))
        .title(Span::styled(
            format!(" approval required · {tool} "),
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let body = Paragraph::new(summary)
        .style(Style::default().fg(theme.text))
        .wrap(Wrap { trim: false });
    let body_area = Rect {
        height: inner.height.saturating_sub(2),
        ..inner
    };
    f.render_widget(body, body_area);

    let hint = if reason_mode {
        format!(" reason for the model: {reason}█   · Enter send · Esc cancel")
    } else {
        " [y] run once   [a] always this command   [n] deny   [r] deny with a reason".to_string()
    };
    f.render_widget(
        Paragraph::new(Line::styled(
            hint,
            Style::default()
                .fg(if reason_mode {
                    theme.accent
                } else {
                    theme.warn
                })
                .add_modifier(Modifier::BOLD),
        )),
        Rect {
            y: inner.bottom().saturating_sub(1),
            height: 1,
            ..inner
        },
    );
}

/// A centred rect with a width budget and exact height, never wider than the
/// screen (tiny terminals must not paint outside the buffer).
fn centred(screen: Rect, max_width: u16, height: u16) -> Rect {
    let width = max_width
        .min(screen.width.saturating_sub(2).max(4))
        .min(screen.width.max(1));
    let width = width.max(1);
    let height = height.min(screen.height).max(3).min(screen.height.max(1));
    Rect {
        x: screen.x + (screen.width.saturating_sub(width)) / 2,
        y: screen.y + (screen.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn clear_block(f: &mut Frame, rect: Rect, theme: Theme, title: &str) {
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_active))
        .title(Span::styled(
            title.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Left);
    f.render_widget(block, rect);
}

/// `(start, shown)` for a list window that keeps `cursor` visible.
fn window(cursor: usize, len: usize, cap: usize) -> (usize, usize) {
    if cap == 0 || len == 0 {
        return (0, 0);
    }
    let shown = len.min(cap);
    let start = if cursor >= shown {
        (cursor + 1 - shown).min(len - shown)
    } else {
        0
    };
    (start, shown)
}

fn clip(s: &str, n: usize) -> String {
    let n = n.max(4);
    if UnicodeWidthStr::width(s) <= n {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > n.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

fn age(saved_at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(saved_at);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", secs / 60),
        3_600..=86_399 => format!("{}h ago", secs / 3_600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn fuzzy_prefers_prefix_and_consecutive_runs() {
        assert!(fuzzy_score("mod", "model").unwrap() > fuzzy_score("mod", "remodel").unwrap());
        assert!(fuzzy_score("cmp", "compact").is_some());
        assert!(fuzzy_score("xyz", "compact").is_none());
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn palette_filters_with_fuzzy_matching() {
        let mut p = Palette::default();
        assert!(p.rows().len() >= 15);
        p.query = "cmp".into();
        let rows = p.rows();
        assert!(
            rows.iter().any(|(cmd, _)| cmd.starts_with("/compact")),
            "rows: {rows:?}"
        );
        p.query = "zzz".into();
        assert!(p.rows().is_empty());
    }

    #[test]
    fn picker_filters_sorts_and_clamps() {
        let mut p = Picker::new(
            PickerKind::Model,
            "models",
            vec!["gpt-4o".into(), "gpt-4o-mini".into(), "claude".into()],
            Some("gpt-4o"),
        );
        assert_eq!(p.filtered().len(), 3);
        p.push_filter('g');
        assert_eq!(p.filtered().len(), 2);
        p.move_cursor(-5);
        assert_eq!(p.cursor, 0);
        p.move_cursor(99);
        assert_eq!(p.cursor, 1);
        assert_eq!(p.selected().as_deref(), Some("gpt-4o-mini"));
        p.pop_filter();
        assert_eq!(p.filter, "");
        assert_eq!(p.merge(vec!["gpt-4o".into(), "new".into()], None), 1);
    }

    #[test]
    fn picker_renders_as_an_overlay() {
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let theme = Theme::default();
        let p = Picker::new(
            PickerKind::Provider,
            "select provider",
            vec!["deepseek".into(), "ollama".into()],
            Some("deepseek"),
        );
        term.draw(|f| {
            let area = f.area();
            draw(f, area, theme, &Overlay::Picker(p.clone()));
        })
        .unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("select provider"));
        assert!(text.contains("deepseek"));
        assert!(text.contains("current"));
    }

    #[test]
    fn search_and_sessions_keep_selection_valid() {
        let mut s = Search {
            query: "x".into(),
            hits: vec![SearchHit {
                entry: 0,
                preview: "x".into(),
            }],
            ..Default::default()
        };
        s.move_cursor(5);
        assert_eq!(s.cursor, 0);

        let mut sessions = Sessions {
            rows: vec![],
            ..Default::default()
        };
        sessions.move_cursor(3);
        assert_eq!(sessions.cursor, 0);
        assert!(sessions.selected().is_none());
        sessions.push('a');
        sessions.pop();
        assert_eq!(sessions.filter, "");
    }

    #[test]
    fn help_lists_commands_and_keys() {
        let theme = Theme::default();
        let lines = help_lines(theme);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("/compact"));
        assert!(text.contains("Ctrl+F"));
        assert!(text.contains("Shift+Tab"));
    }

    #[test]
    fn list_window_follows_the_cursor() {
        assert_eq!(window(0, 0, 5), (0, 0));
        assert_eq!(window(0, 10, 5), (0, 5));
        assert_eq!(window(9, 10, 5), (5, 5));
        assert_eq!(window(2, 3, 5), (0, 3));
    }

    #[test]
    fn approval_prompt_renders_keys() {
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let theme = Theme::default();
        term.draw(|f| {
            let area = f.area();
            draw_approval(
                f,
                area,
                theme,
                "run_command",
                "$ git push --force\n  cwd: D:/Code/init/kazi",
                false,
                "",
            );
        })
        .unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("approval required"));
        assert!(text.contains("git push --force"));
        assert!(text.contains("[y] run once"));
    }
}
