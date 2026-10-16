//! Full-screen TUI for `kazi chat` (`--tui`), built on ratatui + crossterm.
//!
//! Layout: one-row status bar (provider, model, session, rounds, tokens) on
//! top; scrolling transcript in the middle; one-row input at the bottom.
//! Tool activity renders inside the transcript as `>> tool [ok] preview`
//! lines, same visual language as the classic REPL. Any terminal error
//! falls back to the classic line REPL — the TUI is never load-bearing.
//!
//! Input: Enter submits, Esc exits, Ctrl-L clears, typed text supports
//! backspace; the input keeps prior text across redraws. Mouse events are
//! ignored; selection stays native (terminal copy).

use futures::StreamExt;
use std::io::{Stdout, Write};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    DisableMouseCapture, Event as CtEvent, EventStream, KeyCode, KeyEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

use super::{Agent, AgentEvent};

const CYAN: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const GREEN: Color = Color::Green;
const RED: Color = Color::Red;
const YELLOW: Color = Color::Yellow;

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
    /// While true, input is disabled (a task is in flight).
    busy: bool,
    /// Set when the user asked to quit.
    quit: bool,
}

impl Screen {
    fn new(provider_desc: &str, session_name: &str, max_rounds: u32) -> Self {
        Self {
            provider_desc: provider_desc.to_string(),
            session_name: session_name.to_string(),
            max_rounds,
            lines: Vec::new(),
            input: Input::default(),
            rounds: 0,
            tokens_in: 0,
            tokens_out: 0,
            est_tokens: 0,
            streaming: String::new(),
            busy: false,
            quit: false,
        }
    }

    fn push(&mut self, kind: LineKind, text: impl Into<String>) {
        self.lines.push((kind, text.into()));
    }

    /// Render one agent event into the screen state.
    fn consume(&mut self, ev: AgentEvent) {
        match ev {
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
        Some(t)
    }

    /// Paint one frame. Height budget: status 1, input 3, transcript = rest.
    fn draw(&self, f: &mut Frame) {
        let [status, body, input] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .areas(f.area());

        // -- status bar -----------------------------------------------------
        let budget_note = if self.busy {
            format!(
                "round {}/{}  in {} out {}  ctx ~{}",
                self.rounds, self.max_rounds, self.tokens_in, self.tokens_out, self.est_tokens
            )
        } else {
            format!("ctx ~{}", self.est_tokens)
        };
        let status_line = Line::from(vec![
            Span::styled(" kazi ", Style::default().fg(Color::Black).bg(CYAN).bold()),
            Span::raw(" "),
            Span::styled(&self.provider_desc, Style::default().fg(DIM)),
            Span::raw("  "),
            Span::styled(&self.session_name, Style::default().fg(CYAN)),
            Span::raw("  "),
            Span::styled(budget_note, Style::default().fg(DIM)),
        ]);
        f.render_widget(status_line.style(Style::new()), status);

        // -- transcript ------------------------------------------------------
        let mut text = Text::default();
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
            // naive wrap at a comfortable width; ratatui Paragraph would also
            // wrap, but pre-wrapping keeps the scroll arithmetic honest.
            let width = body.width.saturating_sub(2) as usize;
            for chunk in wrap_text(content, width.max(20)) {
                text.lines.push(Line::styled(chunk, style));
            }
        }
        // streaming assistant text lives at the bottom of the transcript
        if !self.streaming.is_empty() {
            for chunk in wrap_text(
                &self.streaming,
                body.width.saturating_sub(2).max(20) as usize,
            ) {
                text.lines.push(Line::raw(chunk));
            }
        }
        f.render_widget(Paragraph::new(text), body);

        // -- input box -------------------------------------------------------
        let prompt = if self.busy { " [busy] " } else { " kazi> " };
        let budget = (input.width as usize)
            .saturating_sub(1)
            .saturating_sub(UnicodeWidthStr::width(prompt));
        let (shown, caret_in_window) =
            window_for_cursor(&self.input.text, self.input.cursor, budget);
        let input_text = Line::from(vec![
            Span::styled(prompt, Style::default().fg(CYAN).bold()),
            Span::raw(&shown),
        ]);
        f.render_widget(
            Paragraph::new(input_text).block(Block::default().borders(Borders::TOP)),
            input,
        );

        // caret sits at the window offset (display columns), clamped inside
        let x = (input.x + UnicodeWidthStr::width(prompt) as u16 + caret_in_window as u16)
            .min(input.right().saturating_sub(1));
        f.set_cursor_position(ratatui::layout::Position { x, y: input.y + 1 });
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
        KeyCode::Char(c) if mods.intersects(M::CONTROL | M::ALT) => None,
        KeyCode::Char(' ') if mods.contains(M::SHIFT) => None,
        KeyCode::Char(c) => Some(*c),
        _ => None,
    }
}

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
    yolo: bool,
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
    let mut agent =
        match Agent::default_cli(&cfg, provider.as_deref(), model.as_deref(), yolo).await {
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
            None => eprintln!("no stored session matches `{name}`"),
        }
    }

    let save = |agent: &Agent| {
        let _ = super::session::save(
            &session_name,
            agent.history.system.as_deref(),
            &agent.history.msgs,
        );
    };

    match chat_loop_tui(agent, session_name.clone(), save).await {
        Ok(code) => {
            println!("\x1b[2msession saved -> {session_name}\x1b[0m");
            Ok(code)
        }
        Err(_) => {
            // terminal can't do alternate screens: classic REPL fallback
            crate::agent::repl::chat_loop(cfg, provider, model, yolo, topic, resume).await
        }
    }
}

/// Run the TUI chat loop. `chat_loop_tui` owns its terminal; stdout is
/// restored on every return path. Returns unchanged session data reduction —
/// persistence continues to live in repl.rs.
pub async fn chat_loop_tui(
    agent: Agent,
    session_name: String,
    on_session_save: impl Fn(&Agent),
) -> anyhow::Result<u8> {
    let provider_desc = agent.provider().describe();
    let max_rounds = agent.max_rounds();

    enable_raw_mode()?;
    let mut stdout: Stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut screen = Screen::new(&provider_desc, &session_name, max_rounds);
    screen.push(
        LineKind::Dim,
        "type a task and press Enter (empty line = same task again) — Esc quits".to_string(),
    );

    let res = tui_inner(&mut terminal, &mut screen, agent, &on_session_save).await;

    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    std::io::stdout().flush().ok();

    let quit_clean = screen.quit;
    // the save already happened inside tui_inner with the real agent
    // (`None` = aborted mid-task: nothing new to persist)
    res.map(|_| if quit_clean { 0 } else { 1 })
}

/// Inner loop: agent tasks run in a spawned tokio task and stream their
/// events through an mpsc channel; the UI loop `select!`s between terminal
/// input and incoming events, so the screen keeps redrawing (streaming
/// deltas land live) and Esc stays responsive even while a task runs.
/// Esc during a busy task aborts the TUI but the task result is still saved.
async fn tui_inner(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    screen: &mut Screen,
    agent: Agent,
    on_session_save: &(impl Fn(&Agent) + ?Sized),
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

    while !screen.quit {
        let _ = terminal.draw(|f| screen.draw(f));

        tokio::select! {
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
                        Err(e) => screen.push(LineKind::Warn, format!("error: {e}")),
                        Ok(_) => {}
                    }
                    screen.busy = false;
                    runner = None;
                    current_cancel = None;
                }
                None => { screen.busy = false; runner = None; current_cancel = None; }
            },

            // ---- input pump (async EventStream) ----------------------------
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(CtEvent::Key(key))) if key.kind != KeyEventKind::Release => {
                    log_key_event(&key);
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
                    KeyCode::Enter if !screen.busy => {
                        if let Some(task) = screen.take_input() {
                            screen.busy = true;
                            // take agent out of the slot for the runner
                            let taken = shared_agent.lock().await.take();
                            if let Some(owned) = taken {
                                current_cancel = Some(owned.cancel_token());
                                let slot = shared_agent.clone();
                                let tx = tx.clone();
                                runner = Some(tokio::spawn(async move {
                                    let mut owned = owned;
                                    let out = owned.run_task(&task, |ev| {
                                        let _ = tx.send(Pump::Ev(ev));
                                    }).await;
                                    // park the agent back first, THEN report
                                    slot.lock().await.replace(owned);
                                    let _ = tx.send(Pump::Done(out));
                                }));
                            }
                        }
                    }
                    KeyCode::Backspace => screen.input.backspace(),
                    KeyCode::Delete => screen.input.delete(),
                    KeyCode::Left => screen.input.left(),
                    KeyCode::Right => screen.input.right(),
                    KeyCode::Home => screen.input.home(),
                    KeyCode::End => screen.input.end(),
                    KeyCode::Up => screen.input.history_prev(),
                    KeyCode::Down => screen.input.history_next(),
                    KeyCode::Char(c)
                        if key
                            .modifiers
                            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        match c {
                            'c' => screen.quit = true,
                            'l' => screen.lines.clear(),
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
                Some(Ok(_)) => {} // resize/focus/mouse: next draw picks size up
                Some(Err(_)) | None => {} // event loop hiccup: keep going
            },
        }
    }

    // hand the agent back to the caller (None when Esc aborted a busy task)
    let returned = shared_agent.lock().await.take();
    if let Some(agent) = &returned {
        if screen.lines.len() > 1 {
            on_session_save(agent);
        }
    }
    Ok(returned)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
