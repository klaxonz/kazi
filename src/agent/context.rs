//! Conversation history and context-window management.
//!
//! Token counting is a *character estimate* (4 chars ≈ 1 token): good enough
//! to decide when to trim, cheap, and dependency-free. Trimming drops whole
//! turns (a user task plus its tool round-trips) from the oldest end so tool
//! results are never orphaned from their assistant `tool_calls` — Anthropic,
//! for one, rejects messages whose `tool_result` lacks its `tool_use` parent.

use crate::llm::{Message, Role};

#[derive(Debug, Clone)]
pub struct History {
    /// Always present as messages[0] in every request, never trimmed away.
    pub system: Option<String>,
    /// Conversation in append order (system excluded).
    pub msgs: Vec<Message>,
}

impl History {
    pub fn new(system: Option<String>) -> Self {
        Self {
            system,
            msgs: Vec::new(),
        }
    }

    pub fn push(&mut self, m: Message) {
        self.msgs.push(m);
    }

    pub fn reset_msgs(&mut self) {
        self.msgs.clear();
    }

    pub fn msgs_len(&self) -> usize {
        self.msgs.len()
    }

    /// All messages for the next request: system first.
    pub fn messages(&self) -> Vec<Message> {
        let mut out = Vec::with_capacity(self.msgs.len() + 1);
        if let Some(s) = &self.system {
            out.push(Message::system(s.clone()));
        }
        out.extend(self.msgs.iter().cloned());
        out
    }

    /// 4 chars ≈ 1 token, plus a flat margin of 32 per message overhead.
    /// Includes the system prompt.
    pub fn estimate_tokens(&self) -> u64 {
        self.system.as_deref().map(estimate_str).unwrap_or(0) + self.estimate_msgs_tokens()
    }

    /// Messages only, no system prompt — the `/context` breakdown needs the
    /// two numbers separately.
    pub fn estimate_msgs_tokens(&self) -> u64 {
        let mut n: u64 = 0;
        for m in &self.msgs {
            n += estimate_str(m.text());
            for tc in &m.tool_calls {
                n += estimate_str(&tc.name) + estimate_str(&tc.arguments.to_string()) + 32;
            }
            n += 32;
        }
        n
    }

    /// Drop whole turns (from oldest) until under `budget`. The most recent
    /// turn is never dropped — even if that means overshooting.
    pub fn trim_to_tokens(&mut self, budget: u64) {
        while self.estimate_tokens() > budget {
            match self.turn_cut() {
                Some(i) => self.msgs.drain(0..i),
                None => break,
            };
        }
    }

    /// Index of the *second* user message, i.e. the boundary after the first
    /// complete turn, or `None` when fewer than two turns exist.
    fn turn_cut(&self) -> Option<usize> {
        let users: Vec<usize> = self
            .msgs
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User)
            .map(|(i, _)| i)
            .collect();
        users.get(1).copied()
    }

    /// Compaction path: extract droppable messages from the oldest end
    /// (serialized as a transcript for summarization).
    ///
    /// Two granularities, whichever yields something:
    /// 1. **Whole completed turns** — everything before the second user
    ///    message (multi-task histories).
    /// 2. **Completed tool ROUNDS inside the current task** — a long single
    ///    task has one user message but many assistant/tool round-trips;
    ///    everything before the *second-to-last* `assistant_with_calls`
    ///    boundary is a finished round whose results the model no longer
    ///    needs verbatim. The latest round (with its pending tool results)
    ///    is always kept intact so the next provider call stays legal.
    ///
    /// `None` = nothing droppable (fewer than two rounds).
    pub fn take_beyond_text(&mut self, _budget: u64) -> Option<String> {
        if let Some(cut) = self.turn_cut() {
            let text = serialize_turns(&self.msgs[..cut]);
            self.msgs.drain(0..cut);
            return Some(text);
        }
        // single task in flight: cut at the last round boundary, keeping the
        // final (assistant-with-calls → tool results) group intact
        let cut = self.last_round_cut()?;
        let text = serialize_rounds(&self.msgs[..cut]);
        self.msgs.drain(0..cut);
        Some(text)
    }

    /// Boundary before the second-to-last assistant-with-tool-calls message,
    /// so everything before it (older rounds) can be summarized away while
    /// the newest round — and whatever it is still executing — stays whole.
    fn last_round_cut(&self) -> Option<usize> {
        let calls: Vec<usize> = self
            .msgs
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.tool_calls.is_empty())
            .map(|(i, _)| i)
            .collect();
        if calls.len() < 2 {
            return None;
        }
        // everything from the second-to-last call boundary onward stays
        Some(cutsweep(calls))
    }

    /// Append a compaction summary to the system prompt — it rides along on
    /// every future request without breaking user/assistant alternation.
    pub fn insert_summary(&mut self, text: String) {
        let label = format!("[Context summary of dropped conversation turns]\n{text}");
        self.system = Some(match &self.system {
            Some(s) => format!("{s}\n\n{label}"),
            None => label,
        });
    }
}

/// For boundaries `[c0, c1, ..., cn]` return `c(n-1)` — the older side of it
/// is droppable, everything from it onward is the newest round.
fn cutsweep(calls: Vec<usize>) -> usize {
    calls[calls.len() - 2]
}

/// Serialize round-trip messages (user task intro, assistant text, tool
/// calls/results) — same content as [`serialize_turns`] but tolerates a
/// history that starts mid-task (no leading user message).
fn serialize_rounds(msgs: &[Message]) -> String {
    if msgs.first().map(|m| m.role) == Some(Role::User) {
        return serialize_turns(msgs);
    }
    // No user message at the head: emit only assistant/tool bodies so the
    // summary prompt sees something coherent.
    let mut text = String::new();
    for m in msgs {
        match m.role {
            Role::User => {}
            _ => {
                text.push_str(&serialize_turns(std::slice::from_ref(m)));
            }
        }
    }
    text
}

fn serialize_turns(msgs: &[Message]) -> String {
    let mut text = String::new();
    for m in msgs {
        match m.role {
            Role::User => text.push_str(&format!("user: {}\n\n", m.text())),
            Role::Assistant => {
                text.push_str(&format!("assistant: {}\n\n", m.text()));
                for tc in &m.tool_calls {
                    text.push_str(&format!("  (tool call: {} {})\n", tc.name, tc.arguments));
                }
            }
            Role::Tool => text.push_str(&format!(
                "  (tool result{}: {})\n\n",
                if m.is_error { " ERROR" } else { "" },
                first_lines(m.text(), 4)
            )),
            Role::System => {} // system lives separately, never dropped
        }
    }
    text
}

fn first_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().take(max).collect();
    let mut out = lines.join(" | ");
    if text.lines().count() > max {
        out.push_str(" | ...");
    }
    out
}

fn estimate_str(s: &str) -> u64 {
    (s.chars().count() / 4).max(1) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;
    use serde_json::json;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "t".into(),
            arguments: json!({}),
        }
    }

    #[test]
    fn messages_keeping_system_first() {
        let mut h = History::new(Some("S".into()));
        h.push(Message::user("hi"));
        h.push(Message::assistant("yo"));
        let m = h.messages();
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].role, Role::System);
        assert_eq!(m[1].role, Role::User);
    }

    #[test]
    fn trim_drops_whole_turns_not_orphans() {
        let big = "x".repeat(2400); // ≈ 600 "tokens"
        let mut h = History::new(Some("S".into()));
        // turn 1: user + assistant(calls) + tool + assistant
        h.push(Message::user(big.clone()));
        h.push(Message::assistant_with_calls(None, vec![call("a")]));
        h.push(Message::tool_result("a", big.clone(), false));
        h.push(Message::assistant(big.clone()));
        // turn 2
        h.push(Message::user(big.clone()));
        h.push(Message::assistant("last"));

        // budget that only fits turn 2 (~600 tokens + system) — turn 1 must go
        h.trim_to_tokens(700);
        let msgs = h.messages();
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(
            msgs.iter().filter(|m| m.role == Role::User).count(),
            1,
            "turn 1 dropped entirely"
        );
        assert!(msgs
            .iter()
            .any(|m| m.role == Role::Assistant && m.text() == "last"));
        // no orphan tool results without their assistant call
        assert_eq!(
            msgs.iter().filter(|m| m.role == Role::Tool).count(),
            0,
            "turn 1 dropped entirely"
        );
    }

    #[test]
    fn trim_fewer_than_two_turns_keeps_everything() {
        let mut h = History::new(None);
        h.push(Message::user("x".repeat(5000)));
        h.push(Message::assistant("y".repeat(5000)));
        h.trim_to_tokens(10); // cannot drop: single turn is the latest turn
        assert_eq!(h.msgs_len(), 2);
    }
}
