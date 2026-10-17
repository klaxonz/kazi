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
use transcript::{Level, SearchHit, Transcript};

/// Approval request travelling from the agent task to the UI loop.
pub struct ApprovalAsk {
    /// Monotonic id: the prompt answers *this* request, not "whatever arrived
    /// last" (two sources can queue up while the screen is busy).
    pub id: u64,
    /// 1-based task number it belongs to, shown to the user.
    pub task: u64,
    pub origin: AskOrigin,
    /// Tool that wants to run (`run_command`, ...).
    pub tool: String,
    pub summary: String,
    pub reply: tokio::sync::oneshot::Sender<Decision>,
}

/// Where an approval request came from. The main task and a `delegate`
/// sub-task used to be indistinguishable in the prompt — and a provider switch
/// could interleave their questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskOrigin {
    /// The conversation the user is looking at.
    Main,
    /// A nested `delegate` run.
    SubAgent,
}

impl AskOrigin {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Main => "task",
            Self::SubAgent => "sub-agent",
        }
    }
}

/// Ids handed out to [`ApprovalAsk`]s (one counter per process is enough).
static NEXT_ASK_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
/// Same for task numbers, so "task #7" is stable across prompts.
static NEXT_TASK_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_ask_id() -> u64 {
    NEXT_ASK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub fn next_task_id() -> u64 {
    NEXT_TASK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Review-mode request travelling from the agent task to the UI loop.
pub struct ChangesetAsk {
    /// Absolute paths, index-aligned with `files`, so the verdict can be
    /// mapped back without re-deriving anything from display strings.
    pub paths: Vec<std::path::PathBuf>,
    pub files: Vec<overlay::ChangesetFile>,
    pub reply: tokio::sync::oneshot::Sender<crate::tools::ReviewOutcome>,
}

/// Channel-based [`Reviewer`](crate::tools::Reviewer): the agent asks, the
/// changeset overlay answers, and a dismissed prompt writes nothing.
pub struct TuiReviewer {
    tx: tokio::sync::mpsc::UnboundedSender<ChangesetAsk>,
}

impl TuiReviewer {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<ChangesetAsk>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl crate::tools::Reviewer for TuiReviewer {
    async fn review(
        &self,
        cwd: &std::path::Path,
        changes: &[crate::tools::checkpoint::StagedChange],
    ) -> crate::tools::ReviewOutcome {
        let files: Vec<overlay::ChangesetFile> = changes
            .iter()
            .map(|c| {
                let (added, removed) = c.counts(cwd);
                overlay::ChangesetFile {
                    path: c.display_path(cwd),
                    added,
                    removed,
                    diff: c.diff(cwd).map(|d| d.text).unwrap_or_default(),
                    // default to accepting: "yes, do what you planned" is the
                    // common case, and rejecting one file is one keystroke
                    accepted: true,
                }
            })
            .collect();
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(ChangesetAsk {
                paths: changes.iter().map(|c| c.path.clone()).collect(),
                files,
                reply: rtx,
            })
            .is_err()
        {
            // UI gone: fail closed, nothing is written
            return crate::tools::ReviewOutcome::reject_all(
                changes,
                "the review prompt could not be shown; no files were written",
            );
        }
        rrx.await.unwrap_or_else(|_| {
            crate::tools::ReviewOutcome::reject_all(
                changes,
                "the review prompt was dismissed; no files were written",
            )
        })
    }
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
    /// Task this approver belongs to (one per agent, so a rebuild gets a new
    /// one and a prompt always names its task).
    task: u64,
    origin: AskOrigin,
}

impl TuiApprover {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<ApprovalAsk>) -> Self {
        Self::with_allows(tx, SessionAllows::default())
    }

    pub fn with_allows(
        tx: tokio::sync::mpsc::UnboundedSender<ApprovalAsk>,
        session_allows: SessionAllows,
    ) -> Self {
        Self {
            tx,
            session_allows,
            task: next_task_id(),
            origin: AskOrigin::Main,
        }
    }

    pub fn origin(mut self, origin: AskOrigin) -> Self {
        self.origin = origin;
        self
    }
}

#[async_trait::async_trait]
impl Approver for TuiApprover {
    fn origin(&self) -> Option<String> {
        (self.origin == AskOrigin::SubAgent).then(|| "sub-agent".to_string())
    }

    fn scoped(&self, label: &str) -> Option<Arc<dyn Approver>> {
        let origin = match label {
            "sub-agent" => AskOrigin::SubAgent,
            _ => AskOrigin::Main,
        };
        Some(Arc::new(Self {
            tx: self.tx.clone(),
            session_allows: self.session_allows.clone(),
            task: self.task,
            origin,
        }))
    }

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
                id: next_ask_id(),
                task: self.task,
                origin: self.origin.clone(),
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

/// Where the composer was last drawn, so a mouse click can put the caret at
/// the clicked character instead of only at the end.
#[derive(Debug, Clone, Copy)]
struct InputHit {
    area: Rect,
    /// Wrap width used for the drawn rows.
    budget: usize,
    /// First visual row shown in the box.
    first: usize,
    /// Width of the `❯ ` prompt (only on the first visible row).
    prompt: usize,
}

/// One undo step: the whole buffer plus caret. A task is normally short, so
/// cloning the text is cheaper (and far less bug-prone) than an op log.
#[derive(Debug, Clone)]
struct Snapshot {
    text: String,
    cursor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Type,
    Delete,
    Other,
}

/// Multi-line editor state: text + a char-indexed caret + recallable history
/// (↑/↓), with readline-style shortcuts (Ctrl+A/E/U/K/W), word-wise motions
/// (Ctrl+←/→), and an undo stack (Ctrl+Z / Ctrl+Y).
#[derive(Default)]
struct Input {
    text: String,
    /// caret position in CHARS (not bytes); always <= char count
    cursor: usize,
    history: Vec<String>,
    /// `Some(i)` while browsing history; the live draft is stashed meanwhile.
    hist_pos: Option<usize>,
    draft: String,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Timestamp/kind of the last snapshot, so a run of keystrokes undoes as
    /// one step instead of one character at a time.
    last_edit: Option<(std::time::Instant, EditKind)>,
}

impl Input {
    const UNDO_CAP: usize = 200;
    const COALESCE: std::time::Duration = std::time::Duration::from_millis(600);

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

    /// Snapshot the buffer before a mutation. Consecutive edits of the same
    /// kind within [`Self::COALESCE`] share the previous snapshot, so typing a
    /// sentence (or holding backspace) is one undo step.
    fn checkpoint(&mut self, kind: EditKind) {
        let now = std::time::Instant::now();
        let coalesce = self
            .last_edit
            .map(|(at, prev)| prev == kind && now.duration_since(at) <= Self::COALESCE)
            .unwrap_or(false);
        self.last_edit = Some((now, kind));
        if coalesce {
            return;
        }
        self.undo.push(Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        });
        if self.undo.len() > Self::UNDO_CAP {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Movement ends the current typing run, so the next edit is its own step.
    fn break_group(&mut self) {
        self.last_edit = None;
    }

    fn insert_raw(&mut self, ch: char) {
        let b = self.byte_of(self.cursor);
        self.text.insert(b, ch);
        self.cursor += 1;
    }

    fn insert(&mut self, ch: char) {
        self.checkpoint(EditKind::Type);
        self.insert_raw(ch);
    }

    /// Insert a pasted block at the caret; CRLF/CR normalize to `\n` so a
    /// multi-line paste lands as ONE task.
    fn insert_str(&mut self, text: &str) {
        self.checkpoint(EditKind::Type);
        let mut after_cr = false;
        for ch in text.chars() {
            match ch {
                '\r' => {
                    self.insert_raw('\n');
                    after_cr = true;
                }
                '\n' if after_cr => after_cr = false,
                c => {
                    self.insert_raw(c);
                    after_cr = false;
                }
            }
        }
    }

    fn set_text(&mut self, text: String) {
        self.checkpoint(EditKind::Other);
        self.text = text;
        self.cursor = self.chars_len();
        self.hist_pos = None;
        self.break_group();
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.hist_pos = None;
        self.draft.clear();
        self.undo.clear();
        self.redo.clear();
        self.break_group();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.checkpoint(EditKind::Delete);
        let b = self.byte_of(self.cursor - 1);
        self.text.remove(b);
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        if self.cursor >= self.chars_len() {
            return;
        }
        self.checkpoint(EditKind::Delete);
        let b = self.byte_of(self.cursor);
        self.text.remove(b);
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.break_group();
    }

    fn right(&mut self) {
        if self.cursor < self.chars_len() {
            self.cursor += 1;
        }
        self.break_group();
    }

    fn is_word(ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_'
    }

    /// Move to the start of the previous word (Ctrl+←).
    fn word_left(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i > 0 && !Self::is_word(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && Self::is_word(chars[i - 1]) {
            i -= 1;
        }
        self.cursor = i;
        self.break_group();
    }

    /// Move to the start of the next word (Ctrl+→).
    fn word_right(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i < chars.len() && Self::is_word(chars[i]) {
            i += 1;
        }
        while i < chars.len() && !Self::is_word(chars[i]) {
            i += 1;
        }
        self.cursor = i;
        self.break_group();
    }

    fn home(&mut self) {
        self.cursor = 0;
        self.break_group();
    }

    fn end(&mut self) {
        self.cursor = self.chars_len();
        self.break_group();
    }

    fn kill_to_start(&mut self) {
        self.checkpoint(EditKind::Other);
        let b = self.byte_of(self.cursor);
        self.text.drain(..b);
        self.cursor = 0;
        self.break_group();
    }

    fn kill_to_end(&mut self) {
        self.checkpoint(EditKind::Other);
        let b = self.byte_of(self.cursor);
        self.text.truncate(b);
        self.break_group();
    }

    /// Delete the word before the caret (Ctrl+W).
    fn delete_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.checkpoint(EditKind::Delete);
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

    /// Delete the word after the caret (Ctrl+Delete).
    fn delete_word_forward(&mut self) {
        if self.cursor >= self.chars_len() {
            return;
        }
        self.checkpoint(EditKind::Delete);
        let chars: Vec<char> = self.text.chars().collect();
        let mut end = self.cursor.min(chars.len());
        while end < chars.len() && !Self::is_word(chars[end]) {
            end += 1;
        }
        while end < chars.len() && Self::is_word(chars[end]) {
            end += 1;
        }
        let (bs, be) = (self.byte_of(self.cursor), self.byte_of(end));
        self.text.replace_range(bs..be, "");
    }

    fn newline(&mut self) {
        self.checkpoint(EditKind::Other);
        self.insert_raw('\n');
        self.break_group();
    }

    fn undo(&mut self) -> bool {
        let Some(snap) = self.undo.pop() else {
            return false;
        };
        self.redo.push(Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        });
        self.text = snap.text;
        self.cursor = snap.cursor.min(self.chars_len());
        self.hist_pos = None;
        self.break_group();
        true
    }

    fn redo(&mut self) -> bool {
        let Some(snap) = self.redo.pop() else {
            return false;
        };
        self.undo.push(Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        });
        self.text = snap.text;
        self.cursor = snap.cursor.min(self.chars_len());
        self.hist_pos = None;
        self.break_group();
        true
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
            let ranges = wrap_ranges(&lchars, budget);
            for &(start, end) in &ranges {
                let text: String = lchars[start..end].iter().collect();
                rows.push(text);
                if let Some(cl) = caret_local {
                    if cl >= start && cl <= end {
                        let col = lchars[start..cl]
                            .iter()
                            .map(|c| UnicodeWidthChar::width(*c).unwrap_or(0))
                            .sum();
                        caret = (rows.len() - 1, col);
                    }
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

    /// Char index under a clicked visual `(row, column)`, using the same wrap
    /// rules as [`Self::visual_rows`]. Columns past the end of a row land on
    /// its last character.
    fn caret_at_visual(&self, target_row: usize, col: usize, budget: usize) -> Option<usize> {
        let budget = budget.max(1);
        let mut row = 0usize;
        let mut base = 0usize;
        for line in self.text.split('\n') {
            let lchars: Vec<char> = line.chars().collect();
            for &(start, end) in &wrap_ranges(&lchars, budget) {
                if row == target_row {
                    let mut idx = start;
                    let mut width = 0usize;
                    while idx < end {
                        let cw = UnicodeWidthChar::width(lchars[idx]).unwrap_or(0);
                        if width + cw > col {
                            break;
                        }
                        width += cw;
                        idx += 1;
                    }
                    return Some(base + idx);
                }
                row += 1;
            }
            base += lchars.len() + 1;
        }
        None
    }

    fn set_cursor(&mut self, idx: usize) {
        self.cursor = idx.min(self.chars_len());
        self.hist_pos = None;
        self.break_group();
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
    /// First body row shown (long commands are scrollable).
    scroll: usize,
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
    /// Ring the terminal bell when a task ends (`[ui] bell = true`).
    bell: bool,
    /// The model's plan checklist: (content, status) pairs.
    todos: Vec<(String, String)>,
    rounds: u32,
    tokens_in: u64,
    tokens_out: u64,
    /// Session-wide totals across finished tasks (+, not overwritten per task).
    total_in: u64,
    total_out: u64,
    tasks_done: u32,
    est_tokens: u64,
    /// Transcript scroll: lines above the bottom (0 = follow the tail).
    scroll: std::cell::Cell<usize>,
    /// New rows that arrived while the reader was scrolled up.
    new_lines: std::cell::Cell<usize>,
    /// Entry the transcript search jumped to; highlighted until the next key.
    highlight: std::cell::Cell<Option<usize>>,
    /// Hits of the last transcript search, so `F3` repeats it.
    search_hits: std::cell::RefCell<Vec<SearchHit>>,
    search_pos: std::cell::Cell<usize>,
    last_total: std::cell::Cell<usize>,
    /// Rows of the last frame's conversation area, for click hit-testing.
    body_area: std::cell::Cell<Rect>,
    /// Composer geometry of the last frame, for click-to-caret.
    input_hit: std::cell::Cell<Option<InputHit>>,
    body_width: std::cell::Cell<u16>,
    hit_owner: std::cell::RefCell<Vec<usize>>,
    started: std::cell::Cell<Option<std::time::Instant>>,
    /// Duration of the last completed task (shown in the footer).
    last_elapsed: std::cell::Cell<Option<std::time::Duration>>,
    /// Esc was pressed and the task is still winding down.
    cancelling: bool,
    spinner: std::cell::Cell<usize>,
    busy: bool,
    /// Follow-ups typed while a task runs; started automatically when it ends.
    queued: VecDeque<String>,
    /// The last submitted task (`/retry`).
    last_task: Option<String>,
    /// Approval prompts waiting for a keystroke, oldest first. A single slot
    /// used to be enough only because two sources never asked at once; a
    /// provider rebuild or a `delegate` sub-task made that false, and the
    /// overwritten request was denied silently with nothing on screen.
    approvals: VecDeque<PendingApproval>,
    /// Task the newest `launch_task` started (labels prompts `task #N`).
    current_task: u64,
    /// Cached prompt tokens of the current task's last round.
    cache_read: u64,
    /// Active effort preset, shown as a badge in the header.
    preset: String,
    /// The project's own slash commands (`.kazi/commands`), loaded at startup.
    customs: Vec<super::custom::CustomCommand>,
    /// How many turns are pinned (`/pin`); the footer warns when a long task
    /// can lose its oldest instructions.
    pin_count: usize,
    /// A staged changeset waiting for the user's per-file verdict: the paths
    /// (index-aligned with the overlay's rows) and the reply channel back to
    /// the agent task.
    changeset_reply: Option<(
        Vec<std::path::PathBuf>,
        tokio::sync::oneshot::Sender<crate::tools::ReviewOutcome>,
    )>,
    /// USD so far in the current task (`None` = the model has no price entry).
    task_cost: Option<f64>,
    /// Background jobs still running, refreshed each frame for the footer.
    jobs_running: usize,
    /// Session-wide USD, accumulated as tasks finish.
    session_cost: f64,
    /// Whether any round this session was actually priced.
    cost_known: bool,
    overlay: Option<Overlay>,
    /// Clickable list area of the open overlay (mouse hit-testing).
    overlay_hit: std::cell::Cell<Option<overlay::Hit>>,
    /// Approximate inner height of the open modal (scroll bounds).
    overlay_body: std::cell::Cell<u16>,
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
            bell: false,
            todos: Vec::new(),
            rounds: 0,
            tokens_in: 0,
            tokens_out: 0,
            total_in: 0,
            total_out: 0,
            tasks_done: 0,
            est_tokens: 0,
            scroll: std::cell::Cell::new(0),
            new_lines: std::cell::Cell::new(0),
            highlight: std::cell::Cell::new(None),
            search_hits: std::cell::RefCell::new(Vec::new()),
            search_pos: std::cell::Cell::new(0),
            last_total: std::cell::Cell::new(0),
            body_area: std::cell::Cell::new(Rect::new(0, 0, 0, 0)),
            input_hit: std::cell::Cell::new(None),
            body_width: std::cell::Cell::new(80),
            hit_owner: std::cell::RefCell::new(Vec::new()),
            started: std::cell::Cell::new(None),
            last_elapsed: std::cell::Cell::new(None),
            cancelling: false,
            spinner: std::cell::Cell::new(0),
            busy: false,
            queued: VecDeque::new(),
            last_task: None,
            approvals: VecDeque::new(),
            current_task: 0,
            cache_read: 0,
            preset: "balanced".to_string(),
            customs: Vec::new(),
            pin_count: 0,
            jobs_running: 0,
            changeset_reply: None,
            task_cost: None,
            session_cost: 0.0,
            cost_known: false,
            overlay: None,
            overlay_hit: std::cell::Cell::new(None),
            overlay_body: std::cell::Cell::new(20),
            cmd_sel: std::cell::Cell::new(0),
            mode: PermissionMode::Edit,
            theme,
            quit: false,
        }
    }

    /// Snapshot of the cost counters `/cost` reports on (console owns the
    /// formatting, the screen owns the state).
    fn cost_context(&self) -> super::console::CostContext {
        super::console::CostContext {
            session_usd: self.session_cost,
            session_priced: self.cost_known,
            tasks_done: self.tasks_done,
            session_in: self.total_in,
            session_out: self.total_out,
            task_usd: self.task_cost,
        }
    }

    /// The prompt the keyboard is answering right now.
    fn front_approval(&self) -> Option<&PendingApproval> {
        self.approvals.front()
    }

    fn front_approval_mut(&mut self) -> Option<&mut PendingApproval> {
        self.approvals.front_mut()
    }

    fn has_approval(&self) -> bool {
        !self.approvals.is_empty()
    }

    /// Queue a prompt, telling the user when one was already on screen: two
    /// requests used to overwrite each other, and the overwritten one was
    /// denied without a word.
    fn push_approval(&mut self, approval: PendingApproval) {
        let queued_behind = self.approvals.len();
        let label = approval.ask.origin.label().to_string();
        let task = approval.ask.task;
        self.approvals.push_back(approval);
        if queued_behind > 0 {
            self.push_notice(
                Level::Info,
                format!("+1 approval queued ({label}, task #{task}, {queued_behind} pending)"),
            );
        }
    }

    /// Deny every pending prompt — used when the screen is tearing down, so no
    /// agent task is left waiting on a oneshot nobody will answer.
    fn deny_pending_approvals(&mut self, reason: &str) {
        while let Some(p) = self.approvals.pop_front() {
            let _ = p.ask.reply.send(Decision::Deny(Some(reason.to_string())));
        }
    }

    /// Render one agent event into the screen state.
    fn consume(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TextDelta { text } => self.transcript.append_delta(&text),
            AgentEvent::ChangesetPending { files } => {
                self.flush_streaming();
                let shown = files
                    .iter()
                    .take(6)
                    .map(|(path, added, removed)| format!("  {path} (+{added} −{removed})"))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.push_notice(
                    Level::Info,
                    format!(
                        "changeset: {} file(s) staged for review\n{shown}",
                        files.len()
                    ),
                );
            }
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
                cost_usd,
            } => {
                self.rounds = round;
                self.tokens_in = usage.input_tokens.unwrap_or(0);
                self.tokens_out = usage.output_tokens.unwrap_or(0);
                self.cache_read = usage.cache_read_tokens.unwrap_or(0);
                self.est_tokens = est_tokens;
                self.task_cost = cost_usd;
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
        self.cancelling = false;
        self.highlight.set(None);
        self.started.set(Some(std::time::Instant::now()));
        self.spinner.set(0);
        self.scroll_to_bottom();
    }

    /// Fold the finished task's per-round counters into the session totals.
    fn finish_task_usage(&mut self) {
        self.total_in = self.total_in.saturating_add(self.tokens_in);
        self.total_out = self.total_out.saturating_add(self.tokens_out);
        self.tasks_done = self.tasks_done.saturating_add(1);
        if let Some(cost) = self.task_cost.take() {
            self.session_cost += cost;
            self.cost_known = true;
        }
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.cache_read = 0;
    }

    /// `/usage` report: this run's totals plus the running task when busy.
    fn usage_report(&self) -> String {
        let total = self.total_in.saturating_add(self.total_out);
        let mut out = format!(
            "session usage (this run) — {} finished task(s)\n  tokens: in {} · out {} · total {}",
            self.tasks_done, self.total_in, self.total_out, total
        );
        if self.busy {
            out.push_str(&format!(
                "\n  running task: in {} · out {}",
                self.tokens_in, self.tokens_out
            ));
        }
        out
    }

    fn scroll_up(&self, n: usize) {
        self.scroll.set(self.scroll.get().saturating_add(n));
        self.new_lines.set(0);
    }

    fn scroll_down(&self, n: usize) {
        self.scroll.set(self.scroll.get().saturating_sub(n));
        if self.scroll.get() == 0 {
            self.new_lines.set(0);
        }
    }

    fn scroll_to_bottom(&self) {
        self.scroll.set(0);
        self.new_lines.set(0);
    }

    fn scroll_to_top(&self) {
        self.scroll.set(usize::MAX); // clamped in draw()
    }

    /// Remember a completed search so `F3` / `Shift+F3` can walk it again.
    fn remember_search(&self, hits: Vec<SearchHit>, pos: usize) {
        self.search_pos.set(pos);
        *self.search_hits.borrow_mut() = hits;
    }

    /// Entry of the next (`delta > 0`) or previous hit, wrapping around.
    fn step_search(&self, delta: isize) -> Option<usize> {
        let hits = self.search_hits.borrow();
        if hits.is_empty() {
            return None;
        }
        let len = hits.len() as isize;
        let cur = (self.search_pos.get().min(hits.len() - 1)) as isize;
        let next = (cur + delta).rem_euclid(len) as usize;
        self.search_pos.set(next);
        Some(hits[next].entry)
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
            Some(token) => super::console::command_rows_with(token, &self.customs),
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

    /// TAB in command position: complete a unique prefix, extend a shared
    /// prefix, or move through the ambiguous candidates.
    fn complete_command_input(&mut self) -> bool {
        if super::console::pending_command(&self.input.text).is_none() {
            return false;
        }
        let hints = self.command_hints();
        if hints.len() == 1 {
            // complete to the full name; keep a trailing space only when the
            // command wants an argument (so Enter never runs a truncated name)
            let row = &hints[0].0;
            let name = row.split_whitespace().next().unwrap_or(row).to_string();
            let wants_arg = row.split_whitespace().nth(1).is_some();
            self.input
                .set_text(if wants_arg { format!("{name} ") } else { name });
        } else if let Some(done) =
            super::console::command_completion_with(&self.input.text, &self.customs)
        {
            self.input.set_text(done);
        } else if hints.len() > 1 {
            self.move_command_sel(1);
        }
        true
    }

    /// Enter/Tab resolution of a half-typed command: the unique match, or the
    /// row the dropdown is highlighting.
    fn resolve_command_input(&self) -> Option<String> {
        let token = super::console::pending_command(&self.input.text)?;
        let rows = super::console::command_rows_with(token, &self.customs);
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

    /// TAB completion for the first argument of a slash command. Returns a
    /// note when several choices share no longer prefix (the UI lists them).
    fn complete_argument(
        &mut self,
        cfg: &crate::config::Config,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Option<String> {
        let line = self.input.text.clone();
        let comp = super::console::arg_completions(cfg, &line, provider, model)
            .or_else(|| theme_arg_completion(&line));
        let (start, candidates) = comp?;
        let typed = line.get(start..).unwrap_or("");
        if candidates.len() == 1 {
            let mut next = line[..start].to_string();
            next.push_str(&candidates[0]);
            next.push(' ');
            self.input.set_text(next);
            return None;
        }
        let common = super::console::common_prefix(&candidates);
        if common.len() > typed.len() {
            let mut next = line[..start].to_string();
            next.push_str(&common);
            self.input.set_text(next);
            return None;
        }
        let mut note = format!(
            "matches: {}",
            candidates
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
        if candidates.len() > 8 {
            note.push_str(&format!(" … +{} more", candidates.len() - 8));
        }
        Some(note)
    }

    /// Mouse click inside the open list overlay: move the selection to the
    /// clicked row. Returns `true` when the click was consumed.
    fn overlay_click(&mut self, row: u16) -> bool {
        let Some(hit) = self.overlay_hit.get() else {
            return false;
        };
        let Some(ov) = self.overlay.as_mut() else {
            return false;
        };
        if row < hit.area.y || row >= hit.area.y.saturating_add(hit.area.height) {
            return false;
        }
        let offset = (row - hit.area.y) as usize;
        if offset >= hit.shown {
            return false;
        }
        let idx = hit.start + offset;
        match ov {
            Overlay::Palette(p) => p.cursor = idx,
            Overlay::Picker(p) => p.cursor = idx,
            Overlay::Sessions(s) => {
                s.cursor = idx;
                s.confirm_delete = None;
                s.refresh_preview();
            }
            Overlay::Search(s) => s.cursor = idx,
            Overlay::Changeset(c) => {
                c.cursor = idx;
                c.scroll = 0;
            }
            Overlay::Help(_) | Overlay::Diff(_) => return false,
        }
        true
    }

    /// Mouse wheel while a modal is open: lists move their cursor, the long
    /// viewers scroll. The transcript behind the overlay stays put.
    fn overlay_scroll(&mut self, delta: isize, body_height: u16) {
        let Some(ov) = self.overlay.as_mut() else {
            return;
        };
        match ov {
            Overlay::Palette(p) => p.move_cursor(delta),
            Overlay::Picker(p) => p.move_cursor(delta),
            Overlay::Sessions(s) => {
                s.move_cursor(delta);
            }
            Overlay::Search(s) => s.move_cursor(delta),
            Overlay::Changeset(c) => {
                c.move_cursor(delta);
            }
            Overlay::Help(h) => {
                // the overlay already carries its extra rows: counting them here
                // needs no borrow of `self` (which is mutably borrowed above)
                let max = (overlay::help_lines(self.theme).len() + h.extra.len())
                    .saturating_sub(body_height as usize);
                h.scroll = (h.scroll.min(max) as isize + delta * 3).clamp(0, max as isize) as usize;
            }
            Overlay::Diff(d) => d.scroll_by(delta * 3, body_height as usize),
        }
    }

    /// The help overlay, with the project's commands appended (they live on
    /// disk, so the static table in `overlay` cannot know them).
    fn help_overlay(&self) -> Overlay {
        Overlay::Help(Help {
            scroll: 0,
            extra: self.help_extra(),
        })
    }

    /// Just the project-command section (empty when there is none).
    fn help_extra(&self) -> Vec<ratatui::text::Line<'static>> {
        let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
        if self.customs.is_empty() {
            return lines;
        }
        let dim = ratatui::style::Style::default().fg(self.theme.dim);
        lines.push(ratatui::text::Line::from(""));
        lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
            "project commands",
            dim,
        )));
        for c in &self.customs {
            lines.push(ratatui::text::Line::from(format!(
                "  {:<22} {}{}",
                c.usage(),
                c.description,
                if super::console::is_builtin_command(&c.name) {
                    " (shadowed by the built-in)"
                } else {
                    ""
                }
            )));
        }
        lines
    }

    fn input_rows(&self, width: u16) -> u16 {
        // grow with the *wrapped* row count (a long single-line task wraps)
        let budget = width.saturating_sub(4).max(4) as usize;
        (self.input.visual_rows(budget).0.len().clamp(1, 6) as u16) + 2
    }

    /// The dim remainder of a unique slash command, drawn after the caret so
    /// the command is discoverable before `Tab` is pressed.
    fn ghost_completion(&self) -> Option<String> {
        if self.busy
            || self.overlay.is_some()
            || self.has_approval()
            || self.input.cursor != self.input.chars_len()
        {
            return None;
        }
        let text = &self.input.text;
        super::console::pending_command(text)?;
        let done = super::console::command_completion(text)?;
        (done.len() > text.len()).then(|| done[text.len()..].to_string())
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

        if let Some(pending) = self.front_approval() {
            overlay::draw_approval(
                f,
                area,
                self.theme,
                overlay::ApprovalView {
                    tool: &pending.ask.tool,
                    summary: &pending.ask.summary,
                    origin: pending.ask.origin.label(),
                    task: pending.ask.task,
                    queue: self.approvals.len(),
                    reason_mode: pending.reason_mode,
                    reason: &pending.reason,
                    scroll: pending.scroll,
                },
            );
            self.overlay_hit.set(None);
        } else if let Some(ov) = &self.overlay {
            // the modal's inner body is roughly the screen minus its chrome;
            // scroll handlers only need a bound, not an exact rect
            self.overlay_body.set(area.height.saturating_sub(6));
            let hit = overlay::draw(f, area, self.theme, ov);
            self.overlay_hit.set(hit);
        } else {
            self.overlay_hit.set(None);
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
            PermissionMode::Review => Style::default().fg(theme.accent),
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
        ];
        // effort badge: without it the same model can answer very differently
        // after a /effort and nothing on screen says so
        if !self.preset.is_empty() && self.preset != "balanced" {
            spans.push(Span::styled(
                format!("· {}", clip(&self.preset, 12)),
                Style::default().fg(theme.warn),
            ));
        }
        spans.push(Span::styled(
            format!("  {}", clip(&self.session_name, 24)),
            Style::default().fg(theme.accent),
        ));
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
        if !self.busy {
            if let Some(d) = self.last_elapsed.get() {
                spans.push(Span::styled(
                    format!("last {:.1}s  ", d.as_secs_f64()),
                    Style::default().fg(theme.faint),
                ));
            }
        }
        if self.busy {
            let secs = self
                .started
                .get()
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            let activity = if self.cancelling {
                format!("{} cancelling…  ", theme.spinner(self.spinner.get()))
            } else {
                format!(
                    "{} {}s  r{}/{}  ",
                    theme.spinner(self.spinner.get()),
                    secs,
                    self.rounds,
                    self.max_rounds
                )
            };
            spans.push(Span::styled(activity, Style::default().fg(theme.accent)));
            spans.push(Span::styled(
                format!("↑{} ↓{} ", self.tokens_in, self.tokens_out),
                Style::default().fg(theme.faint),
            ));
            if let Some(cost) = self.task_cost {
                spans.push(Span::styled(
                    format!("{} ", super::console::money(cost)),
                    Style::default().fg(theme.faint),
                ));
            }
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
            self.new_lines
                .set(self.new_lines.get().saturating_add(grew));
        }
        let height = main.height as usize;
        let (start, end, off) = scroll_range(total, height, self.scroll.get());
        self.scroll.set(off);

        let mut visible: Vec<Line<'static>> = Vec::with_capacity(end.saturating_sub(start));
        let mut owners: Vec<usize> = Vec::with_capacity(end.saturating_sub(start));
        let highlight = self.highlight.get();
        for i in start..end {
            if i < static_len {
                let owner = cache.owner.get(i).copied().unwrap_or(usize::MAX);
                let mut line = cache.lines[i].clone();
                if highlight.is_some() && highlight == Some(owner) {
                    line = line.patch_style(theme.selection());
                }
                visible.push(line);
                owners.push(owner);
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
            self.input_hit.set(None);
            return;
        }

        let prompt = format!("{} ", theme.glyphs.prompt);
        let prompt_w = UnicodeWidthStr::width(prompt.as_str());
        let budget = (inner.width as usize).saturating_sub(prompt_w);
        let (rows, caret_row, caret_col) = self.input.visual_rows(budget);
        let visible_rows = inner.height as usize;
        let first = caret_row.saturating_sub(visible_rows.saturating_sub(1));
        self.input_hit.set(Some(InputHit {
            area: inner,
            budget,
            first,
            prompt: prompt_w,
        }));

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
            if !self.has_approval() && self.overlay.is_none() {
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
        if let Some(ghost) = self.ghost_completion() {
            if caret_row >= first && caret_row < first.saturating_add(visible_rows) {
                if let Some(line) = lines.get_mut(caret_row - first) {
                    let used: usize = line
                        .spans
                        .iter()
                        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                        .sum();
                    if used + UnicodeWidthStr::width(ghost.as_str()) <= inner.width as usize {
                        line.spans
                            .push(Span::styled(ghost, Style::default().fg(theme.faint)));
                    }
                }
            }
        }
        f.render_widget(
            Paragraph::new(lines).style(Style::default().fg(theme.text)),
            inner,
        );
        if !self.has_approval() && self.overlay.is_none() {
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
        let hints: Vec<Span> = if self.has_approval() {
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
                Span::styled("F1", theme.key_style()),
                Span::styled(" help  ", Style::default().fg(theme.dim)),
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
                Span::styled("F1", theme.key_style()),
                Span::styled(" help  ", Style::default().fg(theme.dim)),
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
        if self.jobs_running > 0 {
            right.push(Span::styled(
                format!(
                    "{} job{} running  ",
                    self.jobs_running,
                    if self.jobs_running == 1 { "" } else { "s" }
                ),
                Style::default().fg(theme.accent),
            ));
        }
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
            let new = self.new_lines.get();
            right.push(Span::styled(
                if new > 0 {
                    format!("↓ {new} new · Ctrl+End  ")
                } else {
                    "scrolled · Ctrl+End  ".to_string()
                },
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

/// Split one logical line into display rows `(start, end)` char ranges, using
/// the composer's wrap rule: break at the last space that fits, drop the space
/// that caused the break. An empty line yields one empty range.
fn wrap_ranges(chars: &[char], budget: usize) -> Vec<(usize, usize)> {
    let budget = budget.max(1);
    if chars.is_empty() {
        return vec![(0, 0)];
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let mut end = i;
        let mut width = 0usize;
        while end < chars.len() {
            let cw = UnicodeWidthChar::width(chars[end]).unwrap_or(0);
            if width + cw > budget {
                break;
            }
            width += cw;
            end += 1;
        }
        if end == i {
            end = i + 1; // one glyph wider than the box still gets a row
        }
        let break_at = (i + 1..end).rev().find(|&k| chars[k] == ' ').unwrap_or(end);
        out.push((i, break_at));
        i = if break_at < chars.len() && chars[break_at] == ' ' {
            break_at + 1
        } else {
            break_at
        };
    }
    out
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

/// `/theme <name>` completion (the theme list lives in the TUI, not console).
fn theme_arg_completion(line: &str) -> Option<(usize, Vec<String>)> {
    let trimmed = line.trim_start();
    let (verb, rest) = trimmed.split_once(char::is_whitespace)?;
    if verb != "/theme" {
        return None;
    }
    let arg = rest.trim_start();
    if arg.contains(char::is_whitespace) {
        return None;
    }
    let prefix = arg.to_ascii_lowercase();
    let candidates: Vec<String> = ThemeName::names()
        .into_iter()
        .filter(|n| n.starts_with(&prefix))
        .map(|n| n.to_string())
        .collect();
    if candidates.is_empty() {
        return None;
    }
    Some((line.len() - arg.len(), candidates))
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
    screen.current_task = next_task_id();
    screen.begin_task(&task);
    let refs = super::file_ref_names(&task);
    if !refs.is_empty() {
        // the model reads the contents; the human sees which files went along
        screen.push_notice(Level::Info, format!("attaching @{}", refs.join(", @")));
    }
    let cancel = owned.cancel_token();
    let expanded = super::expand_file_refs(&task, &owned.ctx.cwd);
    // images go as message parts, not as text; anything that could not be
    // attached is said out loud instead of vanishing
    let (images, image_notes) = super::expand_image_refs(&task, &owned.ctx.cwd);
    for note in &image_notes {
        screen.push_notice(Level::Warn, note.clone());
    }
    if !images.is_empty() {
        screen.push_notice(
            Level::Info,
            format!("sending {} image attachment(s)", images.len()),
        );
    }
    let slot2 = slot.clone();
    let tx2 = tx.clone();
    let handle = tokio::spawn(async move {
        let mut owned = owned;
        let out = owned
            .run_task_with_images(&expanded, images, |ev| {
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
    chat_loop_with_config(cfg, provider, model, mode, topic, resume, None).await
}

/// Same as [`chat_loop_with`], plus the config file behind `cfg` so `/theme`
/// and `/details` can persist their `[ui]` choice. A library embedder without a
/// file passes `None` and the settings stay session-only.
#[allow(clippy::too_many_arguments)]
pub async fn chat_loop_with_config(
    cfg: crate::config::Config,
    provider: Option<String>,
    model: Option<String>,
    mode: PermissionMode,
    topic: Option<String>,
    resume: Option<String>,
    config_file: Option<std::path::PathBuf>,
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
    let (review_tx, review_rx) = tokio::sync::mpsc::unbounded_channel::<ChangesetAsk>();
    let review_tx_rebuild = review_tx.clone();
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
    // review mode: the changeset modal answers the engine's reviewer
    agent.ctx.reviewer = Some(Arc::new(TuiReviewer::new(review_tx)));

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
        config_file,
        jobs: agent.ctx.jobs.clone(),
    };

    match chat_loop_tui(
        agent,
        session_name,
        save,
        ask_rx,
        &mut sess,
        ask_tx_rebuild,
        session_allows,
        review_rx,
        review_tx_rebuild,
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
    /// Where `cfg` came from, so `/theme` and `/details` can be remembered.
    pub config_file: Option<std::path::PathBuf>,
    /// The session's background jobs. A clone of the agent's table, kept here
    /// so `/jobs` works while a task holds the agent (and so a provider
    /// rebuild, which clones the context, keeps the same table).
    pub jobs: Option<Arc<crate::tools::jobs::JobTable>>,
}

/// Best-effort persistence of one `[ui]` setting. Returns "" when there is no
/// config file behind this session (a library embedder), " · saved" on success,
/// or a warning suffix when the file could not be rewritten.
fn persist_note(sess: &SessionState, key: &str, value: &str) -> String {
    let Some(path) = sess.config_file.as_ref() else {
        return String::new();
    };
    match crate::config::set_ui_setting(path, key, value) {
        Ok(()) => " · saved".to_string(),
        Err(e) => format!(" · not saved ({e})"),
    }
}

/// Run the TUI chat loop. `chat_loop_tui` owns its terminal; stdout is
/// restored on every return path.
#[allow(clippy::too_many_arguments)]
pub async fn chat_loop_tui(
    agent: Agent,
    session_name: String,
    on_session_save: impl Fn(&str, &Agent),
    mut approvals: tokio::sync::mpsc::UnboundedReceiver<ApprovalAsk>,
    sess: &mut SessionState,
    ask_tx: tokio::sync::mpsc::UnboundedSender<ApprovalAsk>,
    session_allows: SessionAllows,
    mut review_rx: tokio::sync::mpsc::UnboundedReceiver<ChangesetAsk>,
    review_tx: tokio::sync::mpsc::UnboundedSender<ChangesetAsk>,
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
    screen.preset = agent.preset().to_string();
    // project commands are discovered once at startup: editing one mid-session
    // is a restart, which keeps the dropdown and the help text consistent
    screen.customs = super::custom::discover(std::path::Path::new(&screen.cwd));
    if !screen.customs.is_empty() {
        let names: Vec<String> = screen
            .customs
            .iter()
            .map(|c| c.name.clone())
            .filter(|n| !super::console::is_builtin_command(n))
            .collect();
        if !names.is_empty() {
            screen.push_notice(
                Level::Info,
                format!("project commands: {}", names.join(" ")),
            );
        }
    }
    screen.bell = sess.cfg.ui.bell.unwrap_or(false);
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
        &mut review_rx,
        &review_tx,
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
        PermissionMode::Review => "writes staged for your review",
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
            if arg.is_empty() {
                screen.overlay = Some(screen.help_overlay());
            } else {
                match console::help_for_with(arg, &screen.customs) {
                    Some(text) => screen.show_text(text),
                    None => {
                        let verb = if arg.starts_with('/') {
                            arg.to_string()
                        } else {
                            format!("/{arg}")
                        };
                        screen.show_text(console::unknown_command_text(&verb));
                    }
                }
            }
        }
        "jobs" => {
            let Some(table) = sess.jobs.clone() else {
                screen.show_text("background jobs are not available in this session");
                return None;
            };
            let mut parts = arg.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some("kill"), Some(id)) => match id.parse::<u64>() {
                    Ok(id) if table.kill(id) => {
                        screen.push_notice(Level::Warn, format!("job #{id} killed"))
                    }
                    Ok(id) => screen.show_text(format!(
                        "job #{id} is not running (see /jobs for the current state)"
                    )),
                    Err(_) => screen.show_text(console::usage_for("/jobs")),
                },
                (Some(id), None) => match id.parse::<u64>() {
                    Ok(id) => match table.get(id) {
                        Some(job) => {
                            let (tail, truncated) = job.tail(60);
                            let mut out = format!(
                                "#{} {}\ncommand: {}\ncwd: {}\noutput ({} bytes{}):\n{}",
                                job.id,
                                job.status().label(),
                                job.command,
                                job.cwd.display(),
                                job.output_len(),
                                if truncated { ", last 60 lines" } else { "" },
                                if tail.trim().is_empty() {
                                    "(no output yet)"
                                } else {
                                    &tail
                                }
                            );
                            if job.status().is_running() {
                                out.push_str("\n  (/jobs kill <id> stops it)");
                            }
                            screen.show_text(out);
                        }
                        None => screen.show_text(format!("no job #{id}")),
                    },
                    Err(_) => screen.show_text(console::usage_for("/jobs")),
                },
                _ => {
                    let jobs = table.list();
                    if jobs.is_empty() {
                        screen.show_text(
                            "no background jobs yet — the model starts them with run_command background=true",
                        );
                    } else {
                        let mut out = format!(
                            "background jobs ({} running of {}):\n",
                            table.running(),
                            jobs.len()
                        );
                        for job in &jobs {
                            out.push_str(&format!("  {}\n", crate::tools::jobs::describe(job)));
                        }
                        out.push_str("  (/jobs <id> shows output · /jobs kill <id> stops one)");
                        screen.show_text(out);
                    }
                }
            }
        }
        "find" => {
            if arg.is_empty() {
                screen.show_text(console::usage_for("/find"));
                return None;
            }
            let hits = super::session::search(arg, 3, 20);
            if hits.is_empty() {
                screen.show_text(format!(
                    "no stored session mentions {arg:?} (user/assistant text only)"
                ));
            } else {
                let mut out = format!("{} hit(s) for {arg:?}:", hits.len());
                for h in &hits {
                    out.push_str(&format!(
                        "\n  {}  ·  {} line {}  ·  {}",
                        h.session, h.role, h.line, h.snippet
                    ));
                }
                out.push_str("\n  (/resume <session> opens one)");
                screen.show_text(out);
            }
        }
        "pin" => {
            let mut guard = slot.lock().await;
            let Some(a) = guard.as_mut() else {
                drop(guard);
                screen.show_text(BUSY_TEXT);
                return None;
            };
            let (text, pins) = pin_report(a, arg);
            drop(guard);
            screen.show_text(text);
            if let Some(pins) = pins {
                screen.pin_count = pins;
            }
        }
        "cost" => {
            if arg.eq_ignore_ascii_case("reset") {
                screen.session_cost = 0.0;
                screen.cost_known = false;
                screen.total_in = 0;
                screen.total_out = 0;
                screen.tasks_done = 0;
                screen.push_notice(Level::Info, "cost/usage totals reset for this run");
                return None;
            }
            let ctx = screen.cost_context();
            let text = match slot.lock().await.as_ref() {
                Some(a) => console::cost_report(a, ctx),
                None => {
                    // a running task holds the agent: report from the screen
                    let mut out = String::from("cost (a task is running)\n");
                    match ctx.task_usd {
                        Some(cost) => out.push_str(&format!(
                            "  this task   {} so far\n",
                            console::money(cost)
                        )),
                        None => out.push_str(
                            "  this task   unpriced so far (no price entry, or no finished round)\n",
                        ),
                    }
                    if ctx.session_priced {
                        out.push_str(&format!(
                            "  this run    {} before this task\n",
                            console::money(ctx.session_usd)
                        ));
                    }
                    out.trim_end().to_string()
                }
            };
            screen.show_text(text);
        }
        "effort" => {
            let (current, _) = sess.cfg.active_preset();
            let name = if arg.is_empty() {
                // bare /effort cycles, so one keystroke is enough during a task
                let names = sess.cfg.preset_names();
                let pos = names.iter().position(|n| n == &current).unwrap_or(0);
                names[(pos + 1) % names.len().max(1)].clone()
            } else {
                arg.to_string()
            };
            if !sess.cfg.preset_names().iter().any(|n| n == &name) {
                screen.show_text(format!(
                    "usage: /effort [{}]",
                    sess.cfg.preset_names().join("|")
                ));
                return None;
            }
            // persist first, then rebuild the provider so the wire carries it
            sess.cfg.agent.preset = Some(name.clone());
            let note = match sess.config_file.as_ref() {
                Some(path) => match crate::config::set_table_setting(
                    path,
                    "[agent]",
                    "preset",
                    &format!("\"{name}\""),
                ) {
                    Ok(()) => " · saved",
                    Err(_) => " · not saved",
                },
                None => "",
            };
            start_switch(
                screen,
                slot,
                sess,
                ask_tx,
                session_allows,
                permission,
                sess.provider.clone(),
                sess.model.clone(),
                false,
                bg_tx,
            )
            .await;
            screen.push_notice(
                Level::Info,
                format!("effort -> {name}{note} (request settings applied on the switch)"),
            );
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
                    let note = persist_note(sess, "theme", &format!("\"{}\"", name.as_str()));
                    screen.push_notice(
                        Level::Info,
                        format!("theme -> {}{note}", screen.theme.name.as_str()),
                    );
                }
                None => {
                    screen.show_text(format!("usage: /theme [{}]", ThemeName::names().join("|")))
                }
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
            let note = persist_note(sess, "details", if next { "true" } else { "false" });
            screen.push_notice(
                Level::Info,
                format!(
                    "tool detail -> {}{note}",
                    if next { "full" } else { "collapsed" }
                ),
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
        "rename" => {
            if arg.is_empty() {
                screen.show_text(format!(
                    "current session: {}\n{}",
                    sess.session_name,
                    console::usage_for("/rename")
                ));
                return None;
            }
            let name = console::slugify(arg);
            if name.is_empty() {
                screen.show_text("that name has no usable characters — try letters or digits");
                return None;
            }
            if name == sess.session_name {
                screen.push_notice(Level::Info, format!("already saving as `{name}`"));
                return None;
            }
            let guard = slot.lock().await;
            let saved = guard.as_ref().and_then(|a| {
                (a.history.msgs_len() > 0).then(|| {
                    super::session::save(&name, a.history.system.as_deref(), &a.history.msgs)
                })
            });
            drop(guard);
            let old = std::mem::replace(&mut sess.session_name, name.clone());
            screen.session_name = name.clone();
            match saved {
                Some(Ok(path)) => screen.push_notice(
                    Level::Success,
                    format!(
                        "auto-save now targets `{name}` (wrote {}) — the old `{old}` file is kept",
                        path.display()
                    ),
                ),
                Some(Err(e)) => screen.push_notice(
                    Level::Warn,
                    format!("auto-save now targets `{name}`, but the snapshot failed: {e}"),
                ),
                None => screen.push_notice(
                    Level::Info,
                    format!("auto-save now targets `{name}` (nothing to save yet)"),
                ),
            }
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
                screen.show_text(
                    "usage: /permission [read-only|review|edit|yolo] — or Shift+Tab to cycle",
                );
            }
        }
        "allow" | "allows" => {
            let mut allows = session_allows.lock().unwrap_or_else(|e| e.into_inner());
            match arg.to_ascii_lowercase().as_str() {
                "" => {
                    if allows.is_empty() {
                        screen.push_notice(
                            Level::Info,
                            "no commands approved for this session (answer `a` at a prompt to add one)",
                        );
                    } else {
                        screen.push_notice(
                            Level::Info,
                            format!("{} session approval(s):", allows.len()),
                        );
                        let rows: Vec<String> =
                            allows.iter().map(|cmd| first_lines(cmd, 1)).collect();
                        for (i, cmd) in rows.iter().enumerate() {
                            screen.push_notice(Level::Info, format!("  {}. {cmd}", i + 1));
                        }
                        screen.show_text("usage: /allow clear");
                    }
                }
                "clear" | "reset" => {
                    let n = allows.len();
                    allows.clear();
                    drop(allows);
                    screen.push_notice(
                        Level::Info,
                        format!("cleared {n} session approval(s) — commands prompt again"),
                    );
                }
                _ => {
                    drop(allows);
                    screen.show_text(console::usage_for("/allow"));
                }
            }
        }
        "undo" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => console::undo_text(a),
                None => BUSY_TEXT.to_string(),
            };
            screen.show_text(text);
        }
        "plan" | "todos" => {
            if screen.todos.is_empty() {
                screen.push_notice(
                    Level::Info,
                    "no plan yet — the model writes one with todo_write when a task needs steps",
                );
            } else {
                let done = screen
                    .todos
                    .iter()
                    .filter(|(_, s)| s == "completed")
                    .count();
                screen.push_notice(
                    Level::Info,
                    format!("plan — {done}/{} done", screen.todos.len()),
                );
                let rows: Vec<String> = screen
                    .todos
                    .iter()
                    .map(|(content, status)| {
                        let mark = match status.as_str() {
                            "completed" => "[x]",
                            "in_progress" => "[>]",
                            _ => "[ ]",
                        };
                        format!("  {mark} {content}")
                    })
                    .collect();
                for row in rows {
                    screen.push_notice(Level::Info, row);
                }
            }
        }
        "context" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => console::context_report(a),
                None => format!(
                    "context estimate: ≈ {} / {} tokens ({}%)\n  the agent is busy — the full breakdown returns when the task ends",
                    screen.est_tokens,
                    screen.max_context,
                    screen
                        .est_tokens
                        .saturating_mul(100)
                        .checked_div(screen.max_context)
                        .unwrap_or(0)
                ),
            };
            screen.show_text(text);
        }
        "status" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => format!(
                    "{}\n{}\nrounds: {} · ctx ≈ {} tokens · budget {}\n{}",
                    console::status_text_with_cost(a),
                    console::context_report(a),
                    screen.rounds,
                    screen.est_tokens,
                    screen.max_context,
                    screen.usage_report()
                ),
                None => format!(
                    "provider/model: {}\nround budget:   {}\nrounds so far:  {} · ctx ≈ {} / {} tokens\n{}\n(task usage lands when the running round completes)",
                    screen.provider_desc,
                    screen.max_rounds,
                    screen.rounds,
                    screen.est_tokens,
                    screen.max_context,
                    screen.usage_report()
                ),
            };
            screen.show_text(text);
        }
        "usage" => {
            if arg.eq_ignore_ascii_case("reset") {
                let (t, t_in, t_out) = (screen.tasks_done, screen.total_in, screen.total_out);
                screen.total_in = 0;
                screen.total_out = 0;
                screen.tasks_done = 0;
                screen.push_notice(
                    Level::Info,
                    format!("session usage reset (cleared {t} task(s), {t_in} in / {t_out} out)"),
                );
            } else if arg.is_empty() {
                screen.show_text(screen.usage_report());
            } else {
                screen.show_text(console::usage_for("/usage"));
            }
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
        "diff" => {
            let back = if arg.trim().is_empty() {
                1
            } else {
                match console::parse_back(arg) {
                    Some(n) => n,
                    None => {
                        screen.show_text(console::usage_for("/diff"));
                        return None;
                    }
                }
            };
            match screen.transcript.diff_card(back) {
                Some((name, summary, diff)) => {
                    let title = if summary.trim().is_empty() {
                        name
                    } else {
                        format!("{name} {summary}")
                    };
                    screen.overlay = Some(Overlay::Diff(overlay::DiffView::new(title, diff)));
                }
                None => screen.push_notice(
                    Level::Info,
                    if back == 1 {
                        "no file diff yet — write_file/apply_patch calls show one".to_string()
                    } else {
                        format!("no diff {back} results back")
                    },
                ),
            }
        }
        "queue" => {
            let lower = arg.to_ascii_lowercase();
            if arg.is_empty() {
                if screen.queued.is_empty() {
                    screen.push_notice(Level::Info, "the follow-up queue is empty");
                } else {
                    screen.push_notice(
                        Level::Info,
                        format!("{} queued follow-up(s):", screen.queued.len()),
                    );
                    let pending: Vec<String> = screen
                        .queued
                        .iter()
                        .map(|task| first_lines(task, 2).replace('\n', " ⏎ "))
                        .collect();
                    for (i, first) in pending.iter().enumerate() {
                        screen.push_notice(Level::Info, format!("  {}. {first}", i + 1));
                    }
                    screen.show_text("usage: /queue clear · /queue drop <n>");
                }
            } else if lower == "clear" || lower == "drop all" {
                let n = screen.queued.len();
                screen.queued.clear();
                screen.push_notice(Level::Info, format!("cleared {n} queued follow-up(s)"));
            } else if let Some(idx) = lower.strip_prefix("drop ") {
                match idx.trim().parse::<usize>() {
                    Ok(n) if n >= 1 && n <= screen.queued.len() => {
                        if let Some(task) = screen.queued.remove(n - 1) {
                            screen.push_notice(
                                Level::Warn,
                                format!("dropped queued task {n}: {}", first_lines(&task, 1)),
                            );
                        }
                    }
                    _ => screen.show_text(console::usage_for("/queue")),
                }
            } else {
                screen.show_text(console::usage_for("/queue"));
            }
        }
        "tools" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => a.registry().catalog(),
                None => BUSY_TEXT.to_string(),
            };
            screen.show_text(text);
        }
        "skills" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => {
                    let skills = a.skills();
                    if let Some(want) = arg.split_whitespace().next() {
                        match skills.iter().find(|s| s.name == want.to_ascii_lowercase()) {
                            Some(s) => format!(
                                "# skill: {}\n{}\nsource: {}{}\n\n{}",
                                s.name,
                                if s.description.is_empty() {
                                    String::new()
                                } else {
                                    format!("{}\n", s.description)
                                },
                                s.path.display(),
                                if s.truncated { " (truncated)" } else { "" },
                                s.body
                            ),
                            None => format!(
                                "no skill named {want:?}; available: {}",
                                if skills.is_empty() {
                                    "none".to_string()
                                } else {
                                    skills
                                        .iter()
                                        .map(|s| s.name.clone())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                }
                            ),
                        }
                    } else if skills.is_empty() {
                        "no skills — add .kazi/skills/<name>/SKILL.md (name and description go in the system prompt; the model loads the body with use_skill)"
                            .to_string()
                    } else {
                        let mut out = format!("skills ({}):\n", skills.len());
                        for s in skills {
                            out.push_str(&format!(
                                "  {:<16} {}{}\n    {}\n",
                                s.name,
                                match s.scope {
                                    super::skills::Scope::Global => "(global) ",
                                    super::skills::Scope::Project => "",
                                },
                                s.description,
                                s.path.display()
                            ));
                        }
                        out.push_str("  (/skills <name> prints one's procedure)");
                        out
                    }
                }
                None => BUSY_TEXT.to_string(),
            };
            screen.show_text(text);
        }
        "instructions" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => {
                    let ins = a.instructions();
                    if ins.is_empty() {
                        "no project instructions loaded — add KAZI.md (or AGENTS.md) to the repository"
                            .to_string()
                    } else {
                        let mut out = format!("project instructions ({}):\n", ins.files.len());
                        for f in &ins.files {
                            out.push_str(&format!(
                                "  {}{}  ·  {} chars{}\n",
                                f.path.display(),
                                if f.global { " (global)" } else { "" },
                                f.chars,
                                if f.truncated { " (truncated)" } else { "" }
                            ));
                        }
                        for note in &ins.notes {
                            out.push_str(&format!("  note: {note}\n"));
                        }
                        out.push_str("  (the nearest file wins on conflicts; [agent] instructions = false disables)");
                        out
                    }
                }
                None => BUSY_TEXT.to_string(),
            };
            screen.show_text(text);
        }
        "mcp" => {
            let names = match slot.lock().await.as_ref() {
                Some(a) => a.registry().names(),
                None => {
                    screen.show_text(BUSY_TEXT);
                    return None;
                }
            };
            let mut out = String::new();
            if sess.cfg.mcp.servers.is_empty() {
                out.push_str("no MCP servers configured — add [mcp.servers.<nick>] to config.toml");
            } else {
                out.push_str(&format!("MCP servers ({})\n", sess.cfg.mcp.servers.len()));
                for (nick, entry) in &sess.cfg.mcp.servers {
                    let transport = match (&entry.url, &entry.command) {
                        (Some(url), _) => format!("http {url}"),
                        (None, Some(cmd)) => {
                            let args = if entry.args.is_empty() {
                                String::new()
                            } else {
                                format!(" {}", entry.args.join(" "))
                            };
                            format!("stdio {cmd}{args}")
                        }
                        (None, None) => "misconfigured (no url, no command)".to_string(),
                    };
                    let bridged: Vec<&String> = names
                        .iter()
                        .filter(|n| n.starts_with(&format!("mcp_{nick}_")))
                        .collect();
                    out.push_str(&format!(
                        "  {nick}\n    {transport}\n    {} tool(s){}\n",
                        bridged.len(),
                        if bridged.is_empty() {
                            String::new()
                        } else {
                            format!(
                                ": {}",
                                bridged
                                    .iter()
                                    .filter_map(|n| n.rsplit_once('_').map(|(_, last)| last))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        }
                    ));
                }
                if names.iter().any(|n| n == "mcp_resources") {
                    out.push_str(
                        "  resources/prompts: the model can call mcp_resources / mcp_prompts\n",
                    );
                }
            }
            screen.show_text(out.trim_end());
        }
        "export" => {
            let text = match slot.lock().await.as_ref() {
                Some(a) => export_session(a, sess, arg),
                None => BUSY_TEXT.to_string(),
            };
            screen.show_text(text);
        }
        "copy" => {
            let mut parts = arg.split_whitespace();
            let target = parts.next().unwrap_or("").to_ascii_lowercase();
            let back = parts
                .next()
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|n| *n >= 1)
                .unwrap_or(1);
            let text = match target.as_str() {
                "" | "answer" | "last" => {
                    screen.transcript.last_assistant().unwrap_or("").to_string()
                }
                "all" => screen.transcript.plain_text(),
                "tool" | "output" => screen.transcript.tool_output(back).unwrap_or_default(),
                "diff" => screen
                    .transcript
                    .diff_card(back)
                    .map(|(_, _, d)| d)
                    .unwrap_or_default(),
                "code" | "block" => screen.transcript.code_block(back).unwrap_or_default(),
                other => {
                    screen.show_text(format!(
                        "unknown copy target `{other}`\n{}",
                        console::usage_for("/copy")
                    ));
                    return None;
                }
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
            // project commands are prompts, not actions: expanding one yields
            // a task, exactly like typing the text yourself
            let verb = format!("/{other}");
            if let Some(custom) = screen.customs.iter().find(|c| c.name == verb) {
                if console::is_builtin_command(&verb) {
                    // unreachable in practice (built-ins match above), but a
                    // name collision must never run the WRONG prompt
                    screen.show_text(console::unknown_command_text(other));
                    return None;
                }
                let expanded = custom.expand(arg);
                screen.push_notice(
                    Level::Info,
                    format!("{} → {}", custom.name, clip(&custom.description, 60)),
                );
                return Some(expanded);
            }
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

/// `/pin` argument handling: `[n]` pins, `off` clears, bare lists.
/// Returns the report text plus the new pin count so the screen can show the
/// state without holding the agent lock again.
fn pin_report(agent: &mut Agent, arg: &str) -> (String, Option<usize>) {
    let arg = arg.trim();
    if arg.eq_ignore_ascii_case("off") || arg.eq_ignore_ascii_case("clear") {
        agent.history.clear_pins();
        return (
            "pins cleared — trimming may drop any turn again".to_string(),
            Some(0),
        );
    }
    if arg.is_empty() {
        let pins = agent.history.pins_list();
        if pins.is_empty() {
            return (
                "nothing pinned. /pin <n> keeps the turn around message n through trimming                  (n = the number shown by /last, /plan or an error)"
                    .to_string(),
                Some(0),
            );
        }
        let body = pins
            .iter()
            .map(|(i, text)| format!("  #{i}  {}", first_lines(text, 2).replace('\n', " / ")))
            .collect::<Vec<_>>()
            .join("\n");
        return (
            format!("pinned turns (kept through trimming):\n{body}"),
            Some(pins.len()),
        );
    }
    match arg.parse::<usize>() {
        Ok(n) if n < agent.history.msgs_len() => {
            agent.history.pin(n);
            (
                format!(
                    "pinned message #{n} — its whole turn now survives trimming (/pin off clears)"
                ),
                Some(agent.history.pins_len()),
            )
        }
        _ => (
            format!(
                "no message {arg} in this session ({} messages); /pin <n> with 0..{}",
                agent.history.msgs_len(),
                agent.history.msgs_len().saturating_sub(1)
            ),
            None,
        ),
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
                // the preset is part of the request, not of the provider
                agent.set_preset(cfg.active_preset().0);
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

/// Local alias for the shared encoder (OSC 52 clipboard and tests).
fn base64(data: &[u8]) -> String {
    crate::base64::encode(data)
}

/// Ask the terminal itself to set the clipboard with OSC 52. Only used when a
/// remote session makes a local clipboard tool pointless (or `KAZI_OSC52=1`
/// forces it); very large payloads are skipped because terminals cap them.
fn osc52_copy(text: &str) -> bool {
    const CAP: usize = 100_000;
    if text.is_empty() || text.len() > CAP {
        return false;
    }
    let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let mut out = std::io::stdout();
    out.write_all(seq.as_bytes())
        .and_then(|_| out.flush())
        .is_ok()
}

/// Whether the OSC 52 fallback is worth trying in this environment.
fn osc52_enabled() -> bool {
    std::env::var_os("KAZI_OSC52").is_some()
        || std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_TTY").is_some()
}

/// Copy text to the system clipboard, trying the platform's tool first and the
/// terminal's OSC 52 escape as a remote-friendly fallback. Returns how it
/// succeeded, or an error listing what was tried.
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
    if osc52_enabled() && osc52_copy(text) {
        return Ok("the terminal's OSC 52 clipboard".to_string());
    }
    Err(format!(
        "no clipboard tool worked (tried: {}). Use the terminal's own selection (Shift+drag), or set KAZI_OSC52=1 to try the terminal clipboard.",
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
    reviews: &mut tokio::sync::mpsc::UnboundedReceiver<ChangesetAsk>,
    _review_tx: &tokio::sync::mpsc::UnboundedSender<ChangesetAsk>,
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
        // cheap enough per frame (a handful of jobs) and always current
        screen.jobs_running = sess.jobs.as_ref().map(|t| t.running()).unwrap_or(0);
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
                        screen.preset = agent.preset().to_string();
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

            review = reviews.recv() => {
                if let Some(ask) = review {
                    // one changeset at a time: the engine is blocked on it
                    screen.changeset_reply = Some((ask.paths, ask.reply));
                    screen.overlay = Some(Overlay::Changeset(overlay::Changeset::new(ask.files)));
                }
            }

            ask = approvals.recv() => {
                if let Some(ask) = ask {
                    screen.push_approval(PendingApproval {
                        ask,
                        reason_mode: false,
                        reason: String::new(),
                        scroll: 0,
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
                    if let Some(t) = screen.started.get() {
                        screen.last_elapsed.set(Some(t.elapsed()));
                    }
                    screen.finish_task_usage();
                    screen.busy = false;
                    screen.started.set(None);
                    screen.cancelling = false;
                    runner = None;
                    current_cancel = None;
                    // A prompt left behind by this task can never be acted on
                    // any more: answer it explicitly instead of leaving an
                    // agent waiting on a oneshot and a modal on screen.
                    screen.deny_pending_approvals("task already finished");
                    if let Some(agent) = shared_agent.lock().await.as_ref() {
                        on_session_save(&sess.session_name, agent);
                    }
                    if screen.bell {
                        let mut out = std::io::stdout();
                        let _ = out.write_all(b"\x07");
                        let _ = out.flush();
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
                    // a search highlight is a "look here" marker: it lasts
                    // until the next keystroke
                    screen.highlight.set(None);

                    // Ctrl+C always means "get me out" — even mid-approval or
                    // with a modal open (the agent is cancelled on the way out).
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        screen.quit = true;
                        continue;
                    }

                    // 1) the approval prompt is modal
                    if screen.has_approval() {
                        handle_approval(screen, &key);
                        continue;
                    }

                    // 2) an open overlay owns the keyboard
                    if screen.overlay.is_some() {
                        let mut close = false;
                        let mut accept: Option<(PickerKind, String)> = None;
                        let mut jump: Option<usize> = None;
                        let mut resume: Option<String> = None;
                        let mut copy_text: Option<String> = None;
                        let mut search_snapshot: Option<(Vec<SearchHit>, usize)> = None;
                        let mut changeset_done: Option<(Vec<overlay::ChangesetFile>, bool)> = None;
                        let mut notices: Vec<(Level, String)> = Vec::new();
                        let body_h = screen.overlay_body.get() as usize;
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
                                Overlay::Diff(d) => match key.code {
                                    KeyCode::Esc | KeyCode::Char('q') => close = true,
                                    KeyCode::Up => d.scroll_by(-1, body_h),
                                    KeyCode::Down => d.scroll_by(1, body_h),
                                    KeyCode::PageUp => d.scroll_by(-12, body_h),
                                    KeyCode::PageDown => d.scroll_by(12, body_h),
                                    KeyCode::Home => d.scroll = 0,
                                    KeyCode::End => d.scroll = d.max_scroll(body_h),
                                    KeyCode::Char('c') | KeyCode::Char('C') => {
                                        copy_text = Some(d.diff.clone());
                                        close = true;
                                    }
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
                                        // the deleted row may have been selected
                                        s.cursor = s.cursor.min(s.rows.len().saturating_sub(1));
                                        s.refresh_preview();
                                        notices.push((Level::Warn, note));
                                    }
                                }
                                Overlay::Changeset(c) => match key.code {
                                    KeyCode::Up => c.move_cursor(-1),
                                    KeyCode::Down => c.move_cursor(1),
                                    KeyCode::PageUp => c.scroll = c.scroll.saturating_sub(12),
                                    KeyCode::PageDown => c.scroll += 12,
                                    KeyCode::Char(' ') => c.toggle(),
                                    KeyCode::Char('a') | KeyCode::Char('A') => c.accept_all(),
                                    KeyCode::Char('n') | KeyCode::Char('N') => c.reject_all(),
                                    KeyCode::Esc => {
                                        c.reject_all();
                                        changeset_done = Some((c.files.clone(), true));
                                        close = true;
                                    }
                                    KeyCode::Enter => {
                                        changeset_done = Some((c.files.clone(), false));
                                        close = true;
                                    }
                                    _ => {}
                                },
                                Overlay::Search(s) => {
                                    match key.code {
                                        KeyCode::Up => s.move_cursor(-1),
                                        KeyCode::Down => s.move_cursor(1),
                                        KeyCode::Esc => close = true,
                                        KeyCode::Backspace => s.pop(),
                                        KeyCode::Enter => {
                                            jump = s.hits.get(s.cursor).map(|h| h.entry);
                                            search_snapshot = Some((s.hits.clone(), s.cursor));
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
                        if let Some(text) = copy_text {
                            match copy_to_clipboard(&text) {
                                Ok(how) => screen.push_notice(
                                    Level::Success,
                                    format!("copied the diff ({} chars, {how})", text.chars().count()),
                                ),
                                Err(e) => {
                                    screen.push_notice(Level::Error, format!("copy failed: {e}"))
                                }
                            }
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
                        if let Some((hits, pos)) = search_snapshot {
                            screen.remember_search(hits, pos);
                        }
                        if let Some((files, cancelled)) = changeset_done {
                            // hand the per-file verdict back to the agent task
                            if let Some((paths, reply)) = screen.changeset_reply.take() {
                                let outcome = if cancelled {
                                    crate::tools::ReviewOutcome {
                                        files: paths
                                            .iter()
                                            .map(|p| crate::tools::ReviewedFile {
                                                path: p.clone(),
                                                accepted: false,
                                            })
                                            .collect(),
                                        note: Some(
                                            "the user cancelled the changeset review; no files were written"
                                                .to_string(),
                                        ),
                                    }
                                } else {
                                    crate::tools::ReviewOutcome {
                                        files: paths
                                            .iter()
                                            .enumerate()
                                            .map(|(i, p)| crate::tools::ReviewedFile {
                                                path: p.clone(),
                                                accepted: files
                                                    .get(i)
                                                    .map(|f| f.accepted)
                                                    .unwrap_or(false),
                                            })
                                            .collect(),
                                        note: None,
                                    }
                                };
                                let (accepted, total) = outcome
                                    .files
                                    .iter()
                                    .fold((0, 0), |(a, t), f| (a + usize::from(f.accepted), t + 1));
                                let _ = reply.send(outcome);
                                screen.push_notice(
                                    Level::Info,
                                    format!(
                                        "changeset: {accepted}/{total} file(s) accepted{}",
                                        if cancelled {
                                            " — the task was told nothing was written"
                                        } else {
                                            ""
                                        }
                                    ),
                                );
                            }
                        }
                        if let Some(entry) = jump {
                            screen.scroll_to_entry(entry);
                            screen.highlight.set(Some(entry));
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
                        KeyCode::F(1) => {
                            screen.overlay = Some(screen.help_overlay());
                        }
                        KeyCode::F(3) => {
                            let delta = if key.modifiers.contains(KeyModifiers::SHIFT) {
                                -1
                            } else {
                                1
                            };
                            match screen.step_search(delta) {
                                Some(entry) => {
                                    screen.scroll_to_entry(entry);
                                    screen.highlight.set(Some(entry));
                                }
                                None => screen.push_notice(
                                    Level::Info,
                                    "no transcript search yet — press Ctrl+F to search",
                                ),
                            }
                        }
                        KeyCode::Esc => {
                            if screen.busy {
                                if let Some(flag) = &current_cancel {
                                    flag.store(true, Ordering::Relaxed);
                                    screen.cancelling = true;
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
                            let note =
                                persist_note(sess, "theme", &format!("\"{}\"", next.as_str()));
                            screen.push_notice(
                                Level::Info,
                                format!("theme -> {}{note}", next.as_str()),
                            );
                        }
                        KeyCode::Char('o')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            let on = screen.transcript.toggle_details();
                            let note =
                                persist_note(sess, "details", if on { "true" } else { "false" });
                            screen.push_notice(
                                Level::Info,
                                format!(
                                    "tool detail -> {}{note}",
                                    if on { "full" } else { "collapsed" }
                                ),
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
                                if let Some(cmdline) = text.strip_prefix('/') {
                                    let verb = cmdline
                                        .split_whitespace()
                                        .next()
                                        .map(|v| format!("/{v}"))
                                        .unwrap_or_default();
                                    if super::console::works_while_busy(&verb) {
                                        run_command(
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
                                    } else {
                                        screen.push_notice(
                                            Level::Warn,
                                            format!(
                                                "`{verb}` needs the agent — wait for this task, or Esc to interrupt"
                                            ),
                                        );
                                    }
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
                            let token = super::console::pending_command(&screen.input.text);
                            if token.is_some() {
                                screen.complete_command_input();
                            } else if screen.input.text.trim_start().starts_with('/') {
                                if let Some(note) = screen.complete_argument(
                                    &sess.cfg,
                                    sess.provider.as_deref(),
                                    sess.model.as_deref(),
                                ) {
                                    screen.push_notice(Level::Info, note);
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
                        KeyCode::Char('z') | KeyCode::Char('Z')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            if key.modifiers.contains(KeyModifiers::SHIFT) {
                                screen.input.redo();
                            } else {
                                screen.input.undo();
                            }
                        }
                        KeyCode::Char('y')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            screen.input.redo();
                        }
                        KeyCode::Left
                            if key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            screen.input.word_left();
                        }
                        KeyCode::Right
                            if key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            screen.input.word_right();
                        }
                        KeyCode::Delete if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            screen.input.delete_word_forward()
                        }
                        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            screen.input.delete_word()
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
                Some(Ok(CtEvent::Mouse(me))) => {
                    if screen.has_approval() {
                        continue;
                    }
                    if screen.overlay.is_some() {
                        let body = screen.overlay_body.get();
                        match me.kind {
                            MouseEventKind::ScrollUp => screen.overlay_scroll(-1, body),
                            MouseEventKind::ScrollDown => screen.overlay_scroll(1, body),
                            MouseEventKind::Down(MouseButton::Left) => {
                                screen.overlay_click(me.row);
                            }
                            _ => {}
                        }
                        continue;
                    }
                    match me.kind {
                        MouseEventKind::ScrollUp => screen.scroll_up(3),
                        MouseEventKind::ScrollDown => screen.scroll_down(3),
                        MouseEventKind::Down(MouseButton::Left) => {
                            if let Some(hit) = screen.input_hit.get() {
                                if me.row >= hit.area.y
                                    && me.row < hit.area.bottom()
                                    && me.column >= hit.area.x
                                    && me.column < hit.area.right()
                                {
                                    let row =
                                        (me.row - hit.area.y) as usize + hit.first;
                                    let mut col = (me.column - hit.area.x) as usize;
                                    if row == hit.first {
                                        col = col.saturating_sub(hit.prompt);
                                    }
                                    if let Some(idx) =
                                        screen.input.caret_at_visual(row, col, hit.budget)
                                    {
                                        screen.input.set_cursor(idx);
                                    }
                                    continue;
                                }
                            }
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
                        MouseEventKind::Down(MouseButton::Right) => {
                            let area = screen.body_area.get();
                            if area.width > 0 && me.row >= area.y && me.row < area.bottom() {
                                let row = (me.row - area.y) as usize;
                                let owner = screen.hit_owner.borrow().get(row).copied();
                                if let Some(idx) = owner.filter(|i| *i != usize::MAX) {
                                    let text = screen.transcript.entry_text(idx).unwrap_or_default();
                                    if text.trim().is_empty() {
                                        screen.push_notice(Level::Info, "nothing to copy here");
                                    } else {
                                        match copy_to_clipboard(&text) {
                                            Ok(how) => screen.push_notice(
                                                Level::Success,
                                                format!(
                                                    "copied entry ({} chars, {how})",
                                                    text.chars().count()
                                                ),
                                            ),
                                            Err(e) => screen.push_notice(
                                                Level::Error,
                                                format!("copy failed: {e}"),
                                            ),
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Some(Ok(CtEvent::Paste(text))) => {
                    if !screen.has_approval() && screen.overlay.is_none() {
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
        // Always answer the OLDEST pending prompt: the queue order is the
        // order the agents asked in, so "y" can only ever approve the command
        // printed on screen.
        let Some(p) = screen.front_approval_mut() else {
            return;
        };
        match key.code {
            KeyCode::PageUp => {
                p.scroll = p.scroll.saturating_sub(8);
                return;
            }
            KeyCode::PageDown => {
                p.scroll = p.scroll.saturating_add(8);
                return;
            }
            KeyCode::Up => {
                p.scroll = p.scroll.saturating_sub(1);
                return;
            }
            KeyCode::Down => {
                p.scroll = p.scroll.saturating_add(1);
                return;
            }
            _ => {}
        }
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
        let Some(p) = screen.approvals.pop_front() else {
            return;
        };
        let verdict = match &decision {
            Decision::Allow => "allowed",
            Decision::AllowSession => "allowed (always, this command)",
            Decision::Deny(None) => "denied",
            Decision::Deny(Some(_)) => "denied with a reason",
        };
        let _ = p.ask.reply.send(decision);
        let more = screen.approvals.len();
        screen.push_notice(
            if verdict.starts_with("denied") {
                Level::Warn
            } else {
                Level::Info
            },
            format!(
                "approval: {verdict} ({}, task #{}){}",
                p.ask.origin.label(),
                p.ask.task,
                if more > 0 {
                    format!(" — {more} more waiting")
                } else {
                    String::new()
                }
            ),
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
    fn input_undo_redo_and_word_motion() {
        let mut i = Input::default();
        i.insert_str("one two three");
        i.break_group();
        i.insert('!');
        assert_eq!(i.text, "one two three!");
        assert!(i.undo());
        assert_eq!(i.text, "one two three");
        assert!(i.undo());
        assert_eq!(i.text, "");
        assert!(!i.undo());
        assert!(i.redo());
        assert_eq!(i.text, "one two three");
        assert!(i.redo());
        assert_eq!(i.text, "one two three!");

        // word-wise motions land on word starts, not whitespace
        i.end();
        i.word_left();
        assert_eq!(i.cursor, 8, "start of `three`");
        i.word_left();
        assert_eq!(i.cursor, 4, "start of `two`");
        i.word_right();
        assert_eq!(i.cursor, 8, "start of the next word (`three`)");
        i.word_right();
        assert_eq!(i.cursor, 14, "end of the buffer");

        // forward word deletion keeps the word before the caret
        i.end();
        i.word_left();
        i.delete_word_forward();
        assert_eq!(i.text, "one two !");
        i.home();
        i.delete_word_forward();
        assert_eq!(i.text, " two !");
    }

    #[test]
    fn session_usage_totals_accumulate_and_reset() {
        let mut s = test_screen();
        assert!(s.usage_report().contains("0 finished task(s)"));
        s.tokens_in = 100;
        s.tokens_out = 20;
        s.finish_task_usage();
        assert_eq!((s.total_in, s.total_out, s.tasks_done), (100, 20, 1));
        assert_eq!((s.tokens_in, s.tokens_out), (0, 0), "per-task reset");
        s.tokens_in = 5;
        s.tokens_out = 7;
        s.finish_task_usage();
        let report = s.usage_report();
        assert!(
            report.contains("105") && report.contains("27") && report.contains("2 finished"),
            "{report}"
        );
    }

    #[test]
    fn ghost_completion_hints_unique_commands() {
        let mut s = test_screen();
        s.input.set_text("/he".into());
        assert_eq!(s.ghost_completion().as_deref(), Some("lp"));
        s.input.set_text("/co".into());
        assert!(
            s.ghost_completion().is_none(),
            "ambiguous prefix stars no ghost"
        );
        s.input.set_text("plain task".into());
        assert!(s.ghost_completion().is_none());
        s.input.set_text("/help".into());
        assert!(s.ghost_completion().is_none(), "already complete");

        s.input.set_text("/he".into());
        let mut term = Terminal::new(TestBackend::new(60, 10)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let text = frame_rows(&term).join("\n");
        assert!(
            text.contains("/help"),
            "ghost completes the name on screen:\n{text}"
        );
    }

    #[test]
    fn base64_encodes_for_the_osc52_clipboard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        // nothing to copy is never written to the terminal
        assert!(!osc52_copy(""));
    }

    #[test]
    fn f3_walks_the_remembered_search_hits() {
        let mut s = test_screen();
        s.push_notice(Level::Info, "alpha needle");
        s.push_notice(Level::Info, "beta");
        s.push_notice(Level::Info, "needle again");
        let hits = s.transcript.search("needle");
        assert_eq!(hits.len(), 2);
        s.remember_search(hits, 0);
        assert_eq!(s.step_search(1), Some(2));
        assert_eq!(s.step_search(1), Some(0), "wraps to the first hit");
        assert_eq!(s.step_search(-1), Some(2), "backwards wraps");
        assert!(test_screen().step_search(1).is_none(), "no search yet");
    }

    #[test]
    fn search_highlight_paints_the_jumped_entry() {
        let theme = Theme::default();
        let mut s = test_screen();
        s.push_notice(Level::Info, "needle lives here");
        s.highlight.set(Some(0));
        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let painted = (0..buf.area.height).any(|y| {
            (0..buf.area.width).any(|x| {
                let cell = &buf[(x, y)];
                cell.bg == theme.sel_bg && cell.fg == theme.sel_fg
            })
        });
        assert!(painted, "the selected entry gets the selection colours");
    }

    #[test]
    fn click_position_maps_back_to_a_char_index() {
        let mut i = Input::default();
        i.insert_str("hello world");
        assert_eq!(i.caret_at_visual(0, 0, 5), Some(0));
        assert_eq!(i.caret_at_visual(0, 3, 5), Some(3));
        assert_eq!(i.caret_at_visual(0, 99, 5), Some(5), "past the row end");
        assert_eq!(
            i.caret_at_visual(1, 0, 5),
            Some(6),
            "second row starts at `world`"
        );
        assert_eq!(i.caret_at_visual(1, 2, 5), Some(8));
        assert_eq!(i.caret_at_visual(9, 0, 5), None);

        // multi-line: row indices continue across the newline
        let mut m = Input::default();
        m.insert_str("ab\ncd");
        assert_eq!(m.caret_at_visual(1, 0, 10), Some(3));
        assert_eq!(m.caret_at_visual(1, 2, 10), Some(5));
    }

    #[test]
    fn command_tab_completes_unique_prefix_to_the_real_name() {
        let mut s = test_screen();
        s.input.set_text("/he".into());
        assert!(s.complete_command_input());
        assert_eq!(s.input.text, "/help", "must not leave a half command");

        s.input.set_text("/pro".into());
        assert!(s.complete_command_input());
        assert_eq!(s.input.text, "/provider ", "argument commands get a space");

        s.input.set_text("/co".into());
        assert!(s.complete_command_input());
        assert_eq!(s.input.text, "/co", "ambiguous prefix stays put");
        assert!(s.cmd_sel.get() > 0, "Tab cycles the candidate list");

        s.input.set_text("plain task".into());
        assert!(!s.complete_command_input());
    }

    #[test]
    fn command_argument_tab_completion() {
        let cfg: crate::config::Config =
            toml::from_str("[providers.deepseek]\ntype = \"openai\"\nmodel = \"deepseek-chat\"")
                .unwrap();
        let mut s = test_screen();
        // `re` is ambiguous now (read-only | review): Tab offers the choices
        s.input.set_text("/permission re".into());
        let note = s
            .complete_argument(&cfg, None, None)
            .expect("two modes share the prefix");
        assert!(
            note.contains("read-only") && note.contains("review"),
            "{note}"
        );
        assert_eq!(s.input.text, "/permission re");
        // one more letter decides
        s.input.set_text("/permission rea".into());
        assert!(s.complete_argument(&cfg, None, None).is_none());
        assert_eq!(s.input.text, "/permission read-only ");

        s.input.set_text("/effort de".into());
        assert!(s.complete_argument(&cfg, None, None).is_none());
        assert_eq!(s.input.text, "/effort deep ");

        s.input.set_text("/theme so".into());
        assert!(s.complete_argument(&cfg, None, None).is_none());
        assert_eq!(s.input.text, "/theme solarized ");

        s.input.set_text("/provider de".into());
        assert!(s.complete_argument(&cfg, None, None).is_none());
        assert_eq!(s.input.text, "/provider deepseek ");

        // no shared extension beyond `o`: the next Tab lists the candidates
        s.input.set_text("/yolo ".into());
        assert!(s.complete_argument(&cfg, None, None).is_none());
        assert_eq!(s.input.text, "/yolo o");
        let note = s.complete_argument(&cfg, None, None).expect("two choices");
        assert!(note.contains("on") && note.contains("off"), "{note}");

        // unknown argument stays untouched
        s.input.set_text("/permission bogus".into());
        assert!(s.complete_argument(&cfg, None, None).is_none());
        assert_eq!(s.input.text, "/permission bogus");
    }

    #[test]
    fn mouse_click_selects_an_overlay_row() {
        let mut s = test_screen();
        s.overlay = Some(Overlay::Picker(Picker::new(
            PickerKind::Model,
            "models",
            vec!["a/one".into(), "a/two".into(), "a/three".into()],
            None,
        )));
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let hit = s.overlay_hit.get().expect("list hit map");
        assert!(hit.shown >= 3);
        assert!(s.overlay_click(hit.area.y + 2));
        match s.overlay.as_ref().unwrap() {
            Overlay::Picker(p) => assert_eq!(p.cursor, 2),
            other => panic!("unexpected overlay: {other:?}"),
        }
        // a click outside the list rows is ignored
        assert!(!s.overlay_click(0));
    }

    #[test]
    fn diff_card_and_overlay_show_the_patch() {
        let mut s = test_screen();
        s.transcript.start_tool("d1", "apply_patch", "src/x.rs");
        s.transcript.finish_tool(
            "d1",
            "apply_patch",
            false,
            "patched",
            Some("@@ -1 +1 @@\n-old\n+new".into()),
            4,
        );
        let (name, summary, diff) = s.transcript.diff_card(1).expect("diff card");
        assert_eq!(name, "apply_patch");
        assert_eq!(summary, "src/x.rs");
        assert!(diff.contains("+new"));
        assert!(s.transcript.diff_card(2).is_none());

        s.overlay = Some(Overlay::Diff(overlay::DiffView::new(
            "apply_patch src/x.rs",
            diff,
        )));
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| s.draw(f)).unwrap();
        let text = frame_rows(&term).join("\n");
        assert!(text.contains("diff · apply_patch"), "{text}");
        assert!(text.contains("+new"), "{text}");
    }

    #[test]
    fn queue_report_lists_and_drops_follow_ups() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let slot = Arc::new(tokio::sync::Mutex::new(Some(stub_agent())));
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalAsk>();
            let mut sess = SessionState {
                cfg: crate::config::Config::default(),
                provider: None,
                model: None,
                session_name: "sess".to_string(),
                config_file: None,
                jobs: None,
            };
            let mut screen = test_screen();
            let perm = crate::tools::Permission::default();
            let allows = SessionAllows::default();
            let (bg_tx, _bg_rx) = tokio::sync::mpsc::unbounded_channel::<BackgroundEvent>();
            screen.queued.push_back("first task".into());
            screen.queued.push_back("second task".into());

            run_command(
                "queue",
                &mut screen,
                &slot,
                &mut sess,
                &tx,
                &allows,
                &perm,
                &bg_tx,
            )
            .await;
            let text = screen_text(&screen);
            assert!(text.contains("1. first task"), "{text}");
            assert!(text.contains("2. second task"), "{text}");

            run_command(
                "queue drop 1",
                &mut screen,
                &slot,
                &mut sess,
                &tx,
                &allows,
                &perm,
                &bg_tx,
            )
            .await;
            assert_eq!(screen.queued.len(), 1);
            assert_eq!(screen.queued[0], "second task");

            run_command(
                "queue clear",
                &mut screen,
                &slot,
                &mut sess,
                &tx,
                &allows,
                &perm,
                &bg_tx,
            )
            .await;
            assert!(screen.queued.is_empty());

            allows.lock().unwrap().push("$ cargo test".to_string());
            run_command(
                "allow",
                &mut screen,
                &slot,
                &mut sess,
                &tx,
                &allows,
                &perm,
                &bg_tx,
            )
            .await;
            assert!(screen_text(&screen).contains("cargo test"));
            run_command(
                "allow clear",
                &mut screen,
                &slot,
                &mut sess,
                &tx,
                &allows,
                &perm,
                &bg_tx,
            )
            .await;
            assert!(allows.lock().unwrap().is_empty());

            // while a task runs the agent slot is empty; status/context/plan
            // still answer from the screen instead of saying "busy"
            let busy_slot = Arc::new(tokio::sync::Mutex::new(None));
            screen.todos = vec![
                ("write the test".into(), "completed".into()),
                ("fix the bug".into(), "in_progress".into()),
            ];
            for (cmd, needle) in [
                ("status", "rounds so far"),
                ("context", "agent is busy"),
                ("plan", "fix the bug"),
            ] {
                run_command(
                    cmd,
                    &mut screen,
                    &busy_slot,
                    &mut sess,
                    &tx,
                    &allows,
                    &perm,
                    &bg_tx,
                )
                .await;
                let text = screen_text(&screen);
                assert!(text.contains(needle), "{cmd}: {text}");
            }
            assert!(screen_text(&screen).contains("[x] write the test"));
        });
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
                Overlay::Diff(overlay::DiffView::new(
                    "apply_patch src/x.rs",
                    "@@ -1 +1 @@\n-old\n+new",
                )),
            ] {
                s.overlay = Some(overlay);
                term.draw(|f| s.draw(f)).unwrap();
            }
            s.overlay = None;
            let (tx, _rx) = tokio::sync::oneshot::channel();
            s.push_approval(PendingApproval {
                ask: ApprovalAsk {
                    id: next_ask_id(),
                    task: 1,
                    origin: AskOrigin::Main,
                    tool: "run_command".into(),
                    summary: "$ git push --force\n  cwd: /tmp\nsecond".into(),
                    reply: tx,
                },
                reason_mode: false,
                reason: String::new(),
                scroll: 0,
            });
            term.draw(|f| s.draw(f)).unwrap();
            s.approvals.clear();
            s.scroll_to_top();
            term.draw(|f| s.draw(f)).unwrap();
        }
    }

    #[test]
    fn approval_keys_answer_the_agent() {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let mut s = test_screen();
        s.push_approval(PendingApproval {
            ask: ApprovalAsk {
                id: next_ask_id(),
                task: 1,
                origin: AskOrigin::Main,
                tool: "run_command".into(),
                summary: "$ rm -rf build".into(),
                reply: tx,
            },
            reason_mode: false,
            reason: String::new(),
            scroll: 0,
        });
        let key = |code| ratatui::crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        // long commands scroll instead of being clipped
        handle_approval(&mut s, &key(KeyCode::PageDown));
        assert_eq!(s.front_approval().unwrap().scroll, 8);
        handle_approval(&mut s, &key(KeyCode::Up));
        assert_eq!(s.front_approval().unwrap().scroll, 7);
        // `r` opens the reason line; typing + Enter denies with the reason
        handle_approval(&mut s, &key(KeyCode::Char('r')));
        assert!(s.front_approval().unwrap().reason_mode);
        for c in "use --dry-run".chars() {
            handle_approval(&mut s, &key(KeyCode::Char(c)));
        }
        handle_approval(&mut s, &key(KeyCode::Enter));
        assert!(!s.has_approval());
        assert_eq!(
            rx.try_recv().unwrap(),
            Decision::Deny(Some("use --dry-run".into()))
        );

        // plain `y` allows once
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.push_approval(PendingApproval {
            ask: ApprovalAsk {
                id: next_ask_id(),
                task: 1,
                origin: AskOrigin::Main,
                tool: "run_command".into(),
                summary: "$ ls".into(),
                reply: tx,
            },
            reason_mode: false,
            reason: String::new(),
            scroll: 0,
        });
        handle_approval(&mut s, &key(KeyCode::Char('y')));
        assert_eq!(rx.try_recv().unwrap(), Decision::Allow);
    }

    #[test]
    fn queued_approvals_are_answered_oldest_first() {
        // 0.18 kept ONE slot: a prompt arriving while another was on screen
        // replaced it, and the replaced agent got a silent Deny.
        let mut s = test_screen();
        let (tx1, mut rx1) = tokio::sync::oneshot::channel();
        let (tx2, mut rx2) = tokio::sync::oneshot::channel();
        for (tx, tool, origin) in [
            (tx1, "run_command", AskOrigin::Main),
            (tx2, "run_command", AskOrigin::SubAgent),
        ] {
            s.push_approval(PendingApproval {
                ask: ApprovalAsk {
                    id: next_ask_id(),
                    task: 7,
                    origin,
                    tool: tool.into(),
                    summary: "$ cargo test".into(),
                    reply: tx,
                },
                reason_mode: false,
                reason: String::new(),
                scroll: 0,
            });
        }
        assert_eq!(s.approvals.len(), 2, "the second ask must not overwrite");

        let key = |code| ratatui::crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
        handle_approval(&mut s, &key(KeyCode::Char('y')));
        assert_eq!(
            rx1.try_recv().unwrap(),
            Decision::Allow,
            "oldest answered first"
        );
        assert!(rx2.try_recv().is_err(), "the second prompt still waits");
        assert_eq!(s.approvals.len(), 1);

        handle_approval(&mut s, &key(KeyCode::Char('n')));
        assert_eq!(rx2.try_recv().unwrap(), Decision::Deny(None));
        assert!(!s.has_approval());
    }

    #[test]
    fn finishing_a_task_answers_its_orphaned_prompts() {
        let mut s = test_screen();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        s.push_approval(PendingApproval {
            ask: ApprovalAsk {
                id: next_ask_id(),
                task: 9,
                origin: AskOrigin::Main,
                tool: "run_command".into(),
                summary: "$ rm -rf build".into(),
                reply: tx,
            },
            reason_mode: false,
            reason: String::new(),
            scroll: 0,
        });
        s.deny_pending_approvals("task already finished");
        assert!(!s.has_approval());
        match rx.try_recv().unwrap() {
            Decision::Deny(Some(reason)) => assert!(reason.contains("already finished")),
            other => panic!("expected a denial with a reason, got {other:?}"),
        }
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
            staged: None,
            reviewer: None,
            jobs: None,
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
    async fn a_project_command_expands_into_the_task() {
        let slot = Arc::new(tokio::sync::Mutex::new(Some(stub_agent())));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalAsk>();
        let mut sess = SessionState {
            cfg: crate::config::Config::default(),
            provider: None,
            model: None,
            session_name: "sess".to_string(),
            config_file: None,
            jobs: None,
        };
        let mut screen = test_screen();
        screen.customs = vec![super::super::custom::CustomCommand {
            name: "/review".into(),
            description: "Review a file".into(),
            argument_hint: "<path>".into(),
            body: "review this carefully: $ARGUMENTS".into(),
            path: std::path::PathBuf::from("/repo/.kazi/commands/review.md"),
            scope: super::super::custom::Scope::Project,
        }];
        let perm = crate::tools::Permission::default();
        let allows = SessionAllows::default();
        let (bg_tx, _bg_rx) = tokio::sync::mpsc::unbounded_channel::<BackgroundEvent>();

        let follow = run_command(
            "review src/main.rs",
            &mut screen,
            &slot,
            &mut sess,
            &tx,
            &allows,
            &perm,
            &bg_tx,
        )
        .await
        .expect("a project command yields a task");
        assert_eq!(follow, "review this carefully: src/main.rs");
        // the dropdown and help know about it too
        screen.input.set_text("/rev".to_string());
        let rows = screen.command_hints();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "/review <path>");
        assert!(!screen.help_extra().is_empty());
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
            config_file: None,
            jobs: None,
        };
        let mut screen = test_screen();
        let perm = crate::tools::Permission::default();
        let allows = SessionAllows::default();
        let (bg_tx, _bg_rx) = tokio::sync::mpsc::unbounded_channel::<BackgroundEvent>();

        for cmd in [
            "help",
            "help theme",
            "status",
            "auth",
            "last",
            "diff",
            "queue",
            "plan",
            "usage",
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
    fn rename_repoints_autosave_and_keeps_the_old_file() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let slot = Arc::new(tokio::sync::Mutex::new(Some(stub_agent())));
            if let Some(a) = slot.lock().await.as_mut() {
                a.history.push(crate::llm::Message::user("hello"));
            }
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalAsk>();
            let mut sess = SessionState {
                cfg: crate::config::Config::default(),
                provider: None,
                model: None,
                session_name: "old-name".to_string(),
                config_file: None,
                jobs: None,
            };
            let mut screen = test_screen();
            let perm = crate::tools::Permission::default();
            let allows = SessionAllows::default();
            let (bg_tx, _bg_rx) = tokio::sync::mpsc::unbounded_channel::<BackgroundEvent>();
            let new_name = format!("rename-test-{}", std::process::id());

            run_command(
                &format!("rename {new_name}"),
                &mut screen,
                &slot,
                &mut sess,
                &tx,
                &allows,
                &perm,
                &bg_tx,
            )
            .await;
            assert_eq!(sess.session_name, new_name);
            assert_eq!(screen.session_name, new_name);
            let text = screen_text(&screen);
            assert!(text.contains("auto-save now targets"), "{text}");
            assert!(
                super::super::session::load(&new_name).is_ok(),
                "conversation snapshot written"
            );
            let _ = super::super::session::delete(&new_name);

            run_command(
                "rename ",
                &mut screen,
                &slot,
                &mut sess,
                &tx,
                &allows,
                &perm,
                &bg_tx,
            )
            .await;
            assert!(
                screen_text(&screen).contains("current session:"),
                "bare /rename explains the current name"
            );
        });
    }

    #[test]
    fn ui_choices_are_persisted_to_the_config_file() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let dir = std::env::temp_dir().join(format!("kazi-tui-ui-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("config.toml");
            std::fs::write(
                &path,
                "# keep\n[providers.a]\ntype = \"openai\"\nmodel = \"m\"\n",
            )
            .unwrap();

            let slot = Arc::new(tokio::sync::Mutex::new(Some(stub_agent())));
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalAsk>();
            let mut sess = SessionState {
                cfg: crate::config::Config::default(),
                provider: None,
                model: None,
                session_name: "sess".to_string(),
                config_file: Some(path.clone()),
                jobs: None,
            };
            let mut screen = test_screen();
            let perm = crate::tools::Permission::default();
            let allows = SessionAllows::default();
            let (bg_tx, _bg_rx) = tokio::sync::mpsc::unbounded_channel::<BackgroundEvent>();

            for cmd in ["theme nord", "details on"] {
                run_command(
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
            }
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("# keep"), "comments survive:\n{text}");
            assert!(text.contains("theme = \"nord\""), "{text}");
            assert!(text.contains("details = true"), "{text}");
            let parsed: crate::config::Config = toml::from_str(&text).unwrap();
            assert_eq!(parsed.ui.theme.as_deref(), Some("nord"));
            assert_eq!(parsed.ui.details, Some(true));
            assert!(
                screen_text(&screen).contains(" · saved"),
                "the notice tells the user it stuck: {}",
                screen_text(&screen)
            );

            // a session with no config file never claims to have saved
            sess.config_file = None;
            run_command(
                "theme light",
                &mut screen,
                &slot,
                &mut sess,
                &tx,
                &allows,
                &perm,
                &bg_tx,
            )
            .await;
            assert!(!screen_text(&screen).contains("theme -> light · saved"));
            let _ = std::fs::remove_dir_all(&dir);
        });
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
                config_file: None,
                jobs: None,
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
                vision: None,
                r#type: "openai".into(),
                model: Some("deepseek-chat".into()),
                base_url: None,
                api_key_env: None,
                models: None,
                no_auth: None,
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
