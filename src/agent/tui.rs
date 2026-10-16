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

/// Everything needed to render one screenful.
struct Screen {
    provider_desc: String,
    session_name: String,
    max_rounds: u32,
    /// (kind, text) transcript lines; scrolling shows the tail.
    lines: Vec<(LineKind, String)>,
    input: String,
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
            input: String::new(),
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
        let t = self.input.trim().to_string();
        self.input.clear();
        if t.is_empty() {
            None
        } else {
            self.push(LineKind::User, format!("you> {t}"));
            Some(t)
        }
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
        let shown: String = if visible_width(&self.input) + visible_width(prompt)
            > (input.width as usize).saturating_sub(2)
        {
            let keep = (input.width as usize).saturating_sub(2 + visible_width(prompt));
            self.input
                .chars()
                .rev()
                .take(keep)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        } else {
            self.input.clone()
        };
        let input_text = Line::from(vec![
            Span::styled(prompt, Style::default().fg(CYAN).bold()),
            Span::raw(&shown),
        ]);
        f.render_widget(
            Paragraph::new(input_text).block(Block::default().borders(Borders::TOP)),
            input,
        );

        // cursor
        f.set_cursor_position(ratatui::layout::Position {
            x: (input.x + 1 + prompt.len() as u16 + shown.len() as u16)
                .min(input.right().saturating_sub(1)),
            y: input.y + 1,
        });
    }
}

/// Word-ish wrap: break preferentially at spaces, hard at `max` otherwise.
fn wrap_text(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in s.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for word in para.split(' ') {
            // hard-break overly long words
            if word.len() > max {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                let mut start = 0usize;
                while start < word.len() {
                    let end = (start + max).min(word.len());
                    out.push(word[start..end].to_string());
                    start = end;
                }
                continue;
            }
            if !cur.is_empty() && cur.len() + 1 + word.len() > max {
                out.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
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
    mut agent: Agent,
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

    let res = tui_inner(&mut terminal, &mut screen, &mut agent, &on_session_save).await;

    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    std::io::stdout().flush().ok();

    let quit_clean = screen.quit;
    if screen.lines.len() > 1 {
        on_session_save(&agent);
    }
    res.map(|_| if quit_clean { 0 } else { 1 })
}

/// Width of a string as terminal columns — chars here is close enough for
/// ASCII-heavy content and avoids pulling unicode-width directly.
fn visible_width(s: &str) -> usize {
    s.chars().count()
}

/// Inner loop: agent tasks run in a spawned tokio task and stream their
/// events through an mpsc channel; the UI loop `select!`s between terminal
/// input and incoming events, so the screen keeps redrawing (streaming
/// deltas land live) and Esc stays responsive even while a task runs.
/// Esc during a busy task aborts the TUI but the task result is still saved.
async fn tui_inner(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    screen: &mut Screen,
    agent: &mut Agent,
    on_session_save: &(impl Fn(&Agent) + ?Sized),
) -> anyhow::Result<()> {
    // Agent handoff: the runner task owns the agent while a task runs (the
    // engine needs `&mut`), then parks it back here for the next task and
    // for the final save. `Option` because it's empty only DURING a run.
    let shared_agent: std::sync::Arc<tokio::sync::Mutex<Option<Agent>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));

    // First fill: park the freshly built agent. Then the loop drains it on
    // demand instead of `*agent` borrows.
    {
        let mut slot = shared_agent.lock().await;
        *slot = Some(std::mem::replace(agent, parked_placeholder()));
    }

    enum Pump {
        Ev(AgentEvent),
        Done(Result<String, super::AgentError>),
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Pump>(256);
    let mut runner: Option<tokio::task::JoinHandle<()>> = None;

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
                    if let Err(e) = res {
                        screen.push(LineKind::Warn, format!("error: {e}"));
                    }
                    screen.busy = false;
                    runner = None;
                }
                None => { screen.busy = false; runner = None; }
            },

            // ---- input pump (async EventStream) ----------------------------
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(CtEvent::Key(key))) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc => {
                        screen.quit = true;
                        if let Some(r) = runner.take() { r.abort(); }
                    }
                    KeyCode::Enter if !screen.busy => {
                        if let Some(task) = screen.take_input() {
                            screen.busy = true;
                            // take agent out of the slot for the runner
                            let taken = shared_agent.lock().await.take();
                            if let Some(owned) = taken {
                                let slot = shared_agent.clone();
                                let tx = tx.clone();
                                runner = Some(tokio::spawn(async move {
                                    let mut owned = owned;
                                    let out = owned.run_task(&task, |ev| {
                                        let _ = tx.blocking_send(Pump::Ev(ev));
                                    }).await;
                                    // park the agent back first, THEN report
                                    slot.lock().await.replace(owned);
                                    let _ = tx.send(Pump::Done(out)).await;
                                }));
                            }
                        }
                    }
                    KeyCode::Backspace => { screen.input.pop(); }
                    KeyCode::Char(c)
                        if key.modifiers.contains(ratatui::crossterm::event::KeyModifiers::CONTROL)
                            && c == 'l' =>
                    {
                        screen.lines.clear();
                    }
                    KeyCode::Char(c) => screen.input.push(c),
                    _ => {}
                },
                Some(Ok(_)) => {} // resize/focus/mouse: next draw picks size up
                Some(Err(_)) | None => {} // event loop hiccup: keep going
            },
        }
    }

    // retrieve the agent for the caller's save hook
    let returned = shared_agent.lock().await.take();
    if let Some(a) = returned {
        *agent = a;
    }
    if screen.lines.len() > 1 {
        on_session_save(agent);
    }
    Ok(())
}

/// Placeholder only to satisfy `std::mem::replace` types; immediately
/// overwritten by take() semantics. Not a real agent.
fn parked_placeholder() -> Agent {
    unreachable!("see repl.rs — placeholder should never be constructed")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        s.input = "  hello  ".into();
        let task = s.take_input().unwrap();
        assert_eq!(task, "hello");
        assert!(s.input.is_empty());
        assert!(s.lines.last().unwrap().1.contains("you> hello"));
        assert!(s.take_input().is_none(), "empty input = no task");
    }
}
