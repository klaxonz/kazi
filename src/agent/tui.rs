//! Full-screen TUI for `kazi chat` (`--tui`), built on ratatui + crossterm.
//!
//! Layout: one-row status bar (provider, model, session, rounds, tokens) on
//! top; scrolling transcript in the middle; one-row input at the bottom.
//! Tool activity renders inside the transcript as `>> tool [ok] preview`
//! lines, same visual language as the classic REPL. Any terminal error
//! falls back to the classic line REPL — the TUI is never load-bearing.
//!
//! Input: Enter submits, Esc exits, Ctrl-L clears, typed text supports
//! backspace; the input keeps prior text across redraws. Multi-line pastes are
//! preserved (`↵` marks the line breaks) instead of firing the first line as a
//! task. Mouse wheel scrolls; native text selection needs Shift+drag.

use futures::StreamExt;
use std::io::{Stdout, Write};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CtEvent, EventStream, KeyCode, KeyEventKind, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};

use super::{Agent, AgentEvent};
use crate::tools::{approval_key, Approver, Decision};

const CYAN: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const GREEN: Color = Color::Green;
const RED: Color = Color::Red;
const YELLOW: Color = Color::Yellow;

/// Key bindings printed by `/help` (the command list itself is shared with
/// the line REPL — one table, both front-ends).
const TUI_HELP: &[&str] = &[
    "keys: Enter submit · Ctrl+J new line · Esc interrupt (idle: quit) · Ctrl+C quit",
    "↑/↓ input history (lines inside a multi-line task) · Ctrl+A/E/U/K/W edit · Ctrl+L clear transcript",
    "/ opens the command list · Tab completes",
    "PgUp/PgDn or mouse wheel scroll · Ctrl+Home top · Ctrl+End bottom",
    "Shift+Tab cycles permission mode (read-only → yolo → edit)",
    "approval prompt: y once · a always (this command) · n/Esc deny",
    "@file in a task attaches the file's contents",
];

/// Approval request travelling from the agent thread to the UI loop.
pub struct ApprovalAsk {
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
    async fn decide(&self, _tool: &str, summary: &str) -> Decision {
        let key = approval_key(summary);
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

/// What the TUI turns events into before the transcript scroll absorbs them.
#[derive(Debug, Clone)]
enum LineKind {
    User,
    Assistant,
    Tool { is_error: bool },
    Dim,
    Warn,
}

/// Single-line editor state: text + a char-indexed caret + recallable history
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

    /// Insert a pasted block at the caret. Line breaks normalize to `\n` — the
    /// buffer keeps the real text (shown as `↵` on the input line), so a
    /// multi-line paste lands as ONE task instead of submitting at the first
    /// newline and dumping the rest into a busy input box.
    fn insert_str(&mut self, text: &str) {
        let mut after_cr = false;
        for ch in text.chars() {
            match ch {
                '\r' => {
                    self.insert('\n');
                    after_cr = true;
                }
                '\n' if after_cr => after_cr = false, // CRLF: newline already added
                c => {
                    self.insert(c);
                    after_cr = false;
                }
            }
        }
    }

    /// Replace the whole line (Tab completion) and park the caret at the end.
    fn set_text(&mut self, text: String) {
        self.text = text;
        self.cursor = self.chars_len();
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

    // ---- multi-line support: Ctrl+J starts a new line inside a task --------

    /// Ctrl+J: start a new line instead of submitting.
    fn newline(&mut self) {
        self.insert('\n');
    }

    fn line_count(&self) -> usize {
        self.text.matches('\n').count() + 1
    }

    /// The buffer split into display lines (newlines are the boundaries).
    fn display_lines(&self) -> Vec<String> {
        self.text.split('\n').map(str::to_string).collect()
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

    /// Index of the caret's display line (0-based).
    fn caret_line_index(&self) -> usize {
        let (start, _) = self.caret_line();
        self.text.chars().take(start).filter(|c| *c == '\n').count()
    }

    /// Caret column inside its own display line.
    fn caret_col(&self) -> usize {
        let (start, _) = self.caret_line();
        self.cursor - start
    }

    /// First display line to render so the caret's line is inside `visible`.
    fn display_window_start(&self, visible: usize) -> usize {
        let idx = self.caret_line_index();
        if visible == 0 || idx < visible {
            0
        } else {
            idx + 1 - visible
        }
    }

    /// Move the caret one display line up, keeping its column.
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

    /// Move the caret one display line down, keeping its column.
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

    /// Home within the caret's line (multi-line buffers).
    fn home_line(&mut self) {
        let (start, _) = self.caret_line();
        self.cursor = start;
    }

    /// End within the caret's line (multi-line buffers).
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
        self.text.clear();
        self.cursor = 0;
        self.hist_pos = None;
        self.draft.clear();
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

/// Horizontal window that keeps the caret visible inside `budget` columns.
/// Returns the visible slice plus the caret's column offset inside it.
fn window_for_cursor(text: &str, cursor: usize, budget: usize) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    if budget == 0 {
        return (String::new(), 0);
    }
    let mut start = 0usize;
    let mut pre: usize = chars[..cursor]
        .iter()
        .map(|&c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();
    // shift the window right until the caret's prefix fits
    while start < cursor && pre > budget {
        pre -= UnicodeWidthChar::width(chars[start]).unwrap_or(0);
        start += 1;
    }
    // extend to the right while columns remain
    let mut end = cursor;
    let mut total = pre;
    while end < chars.len() {
        let cw = UnicodeWidthChar::width(chars[end]).unwrap_or(0);
        if total + cw > budget {
            break;
        }
        total += cw;
        end += 1;
    }
    (chars[start..end].iter().collect(), pre)
}

/// Which list a modal [`Picker`] is choosing from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    Provider,
    Model,
}

/// A provider/model switch finishing in the background, so the event loop keeps
/// drawing (spinner + elapsed) instead of freezing on MCP/HTTP work.
enum SwitchDone {
    Ok {
        agent: Box<Agent>,
        provider: Option<String>,
        model: Option<String>,
        desc: String,
    },
    Failed {
        agent: Box<Agent>,
        note: String,
    },
}

/// A modal list picker rendered above the input: `/provider` and `/model`
/// open one. Type to filter, ↑/↓ to move, Enter to select, Esc to cancel.
#[derive(Debug, Clone)]
struct Picker {
    kind: PickerKind,
    title: String,
    all: Vec<String>,
    filter: String,
    cursor: usize,
    /// Hoisted/marked in the list (the currently active value).
    current: Option<String>,
}

impl Picker {
    fn new(
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
        }
    }

    /// Case-insensitive substring filter over the candidates.
    fn filtered(&self) -> Vec<&String> {
        if self.filter.is_empty() {
            return self.all.iter().collect();
        }
        let needle = self.filter.to_lowercase();
        self.all
            .iter()
            .filter(|m| m.to_lowercase().contains(&needle))
            .collect()
    }

    /// Move the cursor within the filtered list, clamped at both ends.
    fn move_cursor(&mut self, delta: isize) {
        let len = self.filtered().len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let cur = self.cursor.min(len - 1) as isize;
        self.cursor = (cur + delta).clamp(0, len as isize - 1) as usize;
    }

    fn selected(&self) -> Option<String> {
        self.filtered().get(self.cursor).map(|m| (*m).clone())
    }
}

/// Everything needed to render one screenful.
struct Screen {
    provider_desc: String,
    session_name: String,
    max_rounds: u32,
    /// (kind, text) transcript lines; scrolling shows the tail.
    lines: Vec<(LineKind, String)>,
    input: Input,
    /// Running task stats from the latest events.
    rounds: u32,
    tokens_in: u64,
    tokens_out: u64,
    est_tokens: u64,
    /// Streamed assistant text of the CURRENT task (paragraph, wraps itself).
    streaming: String,
    /// Transcript scroll: lines above the bottom (0 = follow the tail).
    /// Cell because the clamp depends on the last draw's viewport height.
    scroll: std::cell::Cell<usize>,
    /// Wrapped-line count of the last draw: while scrolled, new output must
    /// not drag the viewport out from under the reader.
    last_total: std::cell::Cell<usize>,
    /// Bumped whenever the transcript content changes (wrap-cache key).
    rev: std::cell::Cell<u64>,
    /// (rev, body width, wrapped rows) — re-wrapping the whole transcript on
    /// every keystroke is what made long sessions feel sluggish.
    wrap_cache: std::cell::RefCell<(u64, u16, Vec<Line<'static>>)>,
    /// Task start time + spinner frame while a run is in flight, so a model
    /// that thinks quietly is visibly *working*, not hung.
    started: std::cell::Cell<Option<std::time::Instant>>,
    spinner: std::cell::Cell<usize>,
    /// While true, input is disabled (a task is in flight).
    busy: bool,
    /// A pending approval question: rendered instead of the input line and
    /// answered by the next keystroke (y/a/n).
    pending_approval: Option<ApprovalAsk>,
    /// A modal list picker (provider/model selection), drawn above the input.
    picker: Option<Picker>,
    /// Highlighted row of the `/` command dropdown: ↑/↓ move it, Enter runs it.
    cmd_sel: std::cell::Cell<usize>,
    /// The model's plan checklist: (content, status) pairs.
    todos: Vec<(String, String)>,
    /// Current permission mode (mirrors the shared cell; Shift+Tab cycles).
    mode: crate::tools::PermissionMode,
    /// Set when the user asked to quit.
    quit: bool,
}

impl Screen {
    fn new(provider_desc: &str, session_name: &str, max_rounds: u32) -> Self {
        let input = Input {
            history: super::repl::load_input_history(),
            ..Default::default()
        };
        Self {
            provider_desc: provider_desc.to_string(),
            session_name: session_name.to_string(),
            max_rounds,
            lines: Vec::new(),
            input,
            rounds: 0,
            tokens_in: 0,
            tokens_out: 0,
            est_tokens: 0,
            streaming: String::new(),
            scroll: std::cell::Cell::new(0),
            last_total: std::cell::Cell::new(0),
            rev: std::cell::Cell::new(0),
            wrap_cache: std::cell::RefCell::new((u64::MAX, 0, Vec::new())),
            started: std::cell::Cell::new(None),
            spinner: std::cell::Cell::new(0),
            busy: false,
            pending_approval: None,
            picker: None,
            cmd_sel: std::cell::Cell::new(0),
            todos: Vec::new(),
            mode: crate::tools::PermissionMode::Edit,
            quit: false,
        }
    }

    /// Transcript cap: enough for any real session, bounded so a runaway
    /// stream cannot grow memory (and per-frame work) without limit.
    const MAX_LINES: usize = 4_000;

    fn push(&mut self, kind: LineKind, text: impl Into<String>) {
        self.lines.push((kind, text.into()));
        if self.lines.len() > Self::MAX_LINES {
            let excess = self.lines.len() - Self::MAX_LINES;
            self.lines.drain(0..excess);
        }
        self.touch();
    }

    /// Mark the wrapped-row cache stale after any transcript change.
    fn touch(&self) {
        self.rev.set(self.rev.get().wrapping_add(1));
    }

    /// Ctrl+L: drop the transcript buffer without touching the conversation.
    fn clear_transcript(&mut self) {
        self.lines.clear();
        self.touch();
        self.scroll_to_bottom();
    }

    /// Scroll the transcript (clamped during draw against the viewport).
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

    /// Completion rows to offer while the line is still a command token —
    /// the `/` dropdown. Empty once the user types an argument.
    fn command_hints(&self) -> Vec<(String, String)> {
        match super::repl::pending_command(&self.input.text) {
            Some(token) => super::repl::command_rows(token),
            None => Vec::new(),
        }
    }

    /// Render one agent event into the screen state.
    fn consume(&mut self, ev: AgentEvent) {
        // in-place edits below must invalidate the wrapped-row cache
        self.touch();
        match ev {
            AgentEvent::TodosUpdated { items } => self.todos = items,
            AgentEvent::Feedback {
                command,
                ok,
                preview,
            } => {
                self.flush_streaming();
                self.push(
                    LineKind::Tool { is_error: !ok },
                    format!("feedback `{command}` — {preview}"),
                );
            }
            AgentEvent::TextDelta { text } => self.streaming.push_str(&text),
            AgentEvent::ToolStart { name } => {
                self.flush_streaming();
                self.push(LineKind::Tool { is_error: false }, format!(">> {name} ..."));
            }
            AgentEvent::ToolEnd {
                name,
                is_error,
                preview,
            } => {
                // replace the last ">> name ..." placeholder with the verdict
                let placeholder = format!(">> {name} ...");
                if let Some(last) = self.lines.last_mut() {
                    if last.1 == placeholder {
                        last.0 = LineKind::Tool { is_error };
                        last.1 = format!(
                            ">> {name} [{verdict}] {preview}",
                            verdict = if is_error { "x" } else { "ok" }
                        );
                        return;
                    }
                }
                self.push(
                    LineKind::Tool { is_error },
                    format!(
                        ">> {name} [{verdict}] {preview}",
                        verdict = if is_error { "x" } else { "ok" }
                    ),
                );
            }
            AgentEvent::AutoCompactStarted => {
                self.flush_streaming();
                self.push(LineKind::Warn, ">> auto-compact ...".to_string());
            }
            AgentEvent::AutoCompactDone { note, est_tokens } => {
                let placeholder = ">> auto-compact ...";
                if let Some(last) = self.lines.last_mut() {
                    if last.1 == placeholder {
                        last.0 = LineKind::Dim;
                        last.1 = format!("{note} (est. {est_tokens} tokens)");
                        return;
                    }
                }
                self.push(LineKind::Dim, format!("{note} (est. {est_tokens} tokens)"));
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

    /// Move streamed text into the transcript (task boundary, next tool).
    fn flush_streaming(&mut self) {
        if !self.streaming.is_empty() {
            let text = std::mem::take(&mut self.streaming);
            self.push(LineKind::Assistant, text);
        }
    }

    /// Submit the current input as a user task.
    fn take_input(&mut self) -> Option<String> {
        let t = self.input.submit()?;
        self.push(LineKind::User, format!("you> {t}"));
        // shared with the line REPL: ↑ after a restart recalls the last tasks
        super::repl::remember_input(&t);
        Some(t)
    }

    /// Enter/Tab resolution of a half-typed command: the unique match, or the
    /// row the `/` dropdown is highlighting.
    fn resolve_command_input(&self) -> Option<String> {
        let token = super::repl::pending_command(&self.input.text)?;
        let rows = super::repl::command_rows(token);
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

    /// Move the `/` dropdown highlight by `delta` (wrapping).
    fn move_command_sel(&self, delta: isize) {
        let len = self.command_hints().len();
        if len == 0 {
            return;
        }
        let cur = self.cmd_sel.get().min(len - 1) as isize;
        self.cmd_sel
            .set((cur + delta).rem_euclid(len as isize) as usize);
    }

    /// Lines the input buffer occupies on screen (capped while typing).
    fn input_lines(&self) -> usize {
        self.input.line_count().min(6)
    }

    /// Rows the bottom area needs: 3 for a one-line input, more when the task
    /// spans several lines or when an approval needs the command readable.
    fn bottom_rows(&self, width: u16, height: u16) -> u16 {
        if let Some(p) = &self.pending_approval {
            let w = width.saturating_sub(2).max(20) as usize;
            let want = wrap_text(&p.summary, w).len() as u16 + 2;
            return want.clamp(4, 9).min(height.saturating_sub(4).max(3));
        }
        (self.input_lines() as u16 + 2).min(8)
    }

    /// Paint one frame. Height budget: status 1, plan checklist, transcript,
    /// input 3. The `/` completion menu is an overlay — it floats above the
    /// input and never steals layout rows from the transcript.
    fn draw(&self, f: &mut Frame) {
        let todo_rows = self.todos.len().min(6) as u16;
        // the bottom area grows for a multi-line task and for approvals
        let input_rows = self.bottom_rows(f.area().width, f.area().height);
        let [status, todos_area, body, input] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(todo_rows),
            Constraint::Min(3),
            Constraint::Length(input_rows),
        ])
        .areas(f.area());

        if todo_rows > 0 {
            let lines: Vec<Line> = self
                .todos
                .iter()
                .take(6)
                .map(|(content, status)| {
                    let (marker, style) = match status.as_str() {
                        "completed" => ("[x]", Style::default().fg(DIM)),
                        "in_progress" => ("[>]", Style::default().fg(CYAN)),
                        _ => ("[ ]", Style::default()),
                    };
                    Line::styled(format!("  {marker} {content}"), style)
                })
                .collect();
            f.render_widget(Paragraph::new(Text::from(lines)), todos_area);
        }

        // -- status bar -----------------------------------------------------
        // A live spinner while a task runs: a model thinking quietly used to
        // look exactly like a hang.
        let budget_note = if self.busy {
            let secs = self
                .started
                .get()
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            format!(
                "round {}/{}  in {} out {}  ctx ~{}  {} {secs}s",
                self.rounds,
                self.max_rounds,
                self.tokens_in,
                self.tokens_out,
                self.est_tokens,
                SPINNER[self.spinner.get() % SPINNER.len()]
            )
        } else {
            format!("ctx ~{}", self.est_tokens)
        };
        // the permission mode comes early on purpose: it must survive a narrow
        // terminal's truncation
        let mut status_spans = vec![
            Span::styled(" kazi ", Style::default().fg(Color::Black).bg(CYAN).bold()),
            Span::styled(
                format!("[{}] ", self.mode.as_str()),
                Style::default().fg(match self.mode {
                    crate::tools::PermissionMode::ReadOnly => YELLOW,
                    crate::tools::PermissionMode::Edit => DIM,
                    crate::tools::PermissionMode::Yolo => RED,
                }),
            ),
            Span::styled(&self.provider_desc, Style::default().fg(DIM)),
            Span::raw("  "),
            Span::styled(&self.session_name, Style::default().fg(CYAN)),
            Span::raw("  "),
            Span::styled(budget_note, Style::default().fg(DIM)),
        ];
        if self.scroll.get() > 0 {
            status_spans.push(Span::styled(
                "  [scrolled — Ctrl+End: bottom]",
                Style::default().fg(YELLOW),
            ));
        }
        f.render_widget(Line::from(status_spans).style(Style::new()), status);

        // -- transcript ------------------------------------------------------
        // Wrapping the whole transcript happens only when its content (or the
        // window width) changed; every other frame reuses the cached rows.
        let body_width = body.width.saturating_sub(2).max(20);
        {
            let mut cache = self.wrap_cache.borrow_mut();
            if cache.0 != self.rev.get() || cache.1 != body_width {
                let mut out: Vec<Line<'static>> = Vec::with_capacity(self.lines.len());
                for (kind, content) in &self.lines {
                    let style = match kind {
                        LineKind::User => Style::default().fg(CYAN),
                        LineKind::Assistant => Style::default(),
                        LineKind::Tool { is_error } => {
                            if *is_error {
                                Style::default().fg(RED)
                            } else {
                                Style::default().fg(GREEN)
                            }
                        }
                        LineKind::Dim => Style::default().fg(DIM),
                        LineKind::Warn => Style::default().fg(YELLOW),
                    };
                    // pre-wrapping keeps the scroll arithmetic honest
                    for chunk in wrap_text(content, body_width as usize) {
                        out.push(Line::styled(chunk, style));
                    }
                }
                *cache = (self.rev.get(), body_width, out);
            }
        }

        // streaming assistant text lives at the bottom of the transcript
        let stream_rows: Vec<Line<'static>> = if self.streaming.is_empty() {
            Vec::new()
        } else {
            wrap_text(&self.streaming, body_width as usize)
                .into_iter()
                .map(Line::raw)
                .collect()
        };
        let total = self.wrap_cache.borrow().2.len() + stream_rows.len();
        // a scrolled reader keeps their place: new output moves the anchor too
        let grew = total.saturating_sub(self.last_total.replace(total));
        if grew > 0 && self.scroll.get() > 0 {
            self.scroll.set(self.scroll.get() + grew);
        }
        let height = body.height as usize;
        let (start, end, off) = scroll_range(total, height, self.scroll.get());
        self.scroll.set(off); // keep the stored offset within bounds
        let visible: Vec<Line<'static>> = {
            let cache = self.wrap_cache.borrow();
            let static_len = cache.2.len();
            let mut out = Vec::with_capacity(end.saturating_sub(start));
            for i in start..end {
                if i < static_len {
                    out.push(cache.2[i].clone());
                } else if let Some(row) = stream_rows.get(i - static_len) {
                    out.push(row.clone());
                }
            }
            out
        };
        f.render_widget(Paragraph::new(Text::from(visible)), body);

        // -- `/` completion menu --------------------------------------------
        // An overlay: a full-width panel sitting directly above the input, so
        // it never steals layout rows from the transcript and never leaves
        // fragments of the lines it covers. How many entries fit is decided by
        // the space above the input, not by a fixed budget.
        if self.pending_approval.is_none() {
            let avail = input.y.saturating_sub(status.y + 1) as usize;
            if let Some(picker) = &self.picker {
                draw_picker(f, input, avail, picker);
            } else {
                let hints = self.command_hints();
                if !hints.is_empty() && avail >= 2 {
                    let shown = hints.len().min((avail - 1).min(16));
                    let hidden = hints.len() - shown;
                    let cmd_w = hints
                        .iter()
                        .take(shown)
                        .map(|(c, _)| UnicodeWidthStr::width(c.as_str()))
                        .max()
                        .unwrap_or(8);
                    let height = (shown + 1) as u16;
                    let area = ratatui::layout::Rect {
                        x: 0,
                        y: input.y - height,
                        width: f.area().width,
                        height,
                    };
                    let mut lines: Vec<Line> = hints
                        .iter()
                        .take(shown)
                        .enumerate()
                        .map(|(i, (cmd, help))| {
                            let sel = self.cmd_sel.get().min(shown.saturating_sub(1));
                            let cmd_style = if i == sel {
                                Style::default().fg(Color::Black).bg(CYAN).bold()
                            } else {
                                Style::default().fg(DIM)
                            };
                            Line::from(vec![
                                Span::styled(format!(" {cmd:<cmd_w$} "), cmd_style),
                                Span::styled(help.clone(), Style::default().fg(DIM)),
                            ])
                        })
                        .collect();
                    lines.push(Line::styled(
                        if hidden > 0 {
                            format!(
                                " … +{hidden} more · ↑/↓ select · Enter runs it · Tab completes"
                            )
                        } else {
                            " ↑/↓ select · Enter runs it · Tab completes".to_string()
                        },
                        Style::default().fg(DIM),
                    ));
                    f.render_widget(Clear, area);
                    f.render_widget(Paragraph::new(Text::from(lines)), area);
                }
            }
        }

        // -- input box (or the pending approval question) ---------------------
        if let Some(pending) = &self.pending_approval {
            let width = input.width.saturating_sub(2).max(20) as usize;
            let summary = wrap_text(&pending.summary, width);
            // never clip the key hints: they are the only way to answer
            let room = input.height.saturating_sub(2) as usize;
            let mut lines: Vec<Line> = Vec::with_capacity(room + 1);
            for (i, text) in summary.iter().take(room).enumerate() {
                let mut body = text.clone();
                if i + 1 == room && summary.len() > room {
                    body.push_str(" …");
                }
                let lead = if i == 0 {
                    Span::styled(
                        " approve ",
                        Style::default().fg(Color::Black).bg(YELLOW).bold(),
                    )
                } else {
                    Span::raw("         ")
                };
                lines.push(Line::from(vec![lead, Span::raw(body)]));
            }
            lines.push(Line::from(vec![Span::styled(
                " [y] once   [a] always this command (session)   [n] deny",
                Style::default().fg(YELLOW),
            )]));
            f.render_widget(
                Paragraph::new(Text::from(lines)).block(Block::default().borders(Borders::TOP)),
                input,
            );
            return;
        }
        let prompt = if self.busy { " [busy] " } else { " kazi> " };
        let prompt_w = UnicodeWidthStr::width(prompt);
        let budget = (input.width as usize)
            .saturating_sub(1)
            .saturating_sub(prompt_w);
        // one row per buffer line (Ctrl+J / paste), windowed on the caret
        let rows = self.input.display_lines();
        let visible_rows = (input.height.saturating_sub(1) as usize).max(1);
        let first = self.input.display_window_start(visible_rows);
        let caret_line = self.input.caret_line_index();
        let mut caret_at: Option<(usize, usize)> = None;
        let mut lines: Vec<Line> = Vec::new();
        for (i, text) in rows.iter().enumerate().skip(first).take(visible_rows) {
            let lead = if i == first {
                Span::styled(prompt, Style::default().fg(CYAN).bold())
            } else {
                Span::raw(" ".repeat(prompt_w))
            };
            if i == caret_line {
                let (shown, caret_col) = window_for_cursor(text, self.input.caret_col(), budget);
                caret_at = Some((lines.len() + 1, caret_col));
                lines.push(Line::from(vec![lead, Span::raw(shown)]));
            } else {
                lines.push(Line::from(vec![
                    lead,
                    Span::raw(truncate_display(text, budget)),
                ]));
            }
        }
        f.render_widget(
            Paragraph::new(Text::from(lines)).block(Block::default().borders(Borders::TOP)),
            input,
        );

        if let Some((row, col)) = caret_at {
            let x = (input.x + prompt_w as u16 + col as u16).min(input.right().saturating_sub(1));
            let y = (input.y + row as u16).min(input.bottom().saturating_sub(1));
            f.set_cursor_position(ratatui::layout::Position { x, y });
        }
    }
}

/// Render the modal picker as an overlay directly above the input: a title
/// line, the filtered rows (cursor highlighted, window scrolled to keep it
/// visible), and a footer with the current filter and key hints.
fn draw_picker(f: &mut Frame, input: ratatui::layout::Rect, avail: usize, p: &Picker) {
    if avail < 3 {
        return; // no room for title + a row + footer
    }
    let hints = p.filtered();
    let shown = hints.len().min(avail - 2).min(16);
    let start = if shown > 0 && p.cursor >= shown {
        p.cursor + 1 - shown
    } else {
        0
    };
    let height = (shown + 2) as u16;
    let area = ratatui::layout::Rect {
        x: 0,
        y: input.y - height,
        width: f.area().width,
        height,
    };
    let mut lines: Vec<Line> = Vec::with_capacity(shown + 2);
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {} ", p.title),
            Style::default().fg(Color::Black).bg(CYAN).bold(),
        ),
        Span::styled(
            match &p.current {
                Some(c) => format!(" current: {c}"),
                None => String::new(),
            },
            Style::default().fg(DIM),
        ),
    ]));
    if shown == 0 {
        lines.push(Line::styled("  (no match)", Style::default().fg(YELLOW)));
    } else {
        for (i, m) in hints.iter().enumerate().skip(start).take(shown) {
            let style = if i == p.cursor {
                Style::default().fg(Color::Black).bg(CYAN).bold()
            } else {
                Style::default().fg(DIM)
            };
            let marker = if Some(m.as_str()) == p.current.as_deref() {
                "  (current)"
            } else {
                ""
            };
            lines.push(Line::styled(format!(" {m}{marker}"), style));
        }
    }
    let extra = if shown < hints.len() {
        format!(" · ↑/↓ · Enter · Esc  ({}/{})", p.cursor + 1, hints.len())
    } else {
        " · ↑/↓ · Enter · Esc".to_string()
    };
    lines.push(Line::styled(
        format!(" filter: {}_", p.filter) + &extra,
        Style::default().fg(DIM),
    ));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// (start, end, clamped_offset) of the transcript window: `off` counts lines
/// hidden below the viewport (0 = follow the tail).
fn scroll_range(total: usize, height: usize, off: usize) -> (usize, usize, usize) {
    let off = off.min(total.saturating_sub(height));
    let end = total.saturating_sub(off);
    let start = end.saturating_sub(height);
    (start, end, off)
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
        let _ = writeln!(
            f,
            "code={:?} mods={:?} kind={:?}",
            key.code, key.modifiers, key.kind
        );
    }
}

/// Which key presses become literal text in the input box.
///
/// Modifier combinations are *not* text: terminals deliver IME switches and
/// other shortcuts as `Char`, e.g. `Ctrl+Space` (switch input method) arrives
/// as `Char(' ') + CONTROL` and `Shift+Space` as `Char(' ') + SHIFT`. Typing
/// them verbatim is what produced stray spaces during Chinese input. NUL is
/// an unknown-key artifact. Plain characters (optionally shifted for
/// punctuation) pass through.
fn accepted_char(code: &KeyCode, mods: ratatui::crossterm::event::KeyModifiers) -> Option<char> {
    use ratatui::crossterm::event::KeyModifiers as M;
    match code {
        KeyCode::Char('\0') => None,
        // AltGr arrives as CONTROL|ALT on Windows/X11 keyboard layouts: that is
        // a real character (`@ \ [ ] { } |`), not a shortcut. Only CONTROL
        // without ALT is a modifier combo (`Ctrl+Space` IME switch, `Ctrl+A`).
        KeyCode::Char(c) if mods.contains(M::CONTROL) && !mods.contains(M::ALT) => None,
        KeyCode::Char(' ') if mods.contains(M::SHIFT) => None,
        KeyCode::Char(c) => Some(*c),
        _ => None,
    }
}

/// First `budget` display columns of a line — non-caret input lines are
/// clipped, not horizontally scrolled.
fn truncate_display(text: &str, budget: usize) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

/// Spinner frames for the busy status (ASCII: renders on legacy consoles).
const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

/// Word-ish wrap by DISPLAY width (CJK glyphs are double-width): break
/// preferentially at spaces, hard-break long words glyph by glyph — never by
/// bytes, which would panic on multi-byte chars.
fn wrap_text(s: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut out = Vec::new();
    for para in s.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut cur_w = 0usize;
        let mut has_content = false;
        let push_line = |out: &mut Vec<String>, cur: &mut String, cur_w: &mut usize| {
            out.push(std::mem::take(cur));
            *cur_w = 0;
        };
        for word in para.split(' ') {
            let word_w = UnicodeWidthStr::width(word);
            if has_content && cur_w + 1 + word_w > max {
                push_line(&mut out, &mut cur, &mut cur_w);
                has_content = false;
            }
            if has_content {
                cur.push(' ');
                cur_w += 1;
            }
            for ch in word.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if cur_w + cw > max && cur_w > 0 {
                    push_line(&mut out, &mut cur, &mut cur_w);
                }
                cur.push(ch);
                cur_w += cw;
            }
            has_content = true;
        }
        out.push(cur);
    }
    out
}

/// Park-and-run helper: kept for library users who want the simple blocking
/// variant of a task run with event streaming (the TUI itself uses the
/// concurrent runner above).
#[allow(dead_code)]
async fn run_streaming(
    agent: &mut Agent,
    task: &str,
    on_event: impl FnMut(AgentEvent) + Send,
) -> Result<String, super::AgentError> {
    agent.run_task(task, on_event).await
}

/// Entry used by the CLI: builds the agent (reusing repl's construction and
/// session-name/resume semantics), runs the TUI, and saves at the end.
/// Terminal setup failures (no tty, restricted env) fall back to the classic
/// REPL rather than failing the invocation.
pub async fn chat_loop_with(
    cfg: crate::config::Config,
    provider: Option<String>,
    model: Option<String>,
    mode: crate::tools::PermissionMode,
    topic: Option<String>,
    resume: Option<String>,
) -> anyhow::Result<u8> {
    let session_name = match resume.as_deref() {
        Some(name) => super::session::resolve(name).unwrap_or_else(|| name.to_string()),
        None => {
            let base = match topic
                .as_deref()
                .map(super::repl::slugify)
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
    // Channel-based approval: the agent asks, the UI loop answers by key.
    let (ask_tx, ask_rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalAsk>();
    // kept for agents rebuilt inside the TUI (/provider, /model)
    let ask_tx_rebuild = ask_tx.clone();
    // one shared "always allow" set: rebuilding the agent must not forget it
    let session_allows = SessionAllows::default();
    let approver: std::sync::Arc<dyn Approver> = if mode == crate::tools::PermissionMode::Yolo {
        std::sync::Arc::new(crate::tools::AutoApprover)
    } else {
        std::sync::Arc::new(TuiApprover::with_allows(ask_tx, session_allows.clone()))
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

    // resume the same way repl.rs does
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
    // first task must not print "session saved", and an in-TUI `/new` must not
    // make the exit line name a file that was never written.
    let saved_name = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let save = |name: &str, agent: &Agent| {
        if agent.history.msgs_len() == 0 {
            return; // empty sessions never touch the disk
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
        Err(_) => {
            // terminal can't do alternate screens: classic REPL fallback
            crate::agent::repl::chat_loop(cfg, provider, model, mode, topic, resume).await
        }
    }
}

/// Mutable session view the in-TUI slash commands act on: `/provider`,
/// `/model` and `/new` change what the next save and rebuild use.
pub struct SessionState {
    pub cfg: crate::config::Config,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub session_name: String,
}

/// Run the TUI chat loop. `chat_loop_tui` owns its terminal; stdout is
/// restored on every return path. `on_session_save` receives the CURRENT
/// session name (in-TUI `/new` may have changed it) plus the agent.
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
    let permission = agent.ctx.permission.clone();
    // provider/model switches run as background tasks and report back here
    let (switch_tx, mut switches) = tokio::sync::mpsc::unbounded_channel::<SwitchDone>();

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

    let mut screen = Screen::new(&provider_desc, &session_name, max_rounds);
    screen.mode = permission.get();
    // a missing key would otherwise only surface as a 401 on the first task
    if let Some(hint) = agent.auth_hint() {
        screen.push(LineKind::Warn, hint.to_string());
    }
    screen.push(
        LineKind::Dim,
        "type a task and press Enter · / for commands · Esc quits".to_string(),
    );

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
        &mut switches,
        &switch_tx,
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
    // the save already happened inside tui_inner with the real agent
    // (`None` = aborted mid-task: nothing new to persist)
    res.map(|_| if quit_clean { 0 } else { 1 })
}

/// First few non-empty lines of a message (history replay keeps it readable).
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

/// Paint a resumed session's conversation into the transcript: `/resume` used
/// to load the model's memory while leaving the screen blank, so you had no
/// idea what you were continuing.
fn replay_history(screen: &mut Screen, agent: &Agent) {
    const TAIL: usize = 20;
    let msgs = &agent.history.msgs;
    if msgs.is_empty() {
        return;
    }
    let start = msgs.len().saturating_sub(TAIL);
    screen.push(
        LineKind::Dim,
        format!("— replaying the last {} messages —", msgs.len() - start),
    );
    for m in &msgs[start..] {
        match m.role {
            crate::llm::Role::User => {
                screen.push(LineKind::User, format!("you> {}", first_lines(m.text(), 6)));
            }
            crate::llm::Role::Assistant => {
                let text = m.text().trim();
                if !text.is_empty() {
                    screen.push(LineKind::Assistant, first_lines(text, 12));
                }
                for tc in &m.tool_calls {
                    screen.push(
                        LineKind::Tool { is_error: false },
                        format!(">> {} (call)", tc.name),
                    );
                }
            }
            crate::llm::Role::Tool => screen.push(
                LineKind::Tool {
                    is_error: m.is_error,
                },
                format!(
                    ">> tool [{}] {}",
                    if m.is_error { "x" } else { "ok" },
                    first_lines(m.text(), 4)
                ),
            ),
            crate::llm::Role::System => {}
        }
    }
}

/// Append multi-line text to the transcript, one line per row.
fn push_lines(screen: &mut Screen, text: impl AsRef<str>) {
    for line in text.as_ref().lines() {
        screen.push(LineKind::Dim, line.to_string());
    }
}

const BUSY_TEXT: &str = "the agent is busy — try again when the task finishes";

/// Answer one slash command typed inside the TUI. Same command set as the
/// line REPL (they share `repl`'s text builders); the output lands in the
/// transcript and `/provider`, `/model`, `/new` may replace the agent.
#[allow(clippy::too_many_arguments)]
async fn run_command(
    cmdline: &str,
    screen: &mut Screen,
    slot: &std::sync::Arc<tokio::sync::Mutex<Option<Agent>>>,
    sess: &mut SessionState,
    ask_tx: &tokio::sync::mpsc::UnboundedSender<ApprovalAsk>,
    session_allows: &SessionAllows,
    permission: &crate::tools::Permission,
    switch_tx: &tokio::sync::mpsc::UnboundedSender<SwitchDone>,
    save: &(impl Fn(&str, &Agent) + ?Sized),
) {
    use super::repl;
    use crate::tools::PermissionMode as Mode;
    let verb = cmdline.split_once(' ').map(|(a, _)| a).unwrap_or(cmdline);
    let arg = cmdline.split_once(' ').map(|(_, b)| b.trim()).unwrap_or("");
    // `/Undo` must behave like `/undo`: the table is lowercase
    let verb_key = verb.to_ascii_lowercase();

    // `/undo 3` and friends: explain instead of silently ignoring the argument
    if let Some(usage) = repl::no_arg_violation(&verb_key, arg) {
        push_lines(screen, usage);
        return;
    }

    match verb_key.as_str() {
        "" => {} // a bare `/`
        "help" => {
            push_lines(screen, repl::help_text());
            for line in TUI_HELP {
                screen.push(LineKind::Dim, line.to_string());
            }
        }
        "undo" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => repl::undo_text(a),
                None => BUSY_TEXT.to_string(),
            };
            push_lines(screen, text);
        }
        "context" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => repl::context_report(a),
                None => BUSY_TEXT.to_string(),
            };
            push_lines(screen, text);
        }
        "status" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => repl::status_text(a),
                None => BUSY_TEXT.to_string(),
            };
            push_lines(screen, text);
        }
        "auth" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => repl::auth_report(a, &sess.cfg),
                None => BUSY_TEXT.to_string(),
            };
            push_lines(screen, text);
        }
        "last" => {
            let Some(back) = repl::parse_back(arg) else {
                push_lines(screen, repl::usage_for("/last"));
                return;
            };
            let text = match slot.lock().await.as_ref() {
                Some(a) => repl::last_tool_text(a, back),
                None => BUSY_TEXT.to_string(),
            };
            push_lines(screen, text);
        }
        "tools" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => a.registry().catalog(),
                None => BUSY_TEXT.to_string(),
            };
            push_lines(screen, text);
        }
        "new" | "clear" => {
            let topic = if verb_key == "new" && !arg.is_empty() {
                Some(arg)
            } else {
                None
            };
            let mut guard = slot.lock().await;
            let Some(a) = guard.as_mut() else {
                push_lines(screen, BUSY_TEXT);
                return;
            };
            let (name, note) = repl::start_new_session(a, topic);
            screen.est_tokens = a.history.estimate_tokens();
            sess.session_name = name.clone();
            screen.session_name = name;
            push_lines(
                screen,
                if verb == "clear" {
                    format!("history cleared — {note}")
                } else {
                    note
                },
            );
        }
        "compact" => {
            let force = match arg {
                "" => false,
                "force" | "--force" | "f" => true,
                _ => {
                    push_lines(screen, repl::usage_for("/compact"));
                    return;
                }
            };
            let mut guard = slot.lock().await;
            let text = match guard.as_mut() {
                Some(a) => {
                    let text = repl::compact_text(a, force).await;
                    // the status bar's ctx estimate must not go stale
                    screen.est_tokens = a.history.estimate_tokens();
                    text
                }
                None => BUSY_TEXT.to_string(),
            };
            drop(guard);
            push_lines(screen, text);
        }
        "resume" => {
            if arg.is_empty() {
                push_lines(screen, repl::sessions_text());
            } else {
                let mut guard = slot.lock().await;
                let text = match guard.as_mut() {
                    Some(a) => {
                        // resolve before printing: auto-save must follow the
                        // session we just loaded, not the one this TUI started in
                        let (name, text) = repl::resume_text(a, arg).await;
                        if let Some(name) = name {
                            sess.session_name = name.clone();
                            screen.session_name = name;
                        }
                        screen.est_tokens = a.history.estimate_tokens();
                        replay_history(screen, a);
                        text
                    }
                    None => BUSY_TEXT.to_string(),
                };
                drop(guard);
                push_lines(screen, text);
            }
        }
        "provider" => {
            if arg.is_empty() {
                let nicks = sess.cfg.nicks();
                if nicks.is_empty() {
                    push_lines(screen, repl::providers_text(&sess.cfg, None));
                } else {
                    let current = sess
                        .provider
                        .clone()
                        .or_else(|| sess.cfg.resolve(None, None).ok().map(|r| r.nick));
                    screen.picker = Some(Picker::new(
                        PickerKind::Provider,
                        "select provider",
                        nicks,
                        current.as_deref(),
                    ));
                }
            } else {
                // provider-only switch: the model override is dropped (it may
                // not exist on the new wire) — /model picks both in one step
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
                    switch_tx,
                )
                .await;
            }
        }
        "model" => {
            if arg.is_empty() {
                let nick = sess
                    .provider
                    .clone()
                    .or_else(|| sess.cfg.resolve(None, None).ok().map(|r| r.nick));
                match nick {
                    Some(nick) => {
                        let cur = sess.model.clone().or_else(|| {
                            sess.cfg
                                .resolve(sess.provider.as_deref(), None)
                                .ok()
                                .map(|r| r.settings.model)
                        });
                        // ONE list for provider AND model: a row is `nick/model`,
                        // so a single selection can switch both
                        let choices =
                            repl::all_model_choices(&sess.cfg, Some(&nick), cur.as_deref()).await;
                        if choices.is_empty() {
                            push_lines(
                                screen,
                                format!(
                                    "no model candidates — use /model <name> ({})",
                                    repl::usage_for("/model")
                                ),
                            );
                        } else {
                            let current = cur.as_deref().map(|m| format!("{nick}/{m}"));
                            screen.picker = Some(Picker::new(
                                PickerKind::Model,
                                "provider / model",
                                choices,
                                current.as_deref(),
                            ));
                        }
                    }
                    None => push_lines(
                        screen,
                        "no provider resolved — check config (`kazi providers`)",
                    ),
                }
            } else if arg.chars().all(|c| c.is_ascii_digit()) {
                // a bare number indexes the combined list the picker shows
                let nick = sess
                    .provider
                    .clone()
                    .or_else(|| sess.cfg.resolve(None, None).ok().map(|r| r.nick));
                let cur = sess.model.clone().or_else(|| {
                    sess.cfg
                        .resolve(sess.provider.as_deref(), None)
                        .ok()
                        .map(|r| r.settings.model)
                });
                let choices =
                    repl::all_model_choices(&sess.cfg, nick.as_deref(), cur.as_deref()).await;
                match repl::resolve_model_arg(arg, &choices) {
                    Ok(chosen) => {
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
                            switch_tx,
                        )
                        .await;
                    }
                    Err(msg) => push_lines(screen, msg),
                }
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
                    switch_tx,
                )
                .await;
            }
        }
        "permission" => {
            let requested = if arg.is_empty() {
                None
            } else {
                Mode::parse(arg)
            };
            match requested {
                Some(m) => {
                    permission.set(m);
                    screen.mode = m;
                    push_lines(
                        screen,
                        format!("permission mode -> {} ({})", m.as_str(), mode_note(m)),
                    );
                }
                None if !arg.is_empty() => push_lines(screen, repl::usage_for("/permission")),
                None => push_lines(
                    screen,
                    format!(
                        "permission mode: {} ({})",
                        permission.get().as_str(),
                        mode_note(permission.get())
                    ),
                ),
            }
        }
        "yolo" => {
            let next = match arg {
                "" => {
                    if permission.get() == Mode::Yolo {
                        Mode::Edit
                    } else {
                        Mode::Yolo
                    }
                }
                "on" | "1" | "true" => Mode::Yolo,
                "off" | "0" | "false" => Mode::Edit,
                _ => {
                    push_lines(screen, repl::usage_for("/yolo"));
                    return;
                }
            };
            permission.set(next);
            screen.mode = next;
            push_lines(
                screen,
                format!("permission mode: {} ({})", next.as_str(), mode_note(next)),
            );
        }
        "exit" | "quit" => {
            // the same as Esc when idle: save happens on the way out
            let guard = slot.lock().await;
            if let Some(a) = guard.as_ref() {
                save(&sess.session_name, a);
            }
            drop(guard);
            screen.quit = true;
        }
        other => push_lines(screen, repl::unknown_command_text(&format!("/{other}"))),
    }
}

fn mode_note(mode: crate::tools::PermissionMode) -> &'static str {
    match mode {
        crate::tools::PermissionMode::ReadOnly => "inspect only",
        crate::tools::PermissionMode::Edit => "writes allowed, commands gated",
        crate::tools::PermissionMode::Yolo => "no prompts",
    }
}

/// Kick off a provider/model switch in the BACKGROUND: the event loop keeps
/// drawing (spinner + elapsed) while MCP/HTTP work happens, and the rebuilt
/// agent arrives over `switch_tx` with the conversation carried along.
/// `probe` verifies the model with one tiny round before the switch commits.
#[allow(clippy::too_many_arguments)]
async fn start_switch(
    screen: &mut Screen,
    slot: &std::sync::Arc<tokio::sync::Mutex<Option<Agent>>>,
    sess: &SessionState,
    ask_tx: &tokio::sync::mpsc::UnboundedSender<ApprovalAsk>,
    session_allows: &SessionAllows,
    permission: &crate::tools::Permission,
    provider: Option<String>,
    model: Option<String>,
    probe: bool,
    switch_tx: &tokio::sync::mpsc::UnboundedSender<SwitchDone>,
) {
    let Some(old) = slot.lock().await.take() else {
        push_lines(screen, BUSY_TEXT);
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
    screen.push(LineKind::Dim, format!("switching -> {label} …"));

    let cfg = sess.cfg.clone();
    let approver: std::sync::Arc<dyn Approver> = std::sync::Arc::new(TuiApprover::with_allows(
        ask_tx.clone(),
        session_allows.clone(),
    ));
    let mode = permission.get();
    let tx = switch_tx.clone();
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
                        let _ = tx.send(SwitchDone::Failed {
                            agent: Box::new(old),
                            note,
                        });
                        return;
                    }
                }
                let desc = agent.provider().describe();
                // a switch changes the wire, not the conversation
                agent.history =
                    std::mem::replace(&mut old.history, super::context::History::new(None));
                let _ = tx.send(SwitchDone::Ok {
                    agent: Box::new(agent),
                    provider,
                    model,
                    desc,
                });
            }
            Err(e) => {
                let _ = tx.send(SwitchDone::Failed {
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

/// Apply a completed picker choice. The model list is combined (`nick/model`),
/// so one selection can switch provider AND model together; a provider row
/// switches the wire and keeps the conversation.
#[allow(clippy::too_many_arguments)]
async fn apply_selection(
    screen: &mut Screen,
    slot: &std::sync::Arc<tokio::sync::Mutex<Option<Agent>>>,
    sess: &SessionState,
    ask_tx: &tokio::sync::mpsc::UnboundedSender<ApprovalAsk>,
    session_allows: &SessionAllows,
    permission: &crate::tools::Permission,
    switch_tx: &tokio::sync::mpsc::UnboundedSender<SwitchDone>,
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
                switch_tx,
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
                switch_tx,
            )
            .await;
        }
    }
}

/// Inner loop: agent tasks run in a spawned tokio task and stream their
/// events through an mpsc channel; the UI loop `select!`s between terminal
/// input and incoming events, so the screen keeps redrawing (streaming
/// deltas land live) and Esc stays responsive even while a task runs.
/// Every finished task is autosaved; quitting mid-run cancels the task and
/// waits briefly for the agent to come back before the final save.
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
    switches: &mut tokio::sync::mpsc::UnboundedReceiver<SwitchDone>,
    switch_tx: &tokio::sync::mpsc::UnboundedSender<SwitchDone>,
) -> anyhow::Result<Option<Agent>> {
    // Agent handoff: the runner task owns the agent while a task runs (the
    // engine needs `&mut`), then parks it back here for the next task and
    // for the final save. The slot is `None` only DURING a run; if the user
    // aborts mid-task (Esc) the runner is dropped together with the agent and
    // the slot stays empty — the caller then skips the final save.
    let shared_agent: std::sync::Arc<tokio::sync::Mutex<Option<Agent>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(Some(agent)));

    enum Pump {
        Ev(AgentEvent),
        Done(Result<String, super::AgentError>),
    }
    // Unbounded on purpose: the producer is the agent callback, which runs
    // synchronously inside the runner's runtime thread — an async `send`
    // cannot be awaited there and `blocking_send` would panic ("cannot block
    // the current thread from within a runtime"). A per-task event stream is
    // bounded by the answer length, so unbounded is safe here.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Pump>();
    let mut runner: Option<tokio::task::JoinHandle<()>> = None;
    // Cancellation handle of the in-flight task (Esc mid-run sets it).
    let mut current_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = None;

    // crossterm's async event stream (event-stream feature)
    let mut events = EventStream::new();
    // heartbeat: keeps the spinner turning and the elapsed time honest while a
    // task runs (a quietly thinking model used to look frozen)
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(200));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    while !screen.quit {
        let _ = terminal.draw(|f| screen.draw(f));

        tokio::select! {
            // ---- heartbeat (only while a task runs: an idle TUI sleeps) ------
            _ = ticker.tick(), if screen.busy => {
                screen.spinner.set(screen.spinner.get().wrapping_add(1));
            }

            // ---- a provider/model switch landed ----------------------------
            Some(done) = switches.recv() => {
                screen.busy = false;
                screen.started.set(None);
                match done {
                    SwitchDone::Ok { agent, provider, model, desc } => {
                        *shared_agent.lock().await = Some(*agent);
                        sess.provider = provider;
                        sess.model = model;
                        screen.provider_desc = desc.clone();
                        push_lines(screen, format!("switched -> {desc}"));
                    }
                    SwitchDone::Failed { agent, note } => {
                        *shared_agent.lock().await = Some(*agent);
                        push_lines(screen, note);
                    }
                }
                screen.scroll_to_bottom();
            }

            // ---- approval requests from the agent --------------------------
            ask = approvals.recv() => {
                if let Some(ask) = ask {
                    // echo the full command first: the modal may have to clip a
                    // very long one, and the transcript is the audit trail
                    for line in ask.summary.lines() {
                        screen.push(LineKind::Warn, line.to_string());
                    }
                    screen.pending_approval = Some(ask);
                }
            }

            // ---- agent pump ------------------------------------------------
            maybe = rx.recv(), if runner.is_some() => match maybe {
                Some(Pump::Ev(ev)) => screen.consume(ev),
                Some(Pump::Done(res)) => {
                    screen.flush_streaming();
                    // agent is already parked back by the runner
                    match res {
                        Err(super::AgentError::Cancelled) => {
                            screen.push(LineKind::Dim, "(cancelled)".to_string());
                        }
                        Err(e) => {
                            // auth-ish failures get the exact fixing command
                            let text = shared_agent
                                .lock()
                                .await
                                .as_ref()
                                .map(|a| a.error_text(&e))
                                .unwrap_or_else(|| e.to_string());
                            screen.push(LineKind::Warn, format!("error: {text}"));
                        }
                        Ok(_) => {}
                    }
                    screen.busy = false;
                    screen.started.set(None);
                    runner = None;
                    current_cancel = None;
                    // autosave after EVERY finished task, like the line REPL:
                    // one Ctrl+C used to throw the whole session away
                    if let Some(agent) = shared_agent.lock().await.as_ref() {
                        on_session_save(&sess.session_name, agent);
                    }
                }
                None => { screen.busy = false; runner = None; current_cancel = None; }
            },

            // ---- input pump (async EventStream) ----------------------------
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(CtEvent::Key(key))) if key.kind != KeyEventKind::Release => {
                    log_key_event(&key);
                    // a modal picker owns the keyboard while it is open
                    if screen.picker.is_some() {
                        let mut close = false;
                        let mut accept: Option<(PickerKind, String)> = None;
                        if let Some(p) = screen.picker.as_mut() {
                            match key.code {
                                KeyCode::Up => p.move_cursor(-1),
                                KeyCode::Down => p.move_cursor(1),
                                KeyCode::Backspace => {
                                    p.filter.pop();
                                    p.cursor = 0;
                                }
                                KeyCode::Esc => close = true,
                                KeyCode::Enter => {
                                    accept = p.selected().map(|s| (p.kind, s));
                                }
                                code => {
                                    if let Some(ch) = accepted_char(&code, key.modifiers) {
                                        p.filter.push(ch);
                                        p.cursor = 0;
                                    }
                                }
                            }
                        }
                        if close {
                            screen.picker = None;
                        }
                        if let Some((kind, chosen)) = accept {
                            screen.picker = None;
                            apply_selection(
                                screen,
                                &shared_agent,
                                sess,
                                ask_tx,
                                session_allows,
                                permission,
                                switch_tx,
                                kind,
                                chosen,
                            )
                            .await;
                        }
                        continue;
                    }
                    // an approval is modal: only y/a/n/Esc answer it. Every
                    // other key is ignored, so typing the next task can never
                    // grant an approval by accident (Enter used to = allow).
                    if let Some(pending) = screen.pending_approval.take() {
                        let decision = match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => Some(Decision::Allow),
                            KeyCode::Char('a') | KeyCode::Char('A') => {
                                Some(Decision::AllowSession)
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                Some(Decision::Deny(None))
                            }
                            _ => None,
                        };
                        match decision {
                            Some(decision) => {
                                let verdict = match decision {
                                    Decision::Allow => "allowed",
                                    Decision::AllowSession => "allowed (always, this command)",
                                    Decision::Deny(_) => "denied",
                                };
                                let _ = pending.reply.send(decision);
                                screen.push(LineKind::Dim, format!("approval: {verdict}"));
                            }
                            None => screen.pending_approval = Some(pending),
                        }
                        continue;
                    }
                    match key.code {
                    KeyCode::Esc => {
                        if screen.busy {
                            // interrupt the running task, keep the session
                            if let Some(flag) = &current_cancel {
                                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                                screen.push(LineKind::Dim, "(cancelling…)".to_string());
                            }
                        } else {
                            screen.quit = true;
                        }
                    }
                    KeyCode::BackTab => {
                        // Shift+Tab: cycle read-only → yolo → edit
                        let next = permission.get().cycle();
                        permission.set(next);
                        screen.mode = next;
                        screen.push(
                            LineKind::Dim,
                            format!("permission mode -> {}", next.as_str()),
                        );
                    }
                    KeyCode::Enter if !screen.busy => {
                        // a half-typed command runs the unique match — or the
                        // row the dropdown is highlighting — instead of erroring
                        if let Some(complete) = screen.resolve_command_input() {
                            screen.input.set_text(complete);
                        }
                        if let Some(task) = screen.take_input() {
                            // a leading `/` is a command, never a task
                            if let Some(cmdline) = task.strip_prefix('/') {
                                run_command(
                                    cmdline.trim(),
                                    screen,
                                    &shared_agent,
                                    sess,
                                    ask_tx,
                                    session_allows,
                                    permission,
                                    switch_tx,
                                    on_session_save,
                                )
                                .await;
                                screen.scroll_to_bottom();
                                continue;
                            }
                            screen.busy = true;
                            screen.started.set(Some(std::time::Instant::now()));
                            screen.spinner.set(0);
                            screen.scroll_to_bottom(); // new work pulls the view down
                            // take agent out of the slot for the runner
                            let taken = shared_agent.lock().await.take();
                            if let Some(owned) = taken {
                                current_cancel = Some(owned.cancel_token());
                                let expanded = super::expand_file_refs(&task, &owned.ctx.cwd);
                                let slot = shared_agent.clone();
                                let tx = tx.clone();
                                runner = Some(tokio::spawn(async move {
                                    let mut owned = owned;
                                    let out = owned.run_task(&expanded, |ev| {
                                        let _ = tx.send(Pump::Ev(ev));
                                    }).await;
                                    // park the agent back first, THEN report
                                    slot.lock().await.replace(owned);
                                    let _ = tx.send(Pump::Done(out));
                                }));
                            }
                        }
                    }
                    KeyCode::Enter => {
                        // busy: no task can start now — say so instead of
                        // swallowing the keystroke silently
                        screen.push(LineKind::Dim, BUSY_TEXT.to_string());
                    }
                    KeyCode::Tab => {
                        // complete the command token; a unique match opens its
                        // argument position (trailing space)
                        let token = super::repl::pending_command(&screen.input.text)
                            .map(|t| t.to_string());
                        if let Some(token) = token {
                            let hints = super::repl::command_rows(&token);
                            if hints.len() == 1 {
                                screen.input.set_text(format!("{token} "));
                            } else if let Some(done) = super::repl::command_completion(&token) {
                                screen.input.set_text(done);
                            } else if hints.len() > 1 {
                                // no common prefix to add: walk the candidates
                                screen.move_command_sel(1);
                            }
                        } else {
                            // `@path` completion for file attachments
                            let pos = screen.input.byte_of(screen.input.cursor);
                            if let Some(done) =
                                super::repl::complete_at_path(&screen.input.text, pos)
                            {
                                screen.input.set_text(done);
                            }
                        }
                    }
                    KeyCode::Backspace => screen.input.backspace(),
                    KeyCode::Delete => screen.input.delete(),
                    KeyCode::Left => screen.input.left(),
                    KeyCode::Right => screen.input.right(),
                    KeyCode::Home
                        if key
                            .modifiers
                            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        screen.scroll_to_top()
                    }
                    KeyCode::End
                        if key
                            .modifiers
                            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
                    {
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
                        // the dropdown is the most visible affordance: ↑/↓ drive
                        // it, then multi-line navigation, then history
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
                    KeyCode::Char(c)
                        if key
                            .modifiers
                            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        match c {
                            'c' => screen.quit = true,
                            'j' => screen.input.newline(), // Ctrl+J: new line in the task
                            'l' => screen.clear_transcript(),
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
                    _ => {}
                },
                // bracketed paste: insert the block whole. Without this a
                // multi-line paste fired its first line as a task.
                Some(Ok(CtEvent::Paste(text))) => {
                    if screen.pending_approval.is_none() && screen.picker.is_none() {
                        screen.input.insert_str(&text);
                    }
                }
                Some(Ok(_)) => {} // resize/focus/mouse: next draw picks size up
                Some(Err(_)) | None => {} // event loop hiccup: keep going
            },
        }
    }

    // Quitting with a task in flight: ask it to stop and give the runner a
    // moment to park the agent back, so the session is saved instead of lost.
    if let Some(flag) = &current_cancel {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(handle) = runner.take() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }
    // hand the agent back to the caller (None only when the task outlived the
    // wait above; every earlier task was already autosaved when it finished)
    let returned = shared_agent.lock().await.take();
    if let Some(agent) = &returned {
        on_session_save(&sess.session_name, agent);
    }
    Ok(returned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// Input pre-filled with `s`, caret at the end.
    fn input_with(s: &str) -> Input {
        Input {
            text: s.to_string(),
            cursor: s.chars().count(),
            ..Default::default()
        }
    }

    #[test]
    fn wrap_breaks_long_words_and_joins_short() {
        let long = "a".repeat(80);
        let wrapped = wrap_text(&long, 30);
        assert!(wrapped.iter().all(|w| w.len() <= 30));
        assert_eq!(wrapped.join(""), long);

        let joined = wrap_text("one two three", 20);
        assert_eq!(joined, vec!["one two three"]);
    }

    #[test]
    fn tool_placeholder_replaced_by_verdict() {
        let mut s = Screen::new("p", "sess", 30);
        s.consume(AgentEvent::ToolStart {
            name: "read_file".into(),
        });
        assert!(s.lines.last().unwrap().1.contains(">> read_file ..."));
        s.consume(AgentEvent::ToolEnd {
            name: "read_file".into(),
            is_error: false,
            preview: "3 lines".into(),
        });
        assert!(s.lines.last().unwrap().1.contains("[ok] 3 lines"));
    }

    #[test]
    fn rounddone_updates_stats() {
        let mut s = Screen::new("p", "sess", 30);
        s.consume(AgentEvent::RoundDone {
            round: 2,
            usage: crate::llm::Usage {
                input_tokens: Some(120),
                output_tokens: Some(30),
            },
            est_tokens: 999,
        });
        assert_eq!(s.rounds, 2);
        assert_eq!(s.tokens_in, 120);
        assert_eq!(s.tokens_out, 30);
        assert_eq!(s.est_tokens, 999);
    }

    #[test]
    fn take_input_trims_and_clears() {
        let mut s = Screen::new("p", "sess", 30);
        s.input.text = "  hello  ".into();
        s.input.end();
        let task = s.take_input().unwrap();
        assert_eq!(task, "hello");
        assert!(s.input.text.is_empty());
        assert_eq!(s.input.cursor, 0);
        assert!(s.lines.last().unwrap().1.contains("you> hello"));
        assert!(s.take_input().is_none(), "empty input = no task");
    }

    #[test]
    fn input_editing_moves_the_caret_and_edits_mid_text() {
        let mut i = Input::default();
        for c in "hello".chars() {
            i.insert(c);
        }
        i.left();
        i.left();
        i.insert('X'); // "helXlo"
        assert_eq!(i.text, "helXlo");
        assert_eq!(i.cursor, 4);
        i.backspace(); // "hello"
        assert_eq!(i.text, "hello");
        i.home();
        i.delete(); // "ello"
        assert_eq!(i.text, "ello");
        i.end();
        i.insert('!');
        assert_eq!(i.text, "ello!");
        i.left();
        i.kill_to_end(); // drops "!"
        assert_eq!(i.text, "ello");
        i.kill_to_start();
        assert_eq!(i.text, "");
    }

    #[test]
    fn input_cjk_caret_is_char_indexed() {
        let mut i = Input::default();
        for c in "你好".chars() {
            i.insert(c);
        }
        i.left();
        i.backspace(); // deletes 你 → "好"
        assert_eq!(i.text, "好");
        assert_eq!(i.cursor, 0);
        i.insert('a'); // "a好"
        assert_eq!(i.text, "a好");
    }

    #[test]
    fn input_delete_word_stops_at_whitespace() {
        let mut i = Input::default();
        for c in "cargo test --lib".chars() {
            i.insert(c);
        }
        i.delete_word(); // drop "--lib"
        assert_eq!(i.text, "cargo test ");
        i.delete_word(); // drop "test" + trailing space
        assert_eq!(i.text, "cargo ");
    }

    #[test]
    fn input_history_recall_roundtrip() {
        let mut i = input_with("first");
        i.submit();
        i.text = "second".into();
        i.end();
        i.submit();

        i.history_prev();
        assert_eq!(i.text, "second");
        i.history_prev();
        assert_eq!(i.text, "first");
        i.history_prev(); // already oldest: stays
        assert_eq!(i.text, "first");
        i.history_next();
        assert_eq!(i.text, "second");
        i.history_next(); // back to the (empty) live draft
        assert_eq!(i.text, "");
    }

    #[test]
    fn input_draft_survives_history_browsing() {
        let mut i = input_with("old");
        i.submit();
        i.text = "half-typed".into();
        i.end();
        i.history_prev();
        assert_eq!(i.text, "old");
        i.history_next();
        assert_eq!(i.text, "half-typed", "draft restored after browsing");
    }

    #[test]
    fn window_keeps_caret_visible() {
        // caret at the end of a long line: window shows the tail
        let (win, caret) = window_for_cursor("abcdefghij", 10, 4);
        assert_eq!(win, "ghij");
        assert_eq!(caret, 4);
        // caret in the middle: window recentred to include it
        let (win, caret) = window_for_cursor("你好世界你好世界", 4, 6);
        assert!(UnicodeWidthStr::width(win.as_str()) <= 6);
        assert!(caret <= 6);
        // whole string when it fits
        let (win, caret) = window_for_cursor("hi", 1, 10);
        assert_eq!(win, "hi");
        assert_eq!(caret, 1);
    }

    /// The TUI runner's event callback runs synchronously inside a runtime
    /// thread, so its send must not block. This pins the unbounded-channel
    /// pattern that replaced `blocking_send` (which panicked, v0.10.2).
    #[tokio::test]
    async fn event_callback_sends_without_blocking() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u8>();
        tokio::spawn(async move {
            let callback = |v: u8| {
                tx.send(v).expect("receiver alive");
            };
            callback(7);
        })
        .await
        .unwrap();
        assert_eq!(rx.recv().await, Some(7));
    }

    /// CJK text has no spaces: wrapping must go by display width and cut at
    /// glyph boundaries (the old byte slicing panicked on multi-byte chars).
    #[test]
    fn wrap_handles_cjk_by_display_width() {
        let text = "你好世界这是一段没有空格的中文长文本用于测试换行";
        let lines = wrap_text(text, 10);
        assert!(
            lines
                .iter()
                .all(|l| UnicodeWidthStr::width(l.as_str()) <= 10),
            "width overflow: {lines:?}"
        );
        assert_eq!(lines.join(""), text, "wrapping must not lose characters");
    }

    #[test]
    fn scroll_window_follows_and_clamps() {
        // following the tail
        assert_eq!(scroll_range(100, 10, 0), (90, 100, 0));
        // five lines up
        assert_eq!(scroll_range(100, 10, 5), (85, 95, 5));
        // overshoot clamps to the top
        assert_eq!(scroll_range(100, 10, 999), (0, 10, 90));
        // fewer lines than the viewport
        assert_eq!(scroll_range(3, 10, 0), (0, 3, 0));
    }

    /// The rendered frame as one string per screen row.
    fn frame_rows(term: &Terminal<TestBackend>) -> Vec<String> {
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn command_dropdown_renders_as_overlay() {
        let mut s = Screen::new("deepseek", "sess", 10);
        for _ in 0..8 {
            s.push(LineKind::Dim, "TRANSCRIPT_MARKER".to_string());
        }
        s.input.set_text("/u".into());
        let mut term = Terminal::new(TestBackend::new(64, 12)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let rows = frame_rows(&term);

        assert!(
            rows.iter().any(|r| r.contains("TRANSCRIPT_MARKER")),
            "the menu must not steal transcript rows: {rows:#?}"
        );
        let menu: Vec<&String> = rows.iter().filter(|r| r.contains("/undo")).collect();
        assert_eq!(menu.len(), 1, "exactly one /undo row: {rows:#?}");
        assert!(
            menu[0].starts_with(" /undo"),
            "menu row must start at the left edge: {:?}",
            menu[0]
        );
        assert!(
            rows.iter().any(|r| r.contains("restore files")),
            "help column missing: {rows:#?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("Tab completes")),
            "footer missing: {rows:#?}"
        );
        assert!(
            !rows
                .iter()
                .any(|r| r.contains("TRANSCRIPT") && r.contains("/undo")),
            "the panel must not leave fragments of the lines it covers: {rows:#?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("kazi> /u")),
            "input row untouched: {rows:#?}"
        );
    }

    #[test]
    fn command_hints_track_command_position() {
        let mut s = Screen::new("prov", "sess", 10);
        assert!(s.command_hints().is_empty(), "no slash, no dropdown");

        s.input.set_text("/".into());
        assert!(s.command_hints().len() >= 10, "`/` lists the table");

        s.input.set_text("/undo".into());
        assert_eq!(s.command_hints().len(), 1, "narrowed to one");

        s.input.set_text("/undo ".into());
        assert!(s.command_hints().is_empty(), "arguments close the dropdown");

        s.input.set_text("please /undo".into());
        assert!(
            s.command_hints().is_empty(),
            "a slash mid-line is not a command"
        );
    }

    #[test]
    fn picker_filters_moves_and_selects() {
        let mut p = Picker::new(
            PickerKind::Model,
            "select model",
            vec![
                "alpha".to_string(),
                "beta".to_string(),
                "alpine".to_string(),
            ],
            Some("beta"),
        );
        // cursor parks on the current value
        assert_eq!(p.selected().as_deref(), Some("beta"));
        // case-insensitive substring filter
        p.filter = "AL".to_string();
        let names: Vec<&str> = p.filtered().into_iter().map(String::as_str).collect();
        assert_eq!(names, vec!["alpha", "alpine"]);
        // cursor follows the filtered list and clamps at both ends
        p.cursor = 0;
        p.move_cursor(1);
        assert_eq!(p.selected().as_deref(), Some("alpine"));
        p.move_cursor(9);
        assert_eq!(p.selected().as_deref(), Some("alpine"), "clamped at end");
        p.move_cursor(-9);
        assert_eq!(p.selected().as_deref(), Some("alpha"), "clamped at start");
        // no matches: nothing selected, moving is a no-op (no panic)
        p.filter = "zzz".to_string();
        assert!(p.filtered().is_empty());
        assert_eq!(p.selected(), None);
        p.move_cursor(1);
        assert_eq!(p.selected(), None);
    }

    #[test]
    fn picker_renders_as_overlay() {
        let mut s = Screen::new("deepseek", "sess", 10);
        for _ in 0..8 {
            s.push(LineKind::Dim, "TRANSCRIPT_MARKER".to_string());
        }
        s.picker = Some(Picker::new(
            PickerKind::Model,
            "select model",
            vec!["m1".to_string(), "m2".to_string()],
            Some("m2"),
        ));
        let mut term = Terminal::new(TestBackend::new(64, 14)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let rows = frame_rows(&term);
        assert!(
            rows.iter().any(|r| r.contains("select model")),
            "title missing: {rows:#?}"
        );
        assert!(rows.iter().any(|r| r.contains("m1")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("m2")), "{rows:#?}");
        assert!(
            rows.iter().any(|r| r.contains("filter:")),
            "footer missing: {rows:#?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("TRANSCRIPT_MARKER")),
            "the picker must not eat the whole transcript: {rows:#?}"
        );
    }

    #[test]
    fn modifier_combos_are_not_text() {
        use ratatui::crossterm::event::KeyModifiers as M;
        // plain typing, incl. shifted punctuation
        assert_eq!(accepted_char(&KeyCode::Char('a'), M::NONE), Some('a'));
        assert_eq!(accepted_char(&KeyCode::Char(' '), M::NONE), Some(' '));
        assert_eq!(accepted_char(&KeyCode::Char('!'), M::SHIFT), Some('!'));
        assert_eq!(accepted_char(&KeyCode::Char('中'), M::NONE), Some('中'));
        // IME switches / shortcuts must never insert text
        assert_eq!(accepted_char(&KeyCode::Char(' '), M::CONTROL), None);
        assert_eq!(accepted_char(&KeyCode::Char(' '), M::SHIFT), None);
        assert_eq!(accepted_char(&KeyCode::Char('a'), M::CONTROL), None);
        assert_eq!(accepted_char(&KeyCode::Char('\0'), M::NONE), None);
    }

    #[test]
    fn altgr_characters_are_text() {
        use ratatui::crossterm::event::KeyModifiers as M;
        // AltGr arrives as CONTROL|ALT on Windows/X11 layouts: `@ \ [ ] { } |`
        // must reach the input instead of vanishing
        assert_eq!(
            accepted_char(&KeyCode::Char('@'), M::CONTROL | M::ALT),
            Some('@')
        );
        assert_eq!(
            accepted_char(&KeyCode::Char('\\'), M::CONTROL | M::ALT),
            Some('\\')
        );
        // plain Ctrl combos stay shortcuts
        assert_eq!(accepted_char(&KeyCode::Char('a'), M::CONTROL), None);
    }

    #[test]
    fn paste_keeps_line_breaks_and_submits_as_one_task() {
        let mut i = Input::default();
        i.insert_str("line one\r\nline two\nline three");
        assert_eq!(i.text, "line one\nline two\nline three");
        assert_eq!(i.cursor, i.chars_len());
        // one task, submitted on purpose — not at the first newline
        let task = i.submit().expect("the whole paste is one task");
        assert!(task.contains("line two"), "got: {task}");
    }

    #[test]
    fn completed_command_input_follows_the_dropdown() {
        let mut s = Screen::new("p", "sess", 10);
        s.input.set_text("/".into());
        let rows = crate::agent::repl::command_rows("/");
        assert!(rows.len() > 1);
        let word = |row: &(String, String)| row.0.split_whitespace().next().unwrap().to_string();

        // Enter runs the first row while the highlight sits there
        assert_eq!(s.resolve_command_input(), Some(word(&rows[0])));
        s.move_command_sel(1);
        assert_eq!(s.resolve_command_input(), Some(word(&rows[1])));
        // and wraps around
        s.move_command_sel(-1);
        assert_eq!(s.resolve_command_input(), Some(word(&rows[0])));

        // a unique match needs no selection
        s.input.set_text("/undo".into());
        assert_eq!(s.resolve_command_input().as_deref(), Some("/undo"));
        // arguments take the line out of command position
        s.input.set_text("/model gpt".into());
        assert_eq!(s.resolve_command_input(), None);
    }

    #[test]
    fn model_choice_rows_split_into_provider_and_model() {
        let cfg: crate::config::Config = toml::from_str(
            "[providers.a]\ntype = \"openai\"\nmodel = \"m1\"\n[providers.b]\ntype = \"openai\"\nmodel = \"m2\"",
        )
        .unwrap();
        assert_eq!(
            split_model_choice(&cfg, Some("a"), "b/m2"),
            (Some("b".to_string()), "m2".to_string())
        );
        // a bare name stays on the current provider
        assert_eq!(
            split_model_choice(&cfg, Some("a"), "gpt-4o"),
            (Some("a".to_string()), "gpt-4o".to_string())
        );
        // an unknown prefix is not a provider switch
        assert_eq!(
            split_model_choice(&cfg, Some("a"), "nope/model"),
            (Some("a".to_string()), "nope/model".to_string())
        );
    }

    #[test]
    fn multiline_input_navigates_its_own_lines() {
        let mut i = Input::default();
        i.insert_str("first line\nsecond\nthird");
        assert_eq!(i.line_count(), 3);
        assert_eq!(i.caret_line_index(), 2, "caret starts on the last line");

        i.up();
        assert_eq!(i.caret_line_index(), 1);
        assert_eq!(i.caret_col(), 5, "column kept across lines");
        i.up();
        assert_eq!(i.caret_line_index(), 0);
        i.up();
        assert_eq!(i.caret_line_index(), 0, "clamped at the first line");

        i.down();
        i.down();
        assert_eq!(i.caret_line_index(), 2);
        i.down();
        assert_eq!(i.caret_line_index(), 2, "clamped at the last line");

        i.home_line();
        assert_eq!(i.caret_col(), 0);
        i.end_line();
        assert_eq!(i.caret_col(), "third".chars().count());

        let task = i.submit().expect("multi-line task submits whole");
        assert_eq!(task, "first line\nsecond\nthird");
    }

    #[test]
    fn newline_inserts_and_line_count_tracks() {
        let mut i = Input::default();
        i.insert_str("a");
        i.newline();
        i.insert_str("b");
        assert_eq!(i.text, "a\nb");
        assert_eq!(i.line_count(), 2);
        assert_eq!(i.caret_line_index(), 1);
        i.backspace(); // 'b'
        i.backspace(); // the newline itself
        assert_eq!(i.text, "a");
        assert_eq!(i.line_count(), 1);
    }

    #[test]
    fn multiline_input_renders_every_row() {
        let mut s = Screen::new("deepseek", "sess", 10);
        s.input.insert_str("alpha\nbeta");
        let mut term = Terminal::new(TestBackend::new(48, 12)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let rows = frame_rows(&term);
        assert!(rows.iter().any(|r| r.contains("kazi> alpha")), "{rows:#?}");
        assert!(
            rows.iter().any(|r| r.contains(" beta")),
            "the second line needs its own row: {rows:#?}"
        );
    }

    #[test]
    fn busy_status_shows_mode_spinner_and_elapsed() {
        let mut s = Screen::new("deepseek", "sess", 10);
        s.busy = true;
        s.started.set(Some(std::time::Instant::now()));
        let mut term = Terminal::new(TestBackend::new(80, 12)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let status = frame_rows(&term).into_iter().next().unwrap();
        assert!(status.contains("[edit]"), "mode tag first: {status}");
        assert!(status.contains("0s"), "elapsed time shown: {status}");
    }

    #[test]
    fn scrolled_view_stays_put_while_output_grows() {
        let mut s = Screen::new("p", "sess", 10);
        for i in 0..40 {
            s.push(LineKind::Dim, format!("line {i}"));
        }
        let mut term = Terminal::new(TestBackend::new(40, 12)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let first_row = |term: &Terminal<TestBackend>| {
            frame_rows(term)
                .into_iter()
                .find(|r| r.contains("line "))
                .unwrap_or_default()
        };

        s.scroll_up(5);
        term.draw(|f| s.draw(f)).unwrap();
        let before = first_row(&term);
        assert!(!before.is_empty());

        // new output arrives while the reader is scrolled back
        s.push(LineKind::Dim, "line 40".to_string());
        term.draw(|f| s.draw(f)).unwrap();
        assert_eq!(
            first_row(&term),
            before,
            "the viewport must not slide under the reader"
        );
    }

    #[test]
    fn transcript_is_bounded() {
        let mut s = Screen::new("p", "sess", 10);
        for i in 0..(Screen::MAX_LINES + 50) {
            s.push(LineKind::Dim, format!("l{i}"));
        }
        assert_eq!(s.lines.len(), Screen::MAX_LINES);
        assert!(
            s.lines.first().unwrap().1.starts_with("l50"),
            "oldest rows are dropped first"
        );
    }

    #[tokio::test]
    async fn tui_approver_round_trips_and_remembers() {
        use std::sync::Arc;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalAsk>();
        let approver = Arc::new(TuiApprover::new(tx));

        // first ask: agent side waits, UI side answers "always"
        let a1 = approver.clone();
        let task1 = tokio::spawn(async move { a1.decide("run_command", "$ git status").await });
        let ask = rx.recv().await.expect("ask delivered");
        assert!(ask.summary.contains("git status"));
        ask.reply.send(Decision::AllowSession).expect("ui replies");
        assert_eq!(task1.await.unwrap(), Decision::Allow);

        // the exact same command afterwards: no question, straight allow
        assert_eq!(
            approver.decide("run_command", "$ git status").await,
            Decision::Allow
        );

        // a different command still asks — "always" is per command, not per
        // binary (approving `git status` must never clear `git push --force`)
        let a2 = approver.clone();
        let task2 =
            tokio::spawn(async move { a2.decide("run_command", "$ git log --oneline").await });
        let ask2 = rx.recv().await.expect("different command asks");
        ask2.reply.send(Decision::Deny(None)).expect("ui replies");
        assert_eq!(task2.await.unwrap(), Decision::Deny(None));
    }

    #[tokio::test]
    async fn shared_session_allows_survive_agent_rebuilds() {
        use std::sync::Arc;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalAsk>();
        let allows = SessionAllows::default();
        let first = Arc::new(TuiApprover::with_allows(tx.clone(), allows.clone()));

        let a1 = first.clone();
        let task1 = tokio::spawn(async move { a1.decide("run_command", "$ cargo test").await });
        let ask = rx.recv().await.expect("ask delivered");
        ask.reply.send(Decision::AllowSession).expect("ui replies");
        assert_eq!(task1.await.unwrap(), Decision::Allow);

        // a rebuilt agent gets a fresh approver over the SAME set
        let rebuilt = TuiApprover::with_allows(tx, allows);
        assert_eq!(
            rebuilt.decide("run_command", "$ cargo test").await,
            Decision::Allow,
            "an /model or /provider switch must not forget session approvals"
        );
    }

    /// An agent over a script-less mock provider: enough for commands that
    /// never talk to the model.
    fn stub_agent() -> Agent {
        let provider: std::sync::Arc<dyn crate::llm::Provider> =
            std::sync::Arc::new(crate::llm::mock::MockProvider::new(vec![]));
        let ctx = crate::tools::ToolCtx {
            cwd: std::env::temp_dir(),
            sandbox_root: None,
            timeout: std::time::Duration::from_secs(5),
            output_limit: 4096,
            allow_commands: vec![],
            approver: std::sync::Arc::new(crate::tools::AutoApprover),
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
        let slot = std::sync::Arc::new(tokio::sync::Mutex::new(Some(stub_agent())));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalAsk>();
        let mut sess = SessionState {
            cfg: crate::config::Config::default(),
            provider: None,
            model: None,
            session_name: "sess".to_string(),
        };
        let mut screen = Screen::new("stub-provider", "sess", 5);
        let perm = crate::tools::Permission::default();
        let save = |_: &str, _: &Agent| {};
        let allows = SessionAllows::default();
        let (switch_tx, _switch_rx) = tokio::sync::mpsc::unbounded_channel::<SwitchDone>();

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
            "compact",
            "resume",
            "definitely-not-a-command",
            "new demo",
        ] {
            run_command(
                cmd,
                &mut screen,
                &slot,
                &mut sess,
                &tx,
                &allows,
                &perm,
                &switch_tx,
                &save,
            )
            .await;
        }

        let text = screen
            .lines
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("show this help"), "help:\n{text}");
        assert!(text.contains("provider/model:"), "status:\n{text}");
        assert!(text.contains("auth — provider"), "auth:\n{text}");
        assert!(text.contains("no tool output"), "last:\n{text}");
        assert!(text.contains("context estimate"), "context:\n{text}");
        assert!(
            text.contains("permission mode -> read-only"),
            "permission set:\n{text}"
        );
        assert!(text.contains("permission mode: yolo"), "yolo:\n{text}");
        assert!(text.contains("unknown command"), "unknown:\n{text}");
        assert!(text.contains("new session -> demo"), "new:\n{text}");
        assert_eq!(
            sess.session_name, "demo",
            "session switched for later saves"
        );
        assert_eq!(perm.get(), crate::tools::PermissionMode::Yolo);
    }
}
