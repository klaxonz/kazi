//! Full-screen TUI for `kazi chat` (the only chat mode), built on ratatui +
//! crossterm.
//!
//! Layout (top to bottom): a header bar (mode, provider/model, session, live
//! context gauge), the model's plan checklist, the scrolling conversation, a
//! bordered input box, and a key-hint footer. Modals — command palette, model
//! picker, help, session browser, transcript search and the approval prompt —
//! float above the screen and own the keyboard while open.
//!
//! Rendering lives in sibling modules: [`markdown`] (answers), [`highlight`]
//! (code fences), [`theme`] (palette + glyphs), [`transcript`] (structured
//! entries, tool cards, diffs) and [`overlay`] (modals). This file is the
//! state machine: editor, event pump, commands, and the draw loop.
//!
//! Chat is TUI-only: a terminal that cannot enter the alternate screen fails
//! the invocation — scripts and pipes use `kazi run "<task>"`.

mod highlight;
mod markdown;
mod overlay;
mod theme;
mod transcript;

use futures::StreamExt;
use std::collections::VecDeque;
use std::io::{Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use anyhow::Context as _;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::{Frame, Terminal};

use super::{Agent, AgentEvent};
use crate::tools::{Approver, Decision, PermissionMode};
use overlay::{Help, Overlay, Palette, Picker, PickerKind, Search, Sessions};
use theme::{Theme, ThemeName};
use transcript::{Level, Transcript};

/// Approval request travelling from the agent task to the UI loop.
pub struct ApprovalAsk {
    /// Tool that wants to run (`run_command`, ...).
    pub tool: String,
    pub summary: String,
    pub reply: tokio::sync::oneshot::Sender<Decision>,
}

/// Session-scoped "always allow" memory. Shared by every approver instance so
/// it survives in-TUI agent rebuilds (`/provider`, `/model`) instead of
/// forgetting approvals halfway through a session.
pub type SessionAllows = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// Channel-based approver for the TUI: the agent asks over a channel, the
/// main loop renders the question and answers with a keystroke. Never touches
/// stdin (raw mode owns it) and remembers "always" for the exact command.
pub struct TuiApprover {
    tx: tokio::sync::mpsc::UnboundedSender<ApprovalAsk>,
    session_allows: SessionAllows,
}

impl TuiApprover {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<ApprovalAsk>) -> Self {
        Self::with_allows(tx, SessionAllows::default())
    }

    pub fn with_allows(
        tx: tokio::sync::mpsc::UnboundedSender<ApprovalAsk>,
        session_allows: SessionAllows,
    ) -> Self {
        Self { tx, session_allows }
    }
}

#[async_trait::async_trait]
impl Approver for TuiApprover {
    async fn decide(&self, tool: &str, summary: &str) -> Decision {
        let key = crate::tools::approval_key(summary);
        if !key.is_empty()
            && self
                .session_allows
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|p| p == &key)
        {
            return Decision::Allow;
        }
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(ApprovalAsk {
                tool: tool.to_string(),
                summary: summary.to_string(),
                reply: rtx,
            })
            .is_err()
        {
            return Decision::Deny(None); // UI gone: fail closed
        }
        match rrx.await {
            Ok(Decision::AllowSession) => {
                if !key.is_empty() {
                    self.session_allows
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(key);
                }
                Decision::Allow
            }
            Ok(d) => d,
            Err(_) => Decision::Deny(None),
        }
    }
}

// ---------------------------------------------------------------------------
// editor
// ---------------------------------------------------------------------------

/// Multi-line editor state: text + a char-indexed caret + recallable history
/// (↑/↓), with readline-style shortcuts (Ctrl+A/E/U/K/W).
#[derive(Default)]
struct Input {
    text: String,
    /// caret position in CHARS (not bytes); always <= char count
    cursor: usize,
    history: Vec<String>,
    /// `Some(i)` while browsing history; the live draft is stashed meanwhile.
    hist_pos: Option<usize>,
    draft: String,
}

impl Input {
    fn byte_of(&self, ci: usize) -> usize {
        self.text
            .char_indices()
            .nth(ci)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    fn chars_len(&self) -> usize {
        self.text.chars().count()
    }

    fn insert(&mut self, ch: char) {
        let b = self.byte_of(self.cursor);
        self.text.insert(b, ch);
        self.cursor += 1;
    }

    /// Insert a pasted block at the caret; CRLF/CR normalize to `\n` so a
    /// multi-line paste lands as ONE task.
    fn insert_str(&mut self, text: &str) {
        let mut after_cr = false;
        for ch in text.chars() {
            match ch {
                '\r' => {
                    self.insert('\n');
                    after_cr = true;
                }
                '\n' if after_cr => after_cr = false,
                c => {
                    self.insert(c);
                    after_cr = false;
                }
            }
        }
    }

    fn set_text(&mut self, text: String) {
        self.text = text;
        self.cursor = self.chars_len();
        self.hist_pos = None;
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.hist_pos = None;
        self.draft.clear();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let b = self.byte_of(self.cursor - 1);
        self.text.remove(b);
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        if self.cursor >= self.chars_len() {
            return;
        }
        let b = self.byte_of(self.cursor);
        self.text.remove(b);
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        if self.cursor < self.chars_len() {
            self.cursor += 1;
        }
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.chars_len();
    }

    fn kill_to_start(&mut self) {
        let b = self.byte_of(self.cursor);
        self.text.drain(..b);
        self.cursor = 0;
    }

    fn kill_to_end(&mut self) {
        let b = self.byte_of(self.cursor);
        self.text.truncate(b);
    }

    /// Delete the whitespace-delimited word before the caret.
    fn delete_word(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let (bs, be) = (self.byte_of(i), self.byte_of(self.cursor));
        self.text.replace_range(bs..be, "");
        self.cursor = i;
    }

    fn newline(&mut self) {
        self.insert('\n');
    }

    fn line_count(&self) -> usize {
        self.text.matches('\n').count() + 1
    }

    /// Soft-wrap the buffer for the input box: `(rows, caret_row, caret_col)`.
    /// A long task wraps instead of scrolling horizontally, so the whole thing
    /// stays readable while it is being typed.
    fn visual_rows(&self, budget: usize) -> (Vec<String>, usize, usize) {
        let budget = budget.max(1);
        let mut rows: Vec<String> = Vec::new();
        let mut caret = (0usize, 0usize);
        let mut base = 0usize; // char index where this logical line starts
        for line in self.text.split('\n') {
            let lchars: Vec<char> = line.chars().collect();
            let caret_local = if self.cursor >= base && self.cursor <= base + lchars.len() {
                Some(self.cursor - base)
            } else {
                None
            };
            if lchars.is_empty() {
                rows.push(String::new());
                if caret_local == Some(0) {
                    caret = (rows.len() - 1, 0);
                }
            } else {
                let mut i = 0usize;
                while i < lchars.len() {
                    let mut end = i;
                    let mut w = 0usize;
                    while end < lchars.len() {
                        let cw = UnicodeWidthChar::width(lchars[end]).unwrap_or(0);
                        if w + cw > budget {
                            break;
                        }
                        w += cw;
                        end += 1;
                    }
                    if end == i {
                        end = i + 1; // a single glyph wider than the box
                    }
                    // break at a space when there is one in the window
                    let break_at = (i + 1..end)
                        .rev()
                        .find(|&k| lchars[k] == ' ')
                        .unwrap_or(end);
                    let text: String = lchars[i..break_at].iter().collect();
                    rows.push(text);
                    if let Some(cl) = caret_local {
                        if cl >= i && cl <= break_at {
                            let col = lchars[i..cl]
                                .iter()
                                .map(|c| UnicodeWidthChar::width(*c).unwrap_or(0))
                                .sum();
                            caret = (rows.len() - 1, col);
                        }
                    }
                    i = if break_at < lchars.len() && lchars[break_at] == ' ' {
                        break_at + 1
                    } else {
                        break_at
                    };
                }
            }
            // a caret after trailing spaces belongs to the end of the last row
            if caret_local == Some(lchars.len()) {
                if let Some(last) = rows.last() {
                    caret = (rows.len() - 1, UnicodeWidthStr::width(last.as_str()));
                }
            }
            base += lchars.len() + 1;
        }
        if rows.is_empty() {
            rows.push(String::new());
        }
        (rows, caret.0, caret.1)
    }

    /// (first char, one-past-last char) of the caret's display line.
    fn caret_line(&self) -> (usize, usize) {
        let chars: Vec<char> = self.text.chars().collect();
        let cur = self.cursor.min(chars.len());
        let start = chars[..cur]
            .iter()
            .rposition(|c| *c == '\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let end = chars[cur..]
            .iter()
            .position(|c| *c == '\n')
            .map(|i| cur + i)
            .unwrap_or(chars.len());
        (start, end)
    }

    fn up(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let cur = self.cursor.min(chars.len());
        let start = chars[..cur]
            .iter()
            .rposition(|c| *c == '\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if start == 0 {
            return;
        }
        let col = cur - start;
        let prev_start = chars[..start - 1]
            .iter()
            .rposition(|c| *c == '\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let prev_len = (start - 1) - prev_start;
        self.cursor = prev_start + col.min(prev_len);
    }

    fn down(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let cur = self.cursor.min(chars.len());
        let line_start = chars[..cur]
            .iter()
            .rposition(|c| *c == '\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let line_end = chars[cur..]
            .iter()
            .position(|c| *c == '\n')
            .map(|i| cur + i)
            .unwrap_or(chars.len());
        if line_end >= chars.len() {
            return;
        }
        let col = cur - line_start;
        let next_start = line_end + 1;
        let next_len = chars[next_start..]
            .iter()
            .position(|c| *c == '\n')
            .unwrap_or(chars.len() - next_start);
        self.cursor = next_start + col.min(next_len);
    }

    fn home_line(&mut self) {
        let (start, _) = self.caret_line();
        self.cursor = start;
    }

    fn end_line(&mut self) {
        let (_, end) = self.caret_line();
        self.cursor = end;
    }

    /// Trimmed submission; empty input clears without pushing history.
    fn submit(&mut self) -> Option<String> {
        let t = self.text.trim().to_string();
        if !t.is_empty() {
            self.history.push(t.clone());
        }
        self.clear();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.hist_pos {
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(p) => p - 1,
        };
        self.hist_pos = Some(pos);
        self.text = self.history[pos].clone();
        self.end();
    }

    fn history_next(&mut self) {
        let Some(p) = self.hist_pos else { return };
        if p + 1 >= self.history.len() {
            self.hist_pos = None;
            self.text = std::mem::take(&mut self.draft);
        } else {
            self.hist_pos = Some(p + 1);
            self.text = self.history[p + 1].clone();
        }
        self.end();
    }
}

// ---------------------------------------------------------------------------
// screen state
// ---------------------------------------------------------------------------

/// Work that finishes in the background and reports back to the event loop, so
/// the UI keeps drawing (spinner + elapsed) instead of freezing on MCP/HTTP
/// work.
enum BackgroundEvent {
    Switched {
        agent: Box<Agent>,
        provider: Option<String>,
        model: Option<String>,
        desc: String,
    },
    Failed {
        agent: Box<Agent>,
        note: String,
    },
    Compacted {
        agent: Box<Agent>,
        note: String,
    },
    /// Live model candidates for the open picker (rows are `nick/model`).
    Choices {
        rows: Vec<String>,
        current: Option<String>,
    },
}

struct PendingApproval {
    ask: ApprovalAsk,
    /// `r` was pressed: the next Enter denies with the typed reason.
    reason_mode: bool,
    reason: String,
}

/// Everything needed to render one screenful.
struct Screen {
    provider_desc: String,
    session_name: String,
    cwd: String,
    max_rounds: u32,
    max_context: u64,
    transcript: Transcript,
    input: Input,
    /// The model's plan checklist: (content, status) pairs.
    todos: Vec<(String, String)>,
    rounds: u32,
    tokens_in: u64,
    tokens_out: u64,
    est_tokens: u64,
    /// Transcript scroll: lines above the bottom (0 = follow the tail).
    scroll: std::cell::Cell<usize>,
    last_total: std::cell::Cell<usize>,
    /// Rows of the last frame's conversation area, for click hit-testing.
    body_area: std::cell::Cell<Rect>,
    body_width: std::cell::Cell<u16>,
    hit_owner: std::cell::RefCell<Vec<usize>>,
    started: std::cell::Cell<Option<std::time::Instant>>,
    spinner: std::cell::Cell<usize>,
    busy: bool,
    /// Follow-ups typed while a task runs; started automatically when it ends.
    queued: VecDeque<String>,
    /// The last submitted task (`/retry`).
    last_task: Option<String>,
    pending_approval: Option<PendingApproval>,
    overlay: Option<Overlay>,
    /// Highlighted row of the inline `/` dropdown.
    cmd_sel: std::cell::Cell<usize>,
    mode: PermissionMode,
    theme: Theme,
    quit: bool,
}

impl Screen {
    #[allow(clippy::too_many_arguments)]
    fn new(
        provider_desc: &str,
        session_name: &str,
        max_rounds: u32,
        max_context: u64,
        theme: Theme,
        details: bool,
        cwd: &str,
    ) -> Self {
        let input = Input {
            history: super::console::load_input_history(),
            ..Default::default()
        };
        let transcript = Transcript::new();
        transcript.set_details(details);
        Self {
            provider_desc: provider_desc.to_string(),
            session_name: session_name.to_string(),
            cwd: cwd.to_string(),
            max_rounds,
            max_context,
            transcript,
            input,
            todos: Vec::new(),
            rounds: 0,
            tokens_in: 0,
            tokens_out: 0,
            est_tokens: 0,
            scroll: std::cell::Cell::new(0),
            last_total: std::cell::Cell::new(0),
            body_area: std::cell::Cell::new(Rect::new(0, 0, 0, 0)),
            body_width: std::cell::Cell::new(80),
            hit_owner: std::cell::RefCell::new(Vec::new()),
            started: std::cell::Cell::new(None),
            spinner: std::cell::Cell::new(0),
            busy: false,
            queued: VecDeque::new(),
            last_task: None,
            pending_approval: None,
            overlay: None,
            cmd_sel: std::cell::Cell::new(0),
            mode: PermissionMode::Edit,
            theme,
            quit: false,
        }
    }

    /// Render one agent event into the screen state.
    fn consume(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TextDelta { text } => self.transcript.append_delta(&text),
            AgentEvent::ToolStart { id, name, summary } => {
                self.flush_streaming();
                self.transcript.start_tool(id, name, summary);
            }
            AgentEvent::ToolEnd {
                id,
                name,
                is_error,
                output,
                diff,
                elapsed_ms,
                ..
            } => {
                self.transcript
                    .finish_tool(&id, &name, is_error, output, diff, elapsed_ms);
            }
            AgentEvent::TodosUpdated { items } => self.todos = items,
            AgentEvent::Feedback {
                command,
                ok,
                preview,
            } => {
                self.flush_streaming();
                if ok {
                    self.push_notice(
                        Level::Success,
                        format!("feedback `{command}` ok — {preview}"),
                    );
                } else {
                    self.push_notice(
                        Level::Warn,
                        format!("feedback `{command}` failed — {preview}"),
                    );
                }
            }
            AgentEvent::AutoCompactStarted => {
                self.push_notice(Level::Info, "auto-compacting the context …")
            }
            AgentEvent::AutoCompactDone { note, est_tokens } => {
                self.est_tokens = est_tokens;
                self.push_notice(Level::Info, format!("{note} (est. {est_tokens} tokens)"));
            }
            AgentEvent::RoundDone {
                round,
                usage,
                est_tokens,
            } => {
                self.rounds = round;
                self.tokens_in = usage.input_tokens.unwrap_or(0);
                self.tokens_out = usage.output_tokens.unwrap_or(0);
                self.est_tokens = est_tokens;
            }
        }
    }

    fn flush_streaming(&mut self) {
        self.transcript.flush_streaming();
    }

    fn push_notice(&mut self, level: Level, text: impl Into<String>) {
        self.transcript.push_notice(level, text);
    }

    /// Multi-line command output (status/context/undo reports) into the
    /// transcript without pretending it is the assistant speaking.
    fn show_text(&mut self, text: impl AsRef<str>) {
        for line in text.as_ref().lines() {
            self.transcript.push_notice(Level::Info, line.to_string());
        }
    }

    /// Begin a task: transcript user entry, counters, scroll.
    fn begin_task(&mut self, task: &str) {
        self.flush_streaming();
        self.transcript.push_user(task);
        self.last_task = Some(task.to_string());
        self.busy = true;
        self.started.set(Some(std::time::Instant::now()));
        self.spinner.set(0);
        self.scroll_to_bottom();
    }

    fn scroll_up(&self, n: usize) {
        self.scroll.set(self.scroll.get().saturating_add(n));
    }

    fn scroll_down(&self, n: usize) {
        self.scroll.set(self.scroll.get().saturating_sub(n));
    }

    fn scroll_to_bottom(&self) {
        self.scroll.set(0);
    }

    fn scroll_to_top(&self) {
        self.scroll.set(usize::MAX); // clamped in draw()
    }

    /// Put the viewport at the top of one transcript entry (search jump).
    fn scroll_to_entry(&self, entry: usize) {
        let width = self.body_width.get().max(20);
        let height = self.body_area.get().height as usize;
        let cache = self.transcript.rendered(self.theme, width);
        let Some(start) = cache.starts.get(entry).copied() else {
            return;
        };
        let total = cache.lines.len();
        drop(cache);
        if height == 0 {
            return;
        }
        let off = total.saturating_sub(start + height);
        self.scroll.set(off);
    }

    /// Completion rows for the inline `/` dropdown.
    fn command_hints(&self) -> Vec<(String, String)> {
        match super::console::pending_command(&self.input.text) {
            Some(token) => super::console::command_rows(token),
            None => Vec::new(),
        }
    }

    fn move_command_sel(&self, delta: isize) {
        let len = self.command_hints().len();
        if len == 0 {
            return;
        }
        let cur = self.cmd_sel.get().min(len - 1) as isize;
        self.cmd_sel
            .set((cur + delta).rem_euclid(len as isize) as usize);
    }

    /// Enter/Tab resolution of a half-typed command: the unique match, or the
    /// row the dropdown is highlighting.
    fn resolve_command_input(&self) -> Option<String> {
        let token = super::console::pending_command(&self.input.text)?;
        let rows = super::console::command_rows(token);
        if rows.is_empty() {
            return None;
        }
        let pick = if rows.len() == 1 {
            0
        } else {
            self.cmd_sel.get().min(rows.len() - 1)
        };
        let name = rows[pick].0.split_whitespace().next()?.to_string();
        Some(name)
    }

    fn input_rows(&self, width: u16) -> u16 {
        // grow with the *wrapped* row count (a long single-line task wraps)
        let budget = width.saturating_sub(4).max(4) as usize;
        (self.input.visual_rows(budget).0.len().clamp(1, 6) as u16) + 2
    }

    // -- drawing ----------------------------------------------------------

    fn draw(&self, f: &mut Frame) {
        let area = f.area();
        // A tiny terminal (resize in progress, split pane) cannot fit every
        // band: shrink the optional ones before asking ratatui for areas that
        // would fall outside the buffer.
        if area.width < 12 || area.height < 4 {
            return;
        }
        let mut plan_rows = if self.todos.is_empty() {
            0
        } else {
            (self.todos.len().min(5) + 1) as u16
        };
        let mut input_rows = self.input_rows(area.width);
        let mut footer_rows = 1u16;
        while 1 + plan_rows + 1 + input_rows + footer_rows > area.height {
            if plan_rows > 0 {
                plan_rows -= 1;
            } else if footer_rows > 0 {
                footer_rows = 0;
            } else if input_rows > 1 {
                input_rows -= 1;
            } else {
                break;
            }
        }
        let [header, plan, body, input, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(plan_rows),
            Constraint::Min(1),
            Constraint::Length(input_rows),
            Constraint::Length(footer_rows),
        ])
        .areas(area);

        self.draw_header(f, header);
        self.draw_plan(f, plan);
        self.draw_body(f, body);
        self.draw_input(f, input);
        if footer.height > 0 {
            self.draw_footer(f, footer);
        }

        if let Some(pending) = &self.pending_approval {
            overlay::draw_approval(
                f,
                area,
                self.theme,
                &pending.ask.tool,
                &pending.ask.summary,
                pending.reason_mode,
                &pending.reason,
            );
        } else if let Some(ov) = &self.overlay {
            overlay::draw(f, area, self.theme, ov);
        } else {
            self.draw_command_hints(f, input);
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let theme = self.theme;
        f.render_widget(
            Block::default().style(Style::default().bg(theme.header_bg)),
            area,
        );
        let right = self.header_right();
        let right_w = spans_width(&right)
            .min(area.width.saturating_sub(8) as usize)
            .max(1) as u16;
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Min(10), Constraint::Length(right_w.max(1))])
                .areas(area);

        let badge = if theme.name == ThemeName::Mono {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Black)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        };
        let mode_style = match self.mode {
            PermissionMode::ReadOnly => Style::default().fg(theme.warn),
            PermissionMode::Edit => Style::default().fg(theme.dim),
            PermissionMode::Yolo => Style::default().fg(theme.err).add_modifier(Modifier::BOLD),
        };
        let prov = clip(
            &self.provider_desc,
            (left_area.width as usize).saturating_sub(38),
        );
        let mut spans = vec![
            Span::styled(" kazi ", badge),
            Span::styled(format!(" [{}]", self.mode.as_str()), mode_style),
            Span::styled(format!("  {prov}"), Style::default().fg(theme.dim)),
            Span::styled(
                format!("  {}", clip(&self.session_name, 24)),
                Style::default().fg(theme.accent),
            ),
        ];
        if left_area.width > 100 {
            spans.push(Span::styled(
                format!("  {}", clip(&self.cwd, 30)),
                Style::default().fg(theme.faint),
            ));
        }
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.header_bg)),
            left_area,
        );
        f.render_widget(
            Paragraph::new(Line::from(right))
                .alignment(Alignment::Right)
                .style(Style::default().bg(theme.header_bg)),
            right_area,
        );
    }

    fn header_right(&self) -> Vec<Span<'static>> {
        let theme = self.theme;
        let mut spans = Vec::new();
        // context gauge
        let pct = self
            .est_tokens
            .saturating_mul(100)
            .checked_div(self.max_context)
            .unwrap_or(0);
        let cells = 8usize;
        let filled = ((pct as usize) * cells / 100).min(cells);
        let gauge = format!(
            "{}{}",
            theme.glyphs.bar_full.repeat(filled),
            theme.glyphs.bar_empty.repeat(cells - filled)
        );
        spans.push(Span::styled(
            format!("{} {:>3}% ", gauge, pct),
            Style::default().fg(theme.usage_color(self.est_tokens, self.max_context)),
        ));
        spans.push(Span::styled(
            format!(
                "{} / {}  ",
                compact_tokens(self.est_tokens),
                compact_tokens(self.max_context)
            ),
            Style::default().fg(theme.dim),
        ));
        if self.busy {
            let secs = self
                .started
                .get()
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            spans.push(Span::styled(
                format!(
                    "{} {}s  r{}/{}  ",
                    theme.spinner(self.spinner.get()),
                    secs,
                    self.rounds,
                    self.max_rounds
                ),
                Style::default().fg(theme.accent),
            ));
            spans.push(Span::styled(
                format!("↑{} ↓{} ", self.tokens_in, self.tokens_out),
                Style::default().fg(theme.faint),
            ));
        }
        spans
    }

    fn draw_plan(&self, f: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let theme = self.theme;
        let mut lines = vec![Line::from(vec![
            Span::styled(
                " plan",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  {}/{} done",
                    self.todos.iter().filter(|(_, s)| s == "completed").count(),
                    self.todos.len()
                ),
                Style::default().fg(theme.faint),
            ),
        ])];
        for (content, status) in self.todos.iter().take(5) {
            let (marker, style) = match status.as_str() {
                "completed" => (
                    "[x]",
                    Style::default()
                        .fg(theme.ok)
                        .add_modifier(Modifier::CROSSED_OUT),
                ),
                "in_progress" => ("[>]", Style::default().fg(theme.accent)),
                _ => ("[ ]", Style::default().fg(theme.text)),
            };
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(marker, style),
                Span::styled(
                    format!(" {}", clip(content, area.width.saturating_sub(8) as usize)),
                    if status == "in_progress" {
                        Style::default().fg(theme.text)
                    } else {
                        Style::default().fg(theme.dim)
                    },
                ),
            ]));
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_body(&self, f: &mut Frame, area: Rect) {
        let theme = self.theme;
        let [main, bar] =
            Layout::horizontal([Constraint::Min(10), Constraint::Length(1)]).areas(area);
        let width = main.width.max(10);
        self.body_width.set(width);
        let stream = self.transcript.streaming();
        let stream_rows = if stream.trim().is_empty() {
            Vec::new()
        } else {
            markdown::render(stream, theme, width as usize)
        };
        let cache = self.transcript.rendered(theme, width);
        let static_len = cache.lines.len();
        let total = static_len + stream_rows.len();
        // a scrolled reader keeps their place as new output arrives
        let grew = total.saturating_sub(self.last_total.replace(total));
        if grew > 0 && self.scroll.get() > 0 {
            self.scroll.set(self.scroll.get() + grew);
        }
        let height = main.height as usize;
        let (start, end, off) = scroll_range(total, height, self.scroll.get());
        self.scroll.set(off);

        let mut visible: Vec<Line<'static>> = Vec::with_capacity(end.saturating_sub(start));
        let mut owners: Vec<usize> = Vec::with_capacity(end.saturating_sub(start));
        for i in start..end {
            if i < static_len {
                visible.push(cache.lines[i].clone());
                owners.push(cache.owner.get(i).copied().unwrap_or(usize::MAX));
            } else if let Some(row) = stream_rows.get(i - static_len) {
                visible.push(row.clone());
                owners.push(usize::MAX);
            }
        }
        drop(cache);
        *self.hit_owner.borrow_mut() = owners;
        self.body_area.set(main);
        f.render_widget(Paragraph::new(visible), main);

        if total > height && height > 0 && bar.width > 0 {
            let mut state = ScrollbarState::new(total.saturating_sub(height)).position(start);
            let thumb = if theme.name == ThemeName::Mono {
                "#"
            } else {
                "█"
            };
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None)
                    .track_symbol(Some(theme.glyphs.vbar))
                    .thumb_symbol(thumb)
                    .style(Style::default().fg(theme.border)),
                bar,
                &mut state,
            );
        }
    }

    fn draw_input(&self, f: &mut Frame, area: Rect) {
        let theme = self.theme;
        let border = if self.busy {
            Style::default().fg(theme.border)
        } else {
            Style::default().fg(theme.border_active)
        };
        let title = if self.busy {
            Span::styled(
                format!(" input · {} waiting ", self.queued.len()),
                Style::default().fg(theme.dim),
            )
        } else {
            Span::styled(" input ", Style::default().fg(theme.dim))
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(title);
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let prompt = format!("{} ", theme.glyphs.prompt);
        let prompt_w = UnicodeWidthStr::width(prompt.as_str());
        let budget = (inner.width as usize).saturating_sub(prompt_w);
        let (rows, caret_row, caret_col) = self.input.visual_rows(budget);
        let visible_rows = inner.height as usize;
        let first = caret_row.saturating_sub(visible_rows.saturating_sub(1));

        if self.input.text.trim().is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        prompt,
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "ask kazi to change code, run a command, or explain something…",
                        Style::default().fg(theme.faint),
                    ),
                ])),
                inner,
            );
            if self.pending_approval.is_none() && self.overlay.is_none() {
                f.set_cursor_position(ratatui::layout::Position {
                    x: inner.x + prompt_w as u16,
                    y: inner.y,
                });
            }
            return;
        }

        let mut lines: Vec<Line> = Vec::new();
        for (i, text) in rows.iter().enumerate().skip(first).take(visible_rows) {
            let lead = if i == first {
                Span::styled(
                    prompt.clone(),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(" ".repeat(prompt_w))
            };
            lines.push(Line::from(vec![lead, Span::raw(text.clone())]));
        }
        f.render_widget(
            Paragraph::new(lines).style(Style::default().fg(theme.text)),
            inner,
        );
        if self.pending_approval.is_none() && self.overlay.is_none() {
            let x = (inner.x as usize + prompt_w + caret_col) as u16;
            let y = inner.y + (caret_row - first) as u16;
            f.set_cursor_position(ratatui::layout::Position {
                x: x.min(inner.right().saturating_sub(1)),
                y: y.min(inner.bottom().saturating_sub(1)),
            });
        }
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let theme = self.theme;
        let hints: Vec<Span> = if self.pending_approval.is_some() {
            vec![Span::styled(
                " approval pending — y run · a always · n deny · r deny with reason · Esc deny ",
                Style::default().fg(theme.warn),
            )]
        } else if self.busy {
            vec![
                Span::styled(" Esc", theme.key_style()),
                Span::styled(" interrupt  ", Style::default().fg(theme.dim)),
                Span::styled("Enter", theme.key_style()),
                Span::styled(" queue a follow-up  ", Style::default().fg(theme.dim)),
                Span::styled("Ctrl+O", theme.key_style()),
                Span::styled(" details  ", Style::default().fg(theme.dim)),
                Span::styled("Ctrl+F", theme.key_style()),
                Span::styled(" find", Style::default().fg(theme.dim)),
            ]
        } else {
            vec![
                Span::styled(" Enter", theme.key_style()),
                Span::styled(" send  ", Style::default().fg(theme.dim)),
                Span::styled("/", theme.key_style()),
                Span::styled(" commands  ", Style::default().fg(theme.dim)),
                Span::styled("Ctrl+K", theme.key_style()),
                Span::styled(" palette  ", Style::default().fg(theme.dim)),
                Span::styled("Ctrl+O", theme.key_style()),
                Span::styled(" details  ", Style::default().fg(theme.dim)),
                Span::styled("Ctrl+F", theme.key_style()),
                Span::styled(" find  ", Style::default().fg(theme.dim)),
                Span::styled("Ctrl+R", theme.key_style()),
                Span::styled(" sessions  ", Style::default().fg(theme.dim)),
                Span::styled("Esc", theme.key_style()),
                Span::styled(" quit", Style::default().fg(theme.dim)),
            ]
        };
        f.render_widget(
            Paragraph::new(Line::from(hints)).style(Style::default().bg(theme.header_bg)),
            area,
        );

        let mut right: Vec<Span> = Vec::new();
        if !self.queued.is_empty() {
            right.push(Span::styled(
                format!("{} queued  ", self.queued.len()),
                Style::default().fg(theme.accent),
            ));
        }
        if self.transcript.details() {
            right.push(Span::styled(
                "details: on  ",
                Style::default().fg(theme.warn),
            ));
        }
        if self.scroll.get() > 0 {
            right.push(Span::styled(
                "scrolled · Ctrl+End  ",
                Style::default().fg(theme.warn),
            ));
        }
        if !right.is_empty() {
            let w = spans_width(&right).min(area.width as usize / 2) as u16;
            let [_, r] =
                Layout::horizontal([Constraint::Min(1), Constraint::Length(w.max(1))]).areas(area);
            f.render_widget(
                Paragraph::new(Line::from(right))
                    .alignment(Alignment::Right)
                    .style(Style::default().bg(theme.header_bg)),
                r,
            );
        }
    }

    fn draw_command_hints(&self, f: &mut Frame, input: Rect) {
        let hints = self.command_hints();
        if hints.is_empty() {
            return;
        }
        let avail = input.y.saturating_sub(1) as usize;
        if avail < 2 {
            return;
        }
        let shown = hints.len().min((avail - 1).min(10));
        let height = (shown + 1) as u16;
        let area = Rect {
            x: 0,
            y: input.y - height,
            width: f.area().width,
            height,
        };
        let theme = self.theme;
        let mut lines: Vec<Line> = Vec::new();
        let cmd_w = hints
            .iter()
            .take(shown)
            .map(|(c, _)| UnicodeWidthStr::width(c.as_str()))
            .max()
            .unwrap_or(8);
        let sel = self.cmd_sel.get().min(shown.saturating_sub(1));
        for (i, (cmd, help)) in hints.iter().take(shown).enumerate() {
            let selected = i == sel;
            let mut spans = vec![
                Span::styled(
                    format!(" {} ", if selected { theme.glyphs.prompt } else { " " }),
                    Style::default().fg(theme.accent),
                ),
                Span::styled(
                    format!("{cmd:<cmd_w$} "),
                    if selected {
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.key)
                    },
                ),
                Span::styled(
                    clip(help, area.width as usize / 2),
                    Style::default().fg(theme.dim),
                ),
            ];
            if selected {
                spans.insert(0, Span::raw(""));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::styled(
            if hints.len() > shown {
                format!(
                    "  … +{} more · ↑/↓ · Tab complete · Enter run",
                    hints.len() - shown
                )
            } else {
                "  ↑/↓ select · Tab complete · Enter run".to_string()
            },
            Style::default().fg(theme.faint),
        ));
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(theme.header_bg)),
            area,
        );
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn scroll_range(total: usize, height: usize, off: usize) -> (usize, usize, usize) {
    let off = off.min(total.saturating_sub(height));
    let end = total.saturating_sub(off);
    let start = end.saturating_sub(height);
    (start, end, off)
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

/// Clip to `n` characters (not columns) with an ellipsis.
fn clip(text: &str, n: usize) -> String {
    let n = n.max(4);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > n.saturating_sub(1) {
            out.push('…');
            return out;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// Resolve a theme, honouring the `KAZI_ASCII=1` glyph fallback.
fn theme_named(name: ThemeName) -> Theme {
    let mut theme = Theme::for_name(name);
    if theme::ascii_requested() {
        theme.glyphs = Theme::for_name(ThemeName::Mono).glyphs;
    }
    theme
}

fn compact_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Opt-in key-event tracing to diagnose terminal quirks: set
/// `KAZI_TUI_DEBUG=1` and every key event lands in
/// `<temp>/kazi-tui-keys.log`. Zero cost when the env var is absent.
fn log_key_event(key: &ratatui::crossterm::event::KeyEvent) {
    if std::env::var_os("KAZI_TUI_DEBUG").is_none() {
        return;
    }
    let path = std::env::temp_dir().join("kazi-tui-keys.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{:?} {:?}", key.code, key.modifiers);
    }
}

/// The character a key event produces, if any. Modifiers that mean "command"
/// (Ctrl/Alt) never become text — except AltGr, which terminals report as
/// Ctrl+Alt and which must keep typing `@`, `{`, `\` on many layouts.
fn accepted_char(code: &KeyCode, mods: KeyModifiers) -> Option<char> {
    let KeyCode::Char(c) = code else { return None };
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    if ctrl && alt {
        return Some(*c); // AltGr
    }
    if ctrl || alt || mods.contains(KeyModifiers::SUPER) {
        return None;
    }
    Some(*c)
}

fn first_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(n)
        .collect();
    let mut out = lines.join("\n");
    if text.lines().filter(|l| !l.trim().is_empty()).count() > n {
        out.push_str("\n…");
    }
    out
}

// ---------------------------------------------------------------------------
// agent pump
// ---------------------------------------------------------------------------

enum Pump {
    Ev(AgentEvent),
    Done(Result<String, super::AgentError>),
}

/// Take the agent out of the slot and run one task, streaming events to `tx`.
/// The agent is parked back before `Done` is sent.
async fn launch_task(
    screen: &mut Screen,
    slot: &Arc<tokio::sync::Mutex<Option<Agent>>>,
    tx: &tokio::sync::mpsc::UnboundedSender<Pump>,
    task: String,
) -> Option<(tokio::task::JoinHandle<()>, Arc<AtomicBool>)> {
    let Some(owned) = slot.lock().await.take() else {
        screen.push_notice(Level::Warn, BUSY_TEXT);
        return None;
    };
    screen.begin_task(&task);
    let cancel = owned.cancel_token();
    let expanded = super::expand_file_refs(&task, &owned.ctx.cwd);
    let slot2 = slot.clone();
    let tx2 = tx.clone();
    let handle = tokio::spawn(async move {
        let mut owned = owned;
        let out = owned
            .run_task(&expanded, |ev| {
                let _ = tx2.send(Pump::Ev(ev));
            })
            .await;
        slot2.lock().await.replace(owned);
        let _ = tx2.send(Pump::Done(out));
    });
    Some((handle, cancel))
}

// ---------------------------------------------------------------------------
// startup
// ---------------------------------------------------------------------------

/// Entry used by the CLI: builds the agent (session naming and `--resume`
/// semantics like `kazi run`), runs the TUI, and saves at the end.
/// Terminal failures (no tty, alternate-screen refused) surface as an error —
/// chat is TUI-only; scripts use `kazi run "<task>"`.
pub async fn chat_loop_with(
    cfg: crate::config::Config,
    provider: Option<String>,
    model: Option<String>,
    mode: PermissionMode,
    topic: Option<String>,
    resume: Option<String>,
) -> anyhow::Result<u8> {
    let session_name = match resume.as_deref() {
        Some(name) => super::session::resolve(name).unwrap_or_else(|| name.to_string()),
        None => {
            let base = match topic
                .as_deref()
                .map(super::console::slugify)
                .filter(|s| !s.is_empty())
            {
                Some(n) => n,
                None => super::session::default_name(),
            };
            let mut name = base.clone();
            let mut n = 2;
            while super::session::load(&name).is_ok() {
                name = format!("{base}-{n}");
                n += 1;
            }
            name
        }
    };
    let (ask_tx, ask_rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalAsk>();
    let ask_tx_rebuild = ask_tx.clone();
    let session_allows = SessionAllows::default();
    let approver: Arc<dyn Approver> = if mode == PermissionMode::Yolo {
        Arc::new(crate::tools::AutoApprover)
    } else {
        Arc::new(TuiApprover::with_allows(ask_tx, session_allows.clone()))
    };
    let mut agent = match Agent::default_cli_with_approver(
        &cfg,
        provider.as_deref(),
        model.as_deref(),
        approver,
        mode,
    )
    .await
    {
        Ok(a) => a,
        Err(e) => anyhow::bail!("{e}"),
    };

    if let Some(name) = &resume {
        match super::session::resolve(name) {
            Some(n) => match super::session::load(&n) {
                Ok(f) => {
                    let sys = f.system.or_else(|| agent.history.system.clone());
                    agent.history = super::context::History::new(sys);
                    agent.history.msgs = f.messages;
                    eprintln!("resumed `{n}`");
                }
                Err(e) => eprintln!("cannot resume `{n}`: {e}"),
            },
            None => eprintln!(
                "no stored session matches `{name}` — starting a NEW session named `{name}`"
            ),
        }
    }

    // Which session actually reached the disk? Quitting on a still-running
    // first task must not print "session saved".
    let saved_name = Arc::new(std::sync::Mutex::new(None::<String>));
    let save = |name: &str, agent: &Agent| {
        if agent.history.msgs_len() == 0 {
            return;
        }
        if super::session::save(name, agent.history.system.as_deref(), &agent.history.msgs).is_ok()
        {
            *saved_name.lock().unwrap_or_else(|e| e.into_inner()) = Some(name.to_string());
        }
    };
    let mut sess = SessionState {
        cfg: cfg.clone(),
        provider: provider.clone(),
        model: model.clone(),
        session_name: session_name.clone(),
    };

    match chat_loop_tui(
        agent,
        session_name,
        save,
        ask_rx,
        &mut sess,
        ask_tx_rebuild,
        session_allows,
    )
    .await
    {
        Ok(code) => {
            match saved_name.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                Some(name) => println!("\x1b[2msession saved -> {name}\x1b[0m"),
                None => println!("\x1b[2mnothing to save yet — no task has completed\x1b[0m"),
            }
            Ok(code)
        }
        Err(e) => Err(e).context(
            "the full-screen TUI could not start — `kazi chat` needs a real terminal; non-interactive invocations use `kazi run \"<task>\"`",
        ),
    }
}

/// Mutable session view the in-TUI slash commands act on.
pub struct SessionState {
    pub cfg: crate::config::Config,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub session_name: String,
}

/// Run the TUI chat loop. `chat_loop_tui` owns its terminal; stdout is
/// restored on every return path.
pub async fn chat_loop_tui(
    agent: Agent,
    session_name: String,
    on_session_save: impl Fn(&str, &Agent),
    mut approvals: tokio::sync::mpsc::UnboundedReceiver<ApprovalAsk>,
    sess: &mut SessionState,
    ask_tx: tokio::sync::mpsc::UnboundedSender<ApprovalAsk>,
    session_allows: SessionAllows,
) -> anyhow::Result<u8> {
    let provider_desc = agent.provider().describe();
    let max_rounds = agent.max_rounds();
    let max_context = agent.max_context_tokens();
    let permission = agent.ctx.permission.clone();
    let cwd = agent.ctx.cwd.display().to_string();
    let theme = theme::theme_from_config(sess.cfg.ui.theme.as_deref());
    let details = sess.cfg.ui.details.unwrap_or(false);
    let (bg_tx, mut background) = tokio::sync::mpsc::unbounded_channel::<BackgroundEvent>();

    enable_raw_mode()?;
    let mut stdout: Stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut screen = Screen::new(
        &provider_desc,
        &session_name,
        max_rounds,
        max_context,
        theme,
        details,
        &cwd,
    );
    screen.mode = permission.get();
    screen.transcript.push_rule(format!(
        "kazi · {} · {}",
        screen.session_name,
        clip(&provider_desc, 60)
    ));
    if let Some(hint) = agent.auth_hint() {
        screen.push_notice(Level::Warn, hint.to_string());
    }
    screen.push_notice(
        Level::Info,
        "type a task and press Enter · / for commands · Ctrl+K for the palette · Esc quits",
    );
    replay_history(&mut screen, &agent);

    let res = tui_inner(
        &mut terminal,
        &mut screen,
        agent,
        &on_session_save,
        &mut approvals,
        &permission,
        sess,
        &ask_tx,
        &session_allows,
        &mut background,
        &bg_tx,
    )
    .await;

    disable_raw_mode()?;
    execute!(
        std::io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    std::io::stdout().flush().ok();

    let quit_clean = screen.quit;
    res.map(|_| if quit_clean { 0 } else { 1 })
}

/// Paint a resumed session's conversation into the transcript.
fn replay_history(screen: &mut Screen, agent: &Agent) {
    const TAIL: usize = 20;
    let msgs = &agent.history.msgs;
    if msgs.is_empty() {
        return;
    }
    let start = msgs.len().saturating_sub(TAIL);
    screen.transcript.push_rule(format!(
        "replaying the last {} messages",
        msgs.len() - start
    ));
    for m in &msgs[start..] {
        match m.role {
            crate::llm::Role::User => screen.transcript.push_user(first_lines(m.text(), 30)),
            crate::llm::Role::Assistant => {
                let text = m.text().trim();
                if !text.is_empty() {
                    screen.transcript.push_assistant(first_lines(text, 60));
                }
                for tc in &m.tool_calls {
                    screen.transcript.push_notice(
                        Level::Info,
                        format!("→ {} {}", tc.name, crate::tools::call_summary(tc)),
                    );
                }
            }
            crate::llm::Role::Tool => screen.transcript.push_notice(
                if m.is_error {
                    Level::Error
                } else {
                    Level::Info
                },
                format!(
                    "{} {}",
                    if m.is_error { "tool failed:" } else { "tool:" },
                    first_lines(m.text(), 6)
                ),
            ),
            crate::llm::Role::System => {}
        }
    }
}

const BUSY_TEXT: &str = "the agent is busy — try again when the task finishes";

fn mode_note(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::ReadOnly => "inspect only",
        PermissionMode::Edit => "writes allowed, commands gated",
        PermissionMode::Yolo => "no prompts",
    }
}

// ---------------------------------------------------------------------------
// slash commands
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_command(
    cmdline: &str,
    screen: &mut Screen,
    slot: &Arc<tokio::sync::Mutex<Option<Agent>>>,
    sess: &mut SessionState,
    ask_tx: &tokio::sync::mpsc::UnboundedSender<ApprovalAsk>,
    session_allows: &SessionAllows,
    permission: &crate::tools::Permission,
    bg_tx: &tokio::sync::mpsc::UnboundedSender<BackgroundEvent>,
) -> Option<String> {
    use super::console;
    let verb = cmdline.split_once(' ').map(|(a, _)| a).unwrap_or(cmdline);
    let arg = cmdline.split_once(' ').map(|(_, b)| b.trim()).unwrap_or("");
    let verb_key = verb.to_ascii_lowercase();

    if let Some(usage) = console::no_arg_violation(&verb_key, arg) {
        screen.show_text(usage);
        return None;
    }

    match verb_key.as_str() {
        "" => {}
        "help" => {
            screen.overlay = Some(Overlay::Help(Help::default()));
        }
        "theme" => {
            let next = if arg.is_empty() {
                let name = screen.theme.name.cycle();
                Some(name)
            } else {
                ThemeName::parse(arg)
            };
            match next {
                Some(name) => {
                    screen.theme = theme_named(name);
                    screen.push_notice(
                        Level::Info,
                        format!("theme -> {}", screen.theme.name.as_str()),
                    );
                }
                None => screen.show_text("usage: /theme [dark|light|mono]"),
            }
        }
        "details" | "verbose" => {
            let next = match arg.to_ascii_lowercase().as_str() {
                "" => !screen.transcript.details(),
                "on" | "true" | "1" => true,
                "off" | "false" | "0" => false,
                _ => {
                    screen.show_text("usage: /details [on|off]");
                    return None;
                }
            };
            screen.transcript.set_details(next);
            screen.push_notice(
                Level::Info,
                format!("tool detail -> {}", if next { "full" } else { "collapsed" }),
            );
        }
        "exit" | "quit" => screen.quit = true,
        "new" | "clear" => {
            let topic = if verb_key == "new" && !arg.is_empty() {
                Some(arg)
            } else {
                None
            };
            let mut guard = slot.lock().await;
            let Some(a) = guard.as_mut() else {
                drop(guard);
                screen.show_text(BUSY_TEXT);
                return None;
            };
            let (name, note) = console::start_new_session(a, topic);
            screen.est_tokens = a.history.estimate_tokens();
            sess.session_name = name.clone();
            screen.session_name = name;
            drop(guard);
            screen.transcript.clear();
            screen.transcript.push_rule(format!("session {note}"));
            screen.scroll_to_bottom();
        }
        "compact" => {
            let force = match arg {
                "" => false,
                "force" | "--force" | "f" => true,
                _ => {
                    screen.show_text(console::usage_for("/compact"));
                    return None;
                }
            };
            let Some(agent) = slot.lock().await.take() else {
                screen.show_text(BUSY_TEXT);
                return None;
            };
            screen.busy = true;
            screen.started.set(Some(std::time::Instant::now()));
            screen.push_notice(
                Level::Info,
                format!("compacting{} …", if force { " (force)" } else { "" }),
            );
            let tx = bg_tx.clone();
            tokio::spawn(async move {
                let mut agent = agent;
                let note = console::compact_text(&mut agent, force).await;
                let _ = tx.send(BackgroundEvent::Compacted {
                    agent: Box::new(agent),
                    note,
                });
            });
        }
        "resume" | "sessions" => {
            if arg.is_empty() {
                screen.overlay = Some(Overlay::Sessions(Sessions::load()));
            } else {
                resume_session(screen, slot, sess, arg).await;
            }
        }
        "provider" => {
            if arg.is_empty() {
                let nicks = sess.cfg.nicks();
                if nicks.is_empty() {
                    screen.show_text(console::providers_text(&sess.cfg, None));
                } else {
                    let current = sess
                        .provider
                        .clone()
                        .or_else(|| sess.cfg.resolve(None, None).ok().map(|r| r.nick));
                    screen.overlay = Some(Overlay::Picker(Picker::new(
                        PickerKind::Provider,
                        "select provider",
                        nicks,
                        current.as_deref(),
                    )));
                }
            } else {
                start_switch(
                    screen,
                    slot,
                    sess,
                    ask_tx,
                    session_allows,
                    permission,
                    Some(arg.to_string()),
                    None,
                    false,
                    bg_tx,
                )
                .await;
            }
        }
        "model" => {
            if arg.is_empty() {
                let current_nick = sess
                    .provider
                    .clone()
                    .or_else(|| sess.cfg.resolve(None, None).ok().map(|r| r.nick));
                let current_model = sess.model.clone();
                let rows = console::model_choices_static(
                    &sess.cfg,
                    current_nick.as_deref(),
                    current_model.as_deref(),
                );
                let current = match (&current_nick, &current_model) {
                    (Some(n), Some(m)) => Some(format!("{n}/{m}")),
                    _ => None,
                };
                let mut picker = Picker::new(
                    PickerKind::Model,
                    "select model (provider/model)",
                    rows,
                    current.as_deref(),
                );
                picker.loading = true;
                screen.overlay = Some(Overlay::Picker(picker));
                // live candidates arrive later and merge into the open list
                let cfg = sess.cfg.clone();
                let tx = bg_tx.clone();
                tokio::spawn(async move {
                    let rows = console::all_model_choices(
                        &cfg,
                        current_nick.as_deref(),
                        current_model.as_deref(),
                    )
                    .await;
                    let _ = tx.send(BackgroundEvent::Choices {
                        rows,
                        current: None,
                    });
                });
            } else {
                let (provider, model) =
                    split_model_choice(&sess.cfg, sess.provider.as_deref(), arg);
                start_switch(
                    screen,
                    slot,
                    sess,
                    ask_tx,
                    session_allows,
                    permission,
                    provider,
                    Some(model),
                    true,
                    bg_tx,
                )
                .await;
            }
        }
        "yolo" => {
            let on = match arg.to_ascii_lowercase().as_str() {
                "" => permission.get() != PermissionMode::Yolo,
                "on" | "true" | "1" => true,
                "off" | "false" | "0" => false,
                _ => {
                    screen.show_text("usage: /yolo [on|off]");
                    return None;
                }
            };
            let next = if on {
                PermissionMode::Yolo
            } else {
                PermissionMode::Edit
            };
            permission.set(next);
            screen.mode = next;
            screen.push_notice(
                Level::Warn,
                format!("permission mode -> {} ({})", next.as_str(), mode_note(next)),
            );
        }
        "permission" => {
            if arg.is_empty() {
                screen.push_notice(
                    Level::Info,
                    format!(
                        "permission mode: {} ({}) · Shift+Tab cycles",
                        permission.get().as_str(),
                        mode_note(permission.get())
                    ),
                );
            } else if let Some(mode) = PermissionMode::parse(arg) {
                permission.set(mode);
                screen.mode = mode;
                screen.push_notice(
                    Level::Warn,
                    format!("permission mode -> {} ({})", mode.as_str(), mode_note(mode)),
                );
            } else {
                screen
                    .show_text("usage: /permission [read-only|edit|yolo] — or Shift+Tab to cycle");
            }
        }
        "undo" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => console::undo_text(a),
                None => BUSY_TEXT.to_string(),
            };
            screen.show_text(text);
        }
        "context" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => console::context_report(a),
                None => BUSY_TEXT.to_string(),
            };
            screen.show_text(text);
        }
        "status" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => format!(
                    "{}\n{}\nrounds: {} · ctx ≈ {} tokens · budget {}",
                    console::status_text(a),
                    console::context_report(a),
                    screen.rounds,
                    screen.est_tokens,
                    screen.max_context
                ),
                None => BUSY_TEXT.to_string(),
            };
            screen.show_text(text);
        }
        "auth" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => console::auth_report(a, &sess.cfg),
                None => BUSY_TEXT.to_string(),
            };
            screen.show_text(text);
        }
        "last" => {
            let Some(back) = console::parse_back(arg) else {
                screen.show_text(console::usage_for("/last"));
                return None;
            };
            let text = match slot.lock().await.as_ref() {
                Some(a) => console::last_tool_text(a, back),
                None => BUSY_TEXT.to_string(),
            };
            screen.show_text(text);
        }
        "tools" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => a.registry().catalog(),
                None => BUSY_TEXT.to_string(),
            };
            screen.show_text(text);
        }
        "export" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => export_session(a, sess, arg),
                None => BUSY_TEXT.to_string(),
            };
            screen.show_text(text);
        }
        "copy" => {
            let text = if arg.eq_ignore_ascii_case("all") {
                screen.transcript.plain_text()
            } else {
                screen.transcript.last_assistant().unwrap_or("").to_string()
            };
            if text.trim().is_empty() {
                screen.push_notice(Level::Warn, "nothing to copy yet");
            } else {
                match copy_to_clipboard(&text) {
                    Ok(how) => screen.push_notice(
                        Level::Success,
                        format!("copied {} chars ({how})", text.chars().count()),
                    ),
                    Err(e) => screen.push_notice(Level::Error, format!("copy failed: {e}")),
                }
            }
        }
        "retry" => {
            return screen.last_task.clone();
        }
        other => {
            screen.show_text(console::unknown_command_text(other));
        }
    }
    // a command's output is what the user asked for: show it
    screen.scroll_to_bottom();
    None
}

/// Apply a completed picker choice. Model rows are `nick/model`, so one
/// selection can switch provider AND model together.
#[allow(clippy::too_many_arguments)]
async fn apply_selection(
    screen: &mut Screen,
    slot: &Arc<tokio::sync::Mutex<Option<Agent>>>,
    sess: &mut SessionState,
    ask_tx: &tokio::sync::mpsc::UnboundedSender<ApprovalAsk>,
    session_allows: &SessionAllows,
    permission: &crate::tools::Permission,
    bg_tx: &tokio::sync::mpsc::UnboundedSender<BackgroundEvent>,
    kind: PickerKind,
    chosen: String,
) {
    match kind {
        PickerKind::Provider => {
            start_switch(
                screen,
                slot,
                sess,
                ask_tx,
                session_allows,
                permission,
                Some(chosen),
                None,
                false,
                bg_tx,
            )
            .await;
        }
        PickerKind::Model => {
            let (provider, model) =
                split_model_choice(&sess.cfg, sess.provider.as_deref(), &chosen);
            start_switch(
                screen,
                slot,
                sess,
                ask_tx,
                session_allows,
                permission,
                provider,
                Some(model),
                true,
                bg_tx,
            )
            .await;
        }
    }
}

/// Start a provider/model switch in the background; the rebuilt agent arrives
/// over `bg_tx` with the conversation carried along.
#[allow(clippy::too_many_arguments)]
async fn start_switch(
    screen: &mut Screen,
    slot: &Arc<tokio::sync::Mutex<Option<Agent>>>,
    sess: &SessionState,
    ask_tx: &tokio::sync::mpsc::UnboundedSender<ApprovalAsk>,
    session_allows: &SessionAllows,
    permission: &crate::tools::Permission,
    provider: Option<String>,
    model: Option<String>,
    probe: bool,
    bg_tx: &tokio::sync::mpsc::UnboundedSender<BackgroundEvent>,
) {
    let Some(old) = slot.lock().await.take() else {
        screen.show_text(BUSY_TEXT);
        return;
    };
    let label = match (&provider, &model) {
        (Some(p), Some(m)) => format!("{p} / {m}"),
        (Some(p), None) => p.clone(),
        (None, Some(m)) => m.clone(),
        (None, None) => "the current provider".to_string(),
    };
    screen.busy = true;
    screen.started.set(Some(std::time::Instant::now()));
    screen.push_notice(Level::Info, format!("switching -> {label} …"));

    let cfg = sess.cfg.clone();
    let approver: Arc<dyn Approver> = Arc::new(TuiApprover::with_allows(
        ask_tx.clone(),
        session_allows.clone(),
    ));
    let mode = permission.get();
    let tx = bg_tx.clone();
    tokio::spawn(async move {
        let mut old = old;
        match old
            .rebuilt_with(&cfg, provider.as_deref(), model.as_deref(), approver, mode)
            .await
        {
            Ok(mut agent) => {
                if probe {
                    if let Err(e) = agent.probe().await {
                        let note = format!(
                            "model rejected by provider, staying on {}\n{e}",
                            old.provider().describe()
                        );
                        let _ = tx.send(BackgroundEvent::Failed {
                            agent: Box::new(old),
                            note,
                        });
                        return;
                    }
                }
                let desc = agent.provider().describe();
                agent.history =
                    std::mem::replace(&mut old.history, super::context::History::new(None));
                let _ = tx.send(BackgroundEvent::Switched {
                    agent: Box::new(agent),
                    provider,
                    model,
                    desc,
                });
            }
            Err(e) => {
                let _ = tx.send(BackgroundEvent::Failed {
                    agent: Box::new(old),
                    note: format!("switch failed: {e}"),
                });
            }
        }
    });
}

/// `/model <nick/model>` picks provider AND model in one step; a bare name (or
/// number) stays on the current provider.
fn split_model_choice(
    cfg: &crate::config::Config,
    current: Option<&str>,
    arg: &str,
) -> (Option<String>, String) {
    match arg.split_once('/') {
        Some((nick, model)) if cfg.providers.contains_key(nick) => {
            (Some(nick.to_string()), model.to_string())
        }
        _ => (current.map(str::to_string), arg.to_string()),
    }
}

async fn resume_session(
    screen: &mut Screen,
    slot: &Arc<tokio::sync::Mutex<Option<Agent>>>,
    sess: &mut SessionState,
    prefix: &str,
) {
    let mut guard = slot.lock().await;
    let Some(a) = guard.as_mut() else {
        drop(guard);
        screen.show_text(BUSY_TEXT);
        return;
    };
    let (name, text) = super::console::resume_text(a, prefix).await;
    if let Some(name) = name {
        sess.session_name = name.clone();
        screen.session_name = name;
    }
    screen.est_tokens = a.history.estimate_tokens();
    screen.transcript.clear();
    replay_history(screen, a);
    drop(guard);
    screen.show_text(text);
    screen.scroll_to_bottom();
}

/// Export the live conversation as markdown (same shape as `kazi export`).
fn export_session(agent: &Agent, sess: &SessionState, arg: &str) -> String {
    let file = super::session::SessionFile {
        version: 1,
        name: sess.session_name.clone(),
        system: agent.history.system.clone(),
        messages: agent.history.msgs.clone(),
        saved_at: super::session::now_secs(),
    };
    let md = super::session::export_markdown(&file);
    let path = if arg.is_empty() {
        std::path::PathBuf::from(format!("{}.md", sess.session_name))
    } else {
        std::path::PathBuf::from(arg)
    };
    match std::fs::write(&path, md) {
        Ok(()) => format!("exported session to {}", path.display()),
        Err(e) => format!("export failed: {e}"),
    }
}

/// Copy text to the system clipboard, trying the platform's tool. Returns how
/// it succeeded, or an error listing what was tried.
fn copy_to_clipboard(text: &str) -> Result<String, String> {
    use std::process::{Command, Stdio};
    let mut candidates: Vec<(&str, Vec<&str>)> = Vec::new();
    if cfg!(windows) {
        candidates.push(("clip", vec![]));
    } else if cfg!(target_os = "macos") {
        candidates.push(("pbcopy", vec![]));
    } else {
        candidates.push(("wl-copy", vec![]));
        candidates.push(("xclip", vec!["-selection", "clipboard"]));
        candidates.push(("xsel", vec!["--clipboard", "--input"]));
    }
    let mut tried = Vec::new();
    for (cmd, args) in candidates {
        tried.push(cmd.to_string());
        let child = Command::new(cmd)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { continue };
        if let Some(stdin) = child.stdin.as_mut() {
            if stdin.write_all(text.as_bytes()).is_err() {
                continue;
            }
        }
        drop(child.stdin.take());
        match child.wait() {
            Ok(status) if status.success() => return Ok(cmd.to_string()),
            _ => continue,
        }
    }
    Err(format!(
        "no clipboard tool worked (tried: {}). Use the terminal's own selection (Shift+drag).",
        tried.join(", ")
    ))
}

// ---------------------------------------------------------------------------
// main loop
// ---------------------------------------------------------------------------

/// Inner loop: agent tasks run in a spawned tokio task and stream their events
/// through an mpsc channel; the UI loop `select!`s between terminal input and
/// incoming events, so the screen keeps redrawing (streaming deltas land live)
/// and Esc stays responsive even while a task runs.
#[allow(clippy::too_many_arguments)]
async fn tui_inner(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    screen: &mut Screen,
    agent: Agent,
    on_session_save: &(impl Fn(&str, &Agent) + ?Sized),
    approvals: &mut tokio::sync::mpsc::UnboundedReceiver<ApprovalAsk>,
    permission: &crate::tools::Permission,
    sess: &mut SessionState,
    ask_tx: &tokio::sync::mpsc::UnboundedSender<ApprovalAsk>,
    session_allows: &SessionAllows,
    background: &mut tokio::sync::mpsc::UnboundedReceiver<BackgroundEvent>,
    bg_tx: &tokio::sync::mpsc::UnboundedSender<BackgroundEvent>,
) -> anyhow::Result<Option<Agent>> {
    let shared_agent: Arc<tokio::sync::Mutex<Option<Agent>>> =
        Arc::new(tokio::sync::Mutex::new(Some(agent)));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Pump>();
    let mut runner: Option<tokio::task::JoinHandle<()>> = None;
    let mut current_cancel: Option<Arc<AtomicBool>> = None;

    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(200));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    while !screen.quit {
        let _ = terminal.draw(|f| screen.draw(f));

        tokio::select! {
            _ = ticker.tick(), if screen.busy => {
                screen.spinner.set(screen.spinner.get().wrapping_add(1));
            }

            Some(done) = background.recv() => {
                match done {
                    BackgroundEvent::Switched { agent, provider, model, desc } => {
                        screen.busy = false;
                        screen.started.set(None);
                        *shared_agent.lock().await = Some(*agent);
                        sess.provider = provider;
                        sess.model = model;
                        screen.provider_desc = desc.clone();
                        screen.push_notice(Level::Success, format!("switched -> {desc}"));
                    }
                    BackgroundEvent::Failed { agent, note } => {
                        screen.busy = false;
                        screen.started.set(None);
                        *shared_agent.lock().await = Some(*agent);
                        screen.push_notice(Level::Error, note);
                    }
                    BackgroundEvent::Compacted { agent, note } => {
                        screen.busy = false;
                        screen.started.set(None);
                        screen.est_tokens = agent.history.estimate_tokens();
                        *shared_agent.lock().await = Some(*agent);
                        screen.push_notice(Level::Info, note);
                    }
                    BackgroundEvent::Choices { rows, current } => {
                        if let Some(Overlay::Picker(p)) = screen.overlay.as_mut() {
                            if p.kind == PickerKind::Model {
                                p.loading = false;
                                let added = p.merge(rows, current);
                                if added > 0 {
                                    screen.push_notice(Level::Info, format!("+{added} live model(s) merged in"));
                                }
                            }
                        }
                    }
                }
            }

            ask = approvals.recv() => {
                if let Some(ask) = ask {
                    screen.pending_approval = Some(PendingApproval {
                        ask,
                        reason_mode: false,
                        reason: String::new(),
                    });
                }
            }

            maybe = rx.recv(), if runner.is_some() => match maybe {
                Some(Pump::Ev(ev)) => screen.consume(ev),
                Some(Pump::Done(res)) => {
                    screen.flush_streaming();
                    if res.is_err() {
                        screen
                            .transcript
                            .abandon_running("(interrupted before the tool finished)");
                    }
                    match res {
                        Err(super::AgentError::Cancelled) => {
                            screen.push_notice(Level::Info, "(cancelled)");
                        }
                        Err(e) => {
                            let text = shared_agent
                                .lock()
                                .await
                                .as_ref()
                                .map(|a| a.error_text(&e))
                                .unwrap_or_else(|| e.to_string());
                            screen.push_notice(Level::Error, format!("error: {text}"));
                        }
                        Ok(_) => {}
                    }
                    screen.busy = false;
                    screen.started.set(None);
                    runner = None;
                    current_cancel = None;
                    if let Some(agent) = shared_agent.lock().await.as_ref() {
                        on_session_save(&sess.session_name, agent);
                    }
                    // queued follow-ups: run the next one immediately
                    if let Some(next) = screen.queued.pop_front() {
                        match launch_task(screen, &shared_agent, &tx, next).await {
                            Some((handle, cancel)) => {
                                runner = Some(handle);
                                current_cancel = Some(cancel);
                            }
                            None => screen.busy = false,
                        }
                    }
                }
                None => { screen.busy = false; runner = None; current_cancel = None; }
            },

            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(CtEvent::Key(key))) if key.kind != KeyEventKind::Release => {
                    log_key_event(&key);

                    // Ctrl+C always means "get me out" — even mid-approval or
                    // with a modal open (the agent is cancelled on the way out).
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        screen.quit = true;
                        continue;
                    }

                    // 1) the approval prompt is modal
                    if screen.pending_approval.is_some() {
                        handle_approval(screen, &key);
                        continue;
                    }

                    // 2) an open overlay owns the keyboard
                    if screen.overlay.is_some() {
                        let mut close = false;
                        let mut accept: Option<(PickerKind, String)> = None;
                        let mut jump: Option<usize> = None;
                        let mut resume: Option<String> = None;
                        let mut notices: Vec<(Level, String)> = Vec::new();
                        {
                            let ov = screen.overlay.as_mut().expect("checked");
                            match ov {
                                Overlay::Palette(p) => match key.code {
                                    KeyCode::Up => p.move_cursor(-1),
                                    KeyCode::Down => p.move_cursor(1),
                                    KeyCode::Esc => close = true,
                                    KeyCode::Enter => {
                                        if let Some(cmd) = p.selected_command() {
                                            screen.input.set_text(format!("{cmd} "));
                                            close = true;
                                        }
                                    }
                                    KeyCode::Backspace => p.pop(),
                                    code => {
                                        if let Some(ch) = accepted_char(&code, key.modifiers) {
                                            p.push(ch);
                                        }
                                    }
                                },
                                Overlay::Picker(p) => match key.code {
                                    KeyCode::Up => p.move_cursor(-1),
                                    KeyCode::Down => p.move_cursor(1),
                                    KeyCode::Esc => close = true,
                                    KeyCode::Backspace => p.pop_filter(),
                                    KeyCode::Enter => {
                                        accept = p.selected().map(|s| (p.kind, s));
                                    }
                                    code => {
                                        if let Some(ch) = accepted_char(&code, key.modifiers) {
                                            p.push_filter(ch);
                                        }
                                    }
                                },
                                Overlay::Help(h) => match key.code {
                                    KeyCode::Esc | KeyCode::Char('q') => close = true,
                                    KeyCode::Up => h.scroll = h.scroll.saturating_sub(1),
                                    KeyCode::Down => h.scroll += 1,
                                    KeyCode::PageUp => h.scroll = h.scroll.saturating_sub(12),
                                    KeyCode::PageDown => h.scroll += 12,
                                    KeyCode::Home => h.scroll = 0,
                                    KeyCode::End => h.scroll = usize::MAX / 2,
                                    _ => {}
                                },
                                Overlay::Sessions(s) => {
                                    let mut delete: Option<String> = None;
                                    match key.code {
                                        KeyCode::Up => s.move_cursor(-1),
                                        KeyCode::Down => s.move_cursor(1),
                                        KeyCode::Esc => close = true,
                                        KeyCode::Backspace => s.pop(),
                                        KeyCode::Enter => {
                                            resume = s.selected().map(|x| x.name.clone());
                                            close = true;
                                        }
                                        KeyCode::Char('d') | KeyCode::Char('D') => {
                                            if s.confirm_delete == Some(s.cursor) {
                                                delete = s.selected().map(|x| x.name.clone());
                                            } else {
                                                s.confirm_delete = Some(s.cursor);
                                            }
                                        }
                                        code => {
                                            if let Some(ch) = accepted_char(&code, key.modifiers) {
                                                s.push(ch);
                                            }
                                        }
                                    }
                                    if let Some(name) = delete {
                                        let note = match super::session::delete(&name) {
                                            Ok(Some(n)) => format!("deleted session `{n}`"),
                                            Ok(None) => format!("no session matched `{name}`"),
                                            Err(e) => format!("delete failed: {e}"),
                                        };
                                        s.rows = super::session::list_summaries();
                                        s.confirm_delete = None;
                                        notices.push((Level::Warn, note));
                                    }
                                }
                                Overlay::Search(s) => {
                                    match key.code {
                                        KeyCode::Up => s.move_cursor(-1),
                                        KeyCode::Down => s.move_cursor(1),
                                        KeyCode::Esc => close = true,
                                        KeyCode::Backspace => s.pop(),
                                        KeyCode::Enter => {
                                            jump = s.hits.get(s.cursor).map(|h| h.entry);
                                            close = true;
                                        }
                                        code => {
                                            if let Some(ch) = accepted_char(&code, key.modifiers) {
                                                s.push(ch);
                                            }
                                        }
                                    }
                                    if !close {
                                        s.hits = screen.transcript.search(&s.query);
                                        if s.cursor >= s.hits.len() {
                                            s.cursor = s.hits.len().saturating_sub(1);
                                        }
                                    }
                                }
                            }
                        }
                        for (level, text) in notices {
                            screen.push_notice(level, text);
                        }
                        if close {
                            screen.overlay = None;
                        }
                        if let Some((kind, chosen)) = accept {
                            screen.overlay = None;
                            apply_selection(
                                screen,
                                &shared_agent,
                                sess,
                                ask_tx,
                                session_allows,
                                permission,
                                bg_tx,
                                kind,
                                chosen,
                            )
                            .await;
                        }
                        if let Some(entry) = jump {
                            screen.scroll_to_entry(entry);
                        }
                        if let Some(prefix) = resume {
                            if screen.busy {
                                screen.push_notice(Level::Warn, BUSY_TEXT);
                            } else {
                                resume_session(screen, &shared_agent, sess, &prefix).await;
                                screen.scroll_to_bottom();
                            }
                        }
                        continue;
                    }

                    // 3) the editor
                    match key.code {
                        KeyCode::Esc => {
                            if screen.busy {
                                if let Some(flag) = &current_cancel {
                                    flag.store(true, Ordering::Relaxed);
                                    screen.push_notice(Level::Info, "cancelling…");
                                }
                            } else if !screen.input.text.trim().is_empty() {
                                screen.input.clear();
                            } else {
                                screen.quit = true;
                            }
                        }
                        KeyCode::Char('c')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            screen.quit = true;
                        }
                        KeyCode::Char('k')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            screen.overlay = Some(Overlay::Palette(Palette::default()));
                        }
                        KeyCode::Char('f')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            screen.overlay = Some(Overlay::Search(Search::default()));
                        }
                        KeyCode::Char('r')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            screen.overlay = Some(Overlay::Sessions(Sessions::load()));
                        }
                        KeyCode::Char('t')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            let next = screen.theme.name.cycle();
                            screen.theme = theme_named(next);
                            screen.push_notice(
                                Level::Info,
                                format!("theme -> {}", next.as_str()),
                            );
                        }
                        KeyCode::Char('o')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            let on = screen.transcript.toggle_details();
                            screen.push_notice(
                                Level::Info,
                                format!("tool detail -> {}", if on { "full" } else { "collapsed" }),
                            );
                        }
                        KeyCode::Char('e')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            if let Some(idx) = screen.transcript.last_tool() {
                                screen.transcript.toggle_entry(idx);
                            }
                        }
                        KeyCode::Char('j')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            screen.input.newline();
                        }
                        KeyCode::Char('l')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            screen.transcript.clear();
                            screen.scroll_to_bottom();
                        }
                        KeyCode::BackTab => {
                            let next = permission.get().cycle();
                            permission.set(next);
                            screen.mode = next;
                            screen.push_notice(
                                Level::Warn,
                                format!(
                                    "permission mode -> {} ({})",
                                    next.as_str(),
                                    mode_note(next)
                                ),
                            );
                        }
                        KeyCode::Enter if !screen.busy => {
                            if let Some(complete) = screen.resolve_command_input() {
                                screen.input.set_text(complete);
                            }
                            if let Some(text) = screen.input.submit() {
                                super::console::remember_input(&text);
                                if let Some(cmdline) = text.strip_prefix('/') {
                                    let follow = run_command(
                                        cmdline.trim(),
                                        screen,
                                        &shared_agent,
                                        sess,
                                        ask_tx,
                                        session_allows,
                                        permission,
                                        bg_tx,
                                    )
                                    .await;
                                    if let Some(task) = follow {
                                        match launch_task(screen, &shared_agent, &tx, task).await {
                                            Some((handle, cancel)) => {
                                                runner = Some(handle);
                                                current_cancel = Some(cancel);
                                            }
                                            None => screen.busy = false,
                                        }
                                    }
                                } else {
                                    match launch_task(screen, &shared_agent, &tx, text).await {
                                        Some((handle, cancel)) => {
                                            runner = Some(handle);
                                            current_cancel = Some(cancel);
                                        }
                                        None => screen.busy = false,
                                    }
                                }
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(text) = screen.input.submit() {
                                super::console::remember_input(&text);
                                if text.starts_with('/') {
                                    screen.push_notice(
                                        Level::Warn,
                                        "finish or interrupt the current task before running a command",
                                    );
                                } else {
                                    screen.queued.push_back(text.clone());
                                    screen.transcript.push_notice(
                                        Level::Info,
                                        format!("queued: {}", first_lines(&text, 2)),
                                    );
                                }
                            }
                        }
                        KeyCode::Tab => {
                            let token = super::console::pending_command(&screen.input.text)
                                .map(|t| t.to_string());
                            if let Some(token) = token {
                                let hints = super::console::command_rows(&token);
                                if hints.len() == 1 {
                                    screen.input.set_text(format!("{token} "));
                                } else if let Some(done) = super::console::command_completion(&token)
                                {
                                    screen.input.set_text(done);
                                } else if hints.len() > 1 {
                                    screen.move_command_sel(1);
                                }
                            } else {
                                let pos = screen.input.byte_of(screen.input.cursor);
                                if let Some(done) =
                                    super::console::complete_at_path(&screen.input.text, pos)
                                {
                                    screen.input.set_text(done);
                                }
                            }
                        }
                        KeyCode::Backspace => screen.input.backspace(),
                        KeyCode::Delete => screen.input.delete(),
                        KeyCode::Left => screen.input.left(),
                        KeyCode::Right => screen.input.right(),
                        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            screen.scroll_to_top()
                        }
                        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            screen.scroll_to_bottom()
                        }
                        KeyCode::Home => {
                            if screen.input.line_count() > 1 {
                                screen.input.home_line()
                            } else {
                                screen.input.home()
                            }
                        }
                        KeyCode::End => {
                            if screen.input.line_count() > 1 {
                                screen.input.end_line()
                            } else {
                                screen.input.end()
                            }
                        }
                        KeyCode::Up => {
                            if screen.command_hints().len() > 1 {
                                screen.move_command_sel(-1);
                            } else if screen.input.line_count() > 1 {
                                screen.input.up();
                            } else {
                                screen.input.history_prev();
                            }
                        }
                        KeyCode::Down => {
                            if screen.command_hints().len() > 1 {
                                screen.move_command_sel(1);
                            } else if screen.input.line_count() > 1 {
                                screen.input.down();
                            } else {
                                screen.input.history_next();
                            }
                        }
                        KeyCode::PageUp => screen.scroll_up(10),
                        KeyCode::PageDown => screen.scroll_down(10),
                        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            match c {
                                'a' => screen.input.home(),
                                'e' => screen.input.end(),
                                'u' => screen.input.kill_to_start(),
                                'k' => screen.input.kill_to_end(),
                                'w' => screen.input.delete_word(),
                                _ => {}
                            }
                        }
                        code => {
                            if let Some(ch) = accepted_char(&code, key.modifiers) {
                                screen.input.insert(ch);
                            }
                        }
                    }
                }
                Some(Ok(CtEvent::Mouse(me))) => match me.kind {
                    MouseEventKind::ScrollUp => screen.scroll_up(3),
                    MouseEventKind::ScrollDown => screen.scroll_down(3),
                    MouseEventKind::Down(MouseButton::Left) => {
                        let area = screen.body_area.get();
                        if area.width > 0
                            && me.column >= area.x
                            && me.column < area.right()
                            && me.row >= area.y
                            && me.row < area.bottom()
                        {
                            let row = (me.row - area.y) as usize;
                            let owners = screen.hit_owner.borrow();
                            if let Some(idx) = owners.get(row).copied() {
                                if idx != usize::MAX {
                                    screen.transcript.toggle_entry(idx);
                                }
                            }
                        }
                    }
                    _ => {}
                },
                Some(Ok(CtEvent::Paste(text))) => {
                    if screen.pending_approval.is_none() && screen.overlay.is_none() {
                        screen.input.insert_str(&text);
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => {}
            },
        }
    }

    if let Some(flag) = &current_cancel {
        flag.store(true, Ordering::Relaxed);
    }
    if let Some(handle) = runner.take() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }
    let returned = shared_agent.lock().await.take();
    if let Some(agent) = &returned {
        on_session_save(&sess.session_name, agent);
    }
    Ok(returned)
}

/// Answer the modal approval prompt (y/a/n/r/Esc).
fn handle_approval(screen: &mut Screen, key: &ratatui::crossterm::event::KeyEvent) {
    let mut decision: Option<Decision> = None;
    {
        let Some(p) = screen.pending_approval.as_mut() else {
            return;
        };
        if p.reason_mode {
            match key.code {
                KeyCode::Enter => {
                    let reason = std::mem::take(&mut p.reason);
                    decision = Some(Decision::Deny(Some(reason)));
                }
                KeyCode::Esc => {
                    p.reason_mode = false;
                    p.reason.clear();
                }
                KeyCode::Backspace => {
                    p.reason.pop();
                }
                code => {
                    if let Some(ch) = accepted_char(&code, key.modifiers) {
                        p.reason.push(ch);
                    }
                }
            }
        } else {
            decision = match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(Decision::Allow),
                KeyCode::Char('a') | KeyCode::Char('A') => Some(Decision::AllowSession),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    Some(Decision::Deny(None))
                }
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    p.reason_mode = true;
                    p.reason.clear();
                    None
                }
                _ => None,
            };
        }
    }
    if let Some(decision) = decision {
        let Some(p) = screen.pending_approval.take() else {
            return;
        };
        let verdict = match &decision {
            Decision::Allow => "allowed",
            Decision::AllowSession => "allowed (always, this command)",
            Decision::Deny(None) => "denied",
            Decision::Deny(Some(_)) => "denied with a reason",
        };
        let _ = p.ask.reply.send(decision);
        screen.push_notice(
            if verdict.starts_with("denied") {
                Level::Warn
            } else {
                Level::Info
            },
            format!("approval: {verbatim}", verbatim = verdict),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn test_screen() -> Screen {
        Screen::new(
            "stub-provider",
            "sess",
            5,
            60_000,
            Theme::default(),
            false,
            "D:/tmp",
        )
    }

    fn frame_rows(term: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = term.backend().buffer();
        let mut out = Vec::new();
        for y in 0..buffer.area.height {
            let mut row = String::new();
            for x in 0..buffer.area.width {
                row.push_str(buffer[(x, y)].symbol());
            }
            out.push(row);
        }
        out
    }

    fn screen_text(screen: &Screen) -> String {
        screen.transcript.plain_text()
    }

    #[test]
    fn input_editing_moves_the_caret() {
        let mut i = Input::default();
        i.insert_str("hello world");
        assert_eq!(i.cursor, 11);
        i.left();
        i.left();
        i.backspace();
        assert_eq!(i.text, "hello wold");
        i.home();
        i.delete();
        assert_eq!(i.text, "ello wold");
        i.end();
        i.kill_to_start();
        assert_eq!(i.text, "");
        assert_eq!(i.cursor, 0);
    }

    #[test]
    fn input_history_roundtrip_keeps_the_draft() {
        let mut i = Input::default();
        i.insert_str("first");
        assert_eq!(i.submit().as_deref(), Some("first"));
        i.insert_str("draft");
        i.history_prev();
        assert_eq!(i.text, "first");
        i.history_prev(); // already at the oldest
        assert_eq!(i.text, "first");
        i.history_next();
        assert_eq!(i.text, "draft");
    }

    #[test]
    fn multiline_input_navigates_and_counts_lines() {
        let mut i = Input::default();
        i.insert_str("one\ntwo\nthree");
        assert_eq!(i.line_count(), 3);
        let (rows, caret_row, caret_col) = i.visual_rows(40);
        assert_eq!(rows, vec!["one", "two", "three"]);
        assert_eq!((caret_row, caret_col), (2, 5));
        i.up();
        let (_, caret_row, caret_col) = i.visual_rows(40);
        assert_eq!((caret_row, caret_col), (1, 3));
        i.home_line();
        let (_, _, caret_col) = i.visual_rows(40);
        assert_eq!(caret_col, 0);
        i.end_line();
        let (_, _, caret_col) = i.visual_rows(40);
        assert_eq!(caret_col, 3);
        i.submit();
        assert_eq!(i.line_count(), 1);
    }

    #[test]
    fn long_input_soft_wraps_and_keeps_the_caret_visible() {
        let mut i = Input::default();
        i.insert_str(&"word ".repeat(30));
        let (rows, caret_row, caret_col) = i.visual_rows(20);
        assert!(rows.len() > 5, "rows: {rows:?}");
        for row in &rows {
            assert!(UnicodeWidthStr::width(row.as_str()) <= 20, "{row:?}");
        }
        assert_eq!(caret_row, rows.len() - 1);
        assert!(caret_col <= 20);

        // CJK counts by display width, not char count
        let mut cjk = Input::default();
        cjk.insert_str(&"中文字符".repeat(20));
        let (rows, caret_row, _) = cjk.visual_rows(20);
        assert!(rows.len() > 1);
        for row in &rows {
            assert!(UnicodeWidthStr::width(row.as_str()) <= 20, "{row:?}");
        }
        assert_eq!(caret_row, rows.len() - 1);
    }

    #[test]
    fn modifier_combos_are_not_text() {
        assert_eq!(
            accepted_char(&KeyCode::Char('a'), KeyModifiers::CONTROL),
            None
        );
        assert_eq!(
            accepted_char(
                &KeyCode::Char('@'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ),
            Some('@'),
            "AltGr must keep typing"
        );
        assert_eq!(accepted_char(&KeyCode::Enter, KeyModifiers::NONE), None);
    }

    #[test]
    fn paste_keeps_line_breaks() {
        let mut i = Input::default();
        i.insert_str("a\r\nb\rc");
        assert_eq!(i.text, "a\nb\nc");
    }

    #[test]
    fn events_build_markdown_and_tool_cards() {
        let mut s = test_screen();
        s.consume(AgentEvent::TextDelta {
            text: "looking **now**".into(),
        });
        s.consume(AgentEvent::ToolStart {
            id: "t1".into(),
            name: "read_file".into(),
            summary: "src/main.rs".into(),
        });
        s.consume(AgentEvent::ToolEnd {
            id: "t1".into(),
            name: "read_file".into(),
            is_error: false,
            preview: "1: fn main() {}".into(),
            output: "1: fn main() {}".into(),
            diff: None,
            elapsed_ms: 30,
        });
        s.consume(AgentEvent::TodosUpdated {
            items: vec![("do it".into(), "in_progress".into())],
        });
        let text = screen_text(&s);
        assert!(text.contains("looking **now**"));
        assert!(text.contains("read_file"));
        assert_eq!(s.todos.len(), 1);
        assert!(!s.busy);
    }

    #[test]
    fn header_shows_mode_provider_session_and_context() {
        let mut s = test_screen();
        s.mode = PermissionMode::ReadOnly;
        s.est_tokens = 30_000;
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let rows = frame_rows(&term);
        let status = &rows[0];
        assert!(status.contains("kazi"), "{status}");
        assert!(status.contains("[read-only]"), "{status}");
        assert!(status.contains("stub-provider"), "{status}");
        assert!(status.contains("sess"), "{status}");
        assert!(status.contains("50%"), "gauge percentage: {status}");
    }

    #[test]
    fn busy_header_shows_spinner_and_elapsed() {
        let mut s = test_screen();
        s.busy = true;
        s.started.set(Some(std::time::Instant::now()));
        let mut term = Terminal::new(TestBackend::new(100, 12)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let status = frame_rows(&term)[0].clone();
        assert!(status.contains("0s"), "{status}");
        assert!(status.contains("r0/5"), "{status}");
    }

    #[test]
    fn input_box_renders_every_line() {
        let mut s = test_screen();
        s.input.insert_str("alpha\nbeta");
        let mut term = Terminal::new(TestBackend::new(60, 14)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let rows = frame_rows(&term);
        assert!(rows.iter().any(|r| r.contains("❯ alpha")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains(" beta")), "{rows:#?}");
    }

    #[test]
    fn command_dropdown_renders_above_the_input() {
        let mut s = test_screen();
        s.input.insert_str("/co");
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let rows = frame_rows(&term);
        assert!(rows.iter().any(|r| r.contains("/compact")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("/context")), "{rows:#?}");
    }

    #[test]
    fn overlays_draw_and_close() {
        let mut s = test_screen();
        s.overlay = Some(Overlay::Palette(Palette::default()));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let rows = frame_rows(&term).join("\n");
        assert!(rows.contains("commands"), "palette drawn:\n{rows}");
        assert!(rows.contains("/compact"));

        s.overlay = Some(Overlay::Help(Help::default()));
        term.draw(|f| s.draw(f)).unwrap();
        let rows = frame_rows(&term).join("\n");
        assert!(
            rows.contains("key bindings") || rows.contains("Ctrl+F"),
            "{rows}"
        );
    }

    #[test]
    fn scrolled_view_stays_put_while_output_grows() {
        let mut s = test_screen();
        for i in 0..60 {
            s.push_notice(Level::Info, format!("line {i}"));
        }
        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let first = |term: &Terminal<TestBackend>| {
            frame_rows(term)
                .into_iter()
                .find(|r| r.contains("line "))
                .unwrap_or_default()
        };
        s.scroll_up(5);
        term.draw(|f| s.draw(f)).unwrap();
        let before = first(&term);
        assert!(!before.is_empty());
        s.push_notice(Level::Info, "line 60");
        term.draw(|f| s.draw(f)).unwrap();
        assert_eq!(first(&term), before, "the viewport must not slide");
    }

    #[test]
    fn click_toggles_the_tool_card_under_the_cursor() {
        let mut s = test_screen();
        s.consume(AgentEvent::ToolStart {
            id: "a".into(),
            name: "run_command".into(),
            summary: "cargo test".into(),
        });
        let output: String = (1..=20).map(|i| format!("row {i}\n")).collect();
        s.consume(AgentEvent::ToolEnd {
            id: "a".into(),
            name: "run_command".into(),
            is_error: false,
            preview: "row 1".into(),
            output,
            diff: None,
            elapsed_ms: 10,
        });
        let mut term = Terminal::new(TestBackend::new(70, 20)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let owners = s.hit_owner.borrow().clone();
        let row = owners.iter().position(|o| *o == 0).expect("tool card row");
        // simulate the click dispatch exactly like the event loop does
        let idx = owners.get(row).copied().unwrap();
        assert_eq!(s.transcript.toggle_entry(idx), Some(true));
        term.draw(|f| s.draw(f)).unwrap();
        let text = frame_rows(&term).join("\n");
        assert!(
            text.contains("row 20"),
            "expanded card shows all rows:\n{text}"
        );
    }

    #[test]
    fn key_help_and_commands_stay_in_sync() {
        // every key binding in the help overlay is non-empty
        for (key, what) in overlay::KEY_BINDINGS {
            assert!(!key.is_empty() && !what.is_empty());
        }
        let help = overlay::help_lines(Theme::default());
        let text = help
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("/model"));
        assert!(text.contains("Ctrl+K"));
    }

    #[test]
    fn draw_never_panics_at_small_or_odd_sizes() {
        for (w, h) in [
            (12u16, 4u16),
            (12, 5),
            (16, 6),
            (20, 6),
            (30, 8),
            (44, 10),
            (80, 24),
            (140, 42),
        ] {
            let mut s = test_screen();
            s.input
                .insert_str("a long first line that overflows\nsecond line");
            s.todos = vec![
                ("one".into(), "completed".into()),
                ("two".into(), "in_progress".into()),
                ("three".into(), "pending".into()),
            ];
            s.busy = true;
            s.started.set(Some(std::time::Instant::now()));
            s.est_tokens = 59_000;
            s.push_notice(Level::Warn, "a warning");
            s.consume(AgentEvent::ToolStart {
                id: "z".into(),
                name: "run_command".into(),
                summary: "cargo test".into(),
            });
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| s.draw(f)).unwrap();
            for overlay in [
                Overlay::Palette(Palette::default()),
                Overlay::Help(Help::default()),
                Overlay::Picker(Picker::new(
                    PickerKind::Model,
                    "models",
                    vec!["a/b".into(), "c/d".into()],
                    None,
                )),
                Overlay::Sessions(Sessions::default()),
                Overlay::Search(Search::default()),
            ] {
                s.overlay = Some(overlay);
                term.draw(|f| s.draw(f)).unwrap();
            }
            s.overlay = None;
            let (tx, _rx) = tokio::sync::oneshot::channel();
            s.pending_approval = Some(PendingApproval {
                ask: ApprovalAsk {
                    tool: "run_command".into(),
                    summary: "$ git push --force\n  cwd: /tmp\nsecond".into(),
                    reply: tx,
                },
                reason_mode: false,
                reason: String::new(),
            });
            term.draw(|f| s.draw(f)).unwrap();
            s.pending_approval = None;
            s.scroll_to_top();
            term.draw(|f| s.draw(f)).unwrap();
        }
    }

    #[test]
    fn approval_keys_answer_the_agent() {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let mut s = test_screen();
        s.pending_approval = Some(PendingApproval {
            ask: ApprovalAsk {
                tool: "run_command".into(),
                summary: "$ rm -rf build".into(),
                reply: tx,
            },
            reason_mode: false,
            reason: String::new(),
        });
        let key = |code| ratatui::crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        // `r` opens the reason line; typing + Enter denies with the reason
        handle_approval(&mut s, &key(KeyCode::Char('r')));
        assert!(s.pending_approval.as_ref().unwrap().reason_mode);
        for c in "use --dry-run".chars() {
            handle_approval(&mut s, &key(KeyCode::Char(c)));
        }
        handle_approval(&mut s, &key(KeyCode::Enter));
        assert!(s.pending_approval.is_none());
        assert_eq!(
            rx.try_recv().unwrap(),
            Decision::Deny(Some("use --dry-run".into()))
        );

        // plain `y` allows once
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.pending_approval = Some(PendingApproval {
            ask: ApprovalAsk {
                tool: "run_command".into(),
                summary: "$ ls".into(),
                reply: tx,
            },
            reason_mode: false,
            reason: String::new(),
        });
        handle_approval(&mut s, &key(KeyCode::Char('y')));
        assert_eq!(rx.try_recv().unwrap(), Decision::Allow);
    }

    #[tokio::test]
    async fn tui_approver_round_trips_and_remembers() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalAsk>();
        let approver = Arc::new(TuiApprover::new(tx));

        let a1 = approver.clone();
        let task1 = tokio::spawn(async move { a1.decide("run_command", "$ git status").await });
        let ask = rx.recv().await.expect("ask delivered");
        assert_eq!(ask.tool, "run_command");
        assert!(ask.summary.contains("git status"));
        ask.reply.send(Decision::AllowSession).expect("ui replies");
        assert_eq!(task1.await.unwrap(), Decision::Allow);
        assert_eq!(
            approver.decide("run_command", "$ git status").await,
            Decision::Allow
        );
        let a2 = approver.clone();
        let task2 =
            tokio::spawn(async move { a2.decide("run_command", "$ git log --oneline").await });
        let ask2 = rx.recv().await.expect("different command asks");
        ask2.reply.send(Decision::Deny(None)).expect("ui replies");
        assert_eq!(task2.await.unwrap(), Decision::Deny(None));
    }

    #[tokio::test]
    async fn shared_session_allows_survive_agent_rebuilds() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalAsk>();
        let allows = SessionAllows::default();
        let first = Arc::new(TuiApprover::with_allows(tx.clone(), allows.clone()));
        let a1 = first.clone();
        let task1 = tokio::spawn(async move { a1.decide("run_command", "$ cargo test").await });
        let ask = rx.recv().await.expect("ask delivered");
        ask.reply.send(Decision::AllowSession).expect("ui replies");
        assert_eq!(task1.await.unwrap(), Decision::Allow);
        let rebuilt = TuiApprover::with_allows(tx, allows);
        assert_eq!(
            rebuilt.decide("run_command", "$ cargo test").await,
            Decision::Allow
        );
    }

    fn stub_agent() -> Agent {
        let provider: Arc<dyn crate::llm::Provider> =
            Arc::new(crate::llm::mock::MockProvider::new(vec![]));
        let ctx = crate::tools::ToolCtx {
            cwd: std::env::temp_dir(),
            sandbox_root: None,
            timeout: std::time::Duration::from_secs(5),
            output_limit: 4096,
            allow_commands: vec![],
            approver: Arc::new(crate::tools::AutoApprover),
            checkpointing: None,
            todos: None,
            permission: crate::tools::Permission::default(),
            subagent: None,
        };
        Agent::with_parallel(
            provider,
            crate::tools::ToolRegistry::builtin(),
            ctx,
            Some("system".into()),
            3,
            1_000,
            2,
        )
    }

    #[tokio::test]
    async fn tui_commands_cover_the_shared_command_set() {
        let slot = Arc::new(tokio::sync::Mutex::new(Some(stub_agent())));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalAsk>();
        let mut sess = SessionState {
            cfg: crate::config::Config::default(),
            provider: None,
            model: None,
            session_name: "sess".to_string(),
        };
        let mut screen = test_screen();
        let perm = crate::tools::Permission::default();
        let allows = SessionAllows::default();
        let (bg_tx, _bg_rx) = tokio::sync::mpsc::unbounded_channel::<BackgroundEvent>();

        for cmd in [
            "help",
            "status",
            "auth",
            "last",
            "context",
            "tools",
            "undo",
            "permission",
            "permission read-only",
            "yolo",
            "resume",
            "details",
            "theme",
            "definitely-not-a-command",
            "retry",
        ] {
            let follow = run_command(
                cmd,
                &mut screen,
                &slot,
                &mut sess,
                &tx,
                &allows,
                &perm,
                &bg_tx,
            )
            .await;
            if cmd == "retry" {
                assert!(follow.is_none(), "no previous task to retry");
            }
        }

        let text = screen_text(&screen);
        assert!(text.contains("provider/model:"), "status:\n{text}");
        assert!(text.contains("no tool output"), "last:\n{text}");
        assert!(text.contains("context estimate"), "context:\n{text}");
        assert!(
            text.contains("permission mode -> read-only"),
            "permission set:\n{text}"
        );
        assert!(text.contains("permission mode -> yolo"), "yolo:\n{text}");
        assert!(text.contains("unknown command"), "unknown:\n{text}");
        assert_eq!(perm.get(), PermissionMode::Yolo);

        // `/help` opens the overlay rather than dumping text
        run_command(
            "help",
            &mut screen,
            &slot,
            &mut sess,
            &tx,
            &allows,
            &perm,
            &bg_tx,
        )
        .await;
        assert!(matches!(screen.overlay, Some(Overlay::Help(_))));
        screen.overlay = None;

        // `/new` clears the transcript and repoints autosave
        run_command(
            "new demo",
            &mut screen,
            &slot,
            &mut sess,
            &tx,
            &allows,
            &perm,
            &bg_tx,
        )
        .await;
        assert_eq!(
            sess.session_name, "demo",
            "session switched for later saves"
        );
        let text = screen_text(&screen);
        assert!(text.contains("new session -> demo"), "new view:\n{text}");

        // compact takes the agent out and finishes in the background
        run_command(
            "compact",
            &mut screen,
            &slot,
            &mut sess,
            &tx,
            &allows,
            &perm,
            &bg_tx,
        )
        .await;
        assert!(screen.busy);
        assert!(screen_text(&screen).contains("compacting"));
    }

    #[test]
    fn retry_returns_the_last_task() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let slot = Arc::new(tokio::sync::Mutex::new(Some(stub_agent())));
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalAsk>();
            let mut sess = SessionState {
                cfg: crate::config::Config::default(),
                provider: None,
                model: None,
                session_name: "sess".to_string(),
            };
            let mut screen = test_screen();
            screen.last_task = Some("fix the test".into());
            let perm = crate::tools::Permission::default();
            let allows = SessionAllows::default();
            let (bg_tx, _bg_rx) = tokio::sync::mpsc::unbounded_channel::<BackgroundEvent>();
            let follow = run_command(
                "retry",
                &mut screen,
                &slot,
                &mut sess,
                &tx,
                &allows,
                &perm,
                &bg_tx,
            )
            .await;
            assert_eq!(follow.as_deref(), Some("fix the test"));
        });
    }

    #[test]
    fn model_choices_split_provider_and_model() {
        let mut cfg = crate::config::Config::default();
        cfg.providers.insert(
            "deepseek".into(),
            crate::config::ProviderEntry {
                r#type: "openai".into(),
                model: Some("deepseek-chat".into()),
                base_url: None,
                api_key_env: None,
                models: None,
            },
        );
        assert_eq!(
            split_model_choice(&cfg, Some("other"), "deepseek/deepseek-reasoner"),
            (
                Some("deepseek".to_string()),
                "deepseek-reasoner".to_string()
            )
        );
        assert_eq!(
            split_model_choice(&cfg, Some("deepseek"), "gpt-5"),
            (Some("deepseek".to_string()), "gpt-5".to_string())
        );
    }

    #[test]
    fn transcript_helper_text_and_details() {
        let mut s = test_screen();
        s.push_notice(Level::Info, "hello");
        s.transcript.push_user("world");
        assert!(screen_text(&s).contains("hello"));
        assert!(!s.transcript.details());
        s.transcript.set_details(true);
        assert!(s.transcript.details());
    }
}
