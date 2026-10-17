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
    /// Message indexes the user pinned (`/pin`): trimming steps over the whole
    /// turn containing a pinned message, so a constraint survives the budget.
    /// Indexes shift when older turns are dropped, so they are stored as
    /// relative positions and rebased on every drain.
    pub pinned: std::collections::BTreeSet<usize>,
}

impl History {
    pub fn new(system: Option<String>) -> Self {
        Self {
            system,
            msgs: Vec::new(),
            pinned: std::collections::BTreeSet::new(),
        }
    }

    pub fn push(&mut self, m: Message) {
        self.msgs.push(m);
    }

    pub fn reset_msgs(&mut self) {
        self.msgs.clear();
        self.pinned.clear();
    }

    /// Pin one message index (1-based in the transcript's display order is the
    /// UI's business; this takes the raw `msgs` index).
    pub fn pin(&mut self, idx: usize) -> bool {
        if idx >= self.msgs.len() {
            return false;
        }
        self.pinned.insert(idx);
        true
    }

    pub fn unpin(&mut self, idx: usize) -> bool {
        self.pinned.remove(&idx)
    }

    pub fn clear_pins(&mut self) {
        self.pinned.clear();
    }

    pub fn is_pinned(&self, idx: usize) -> bool {
        self.pinned.contains(&idx)
    }

    pub fn pins_len(&self) -> usize {
        self.pinned.len()
    }

    /// `(index, text)` for every pinned message, in order.
    pub fn pins_list(&self) -> Vec<(usize, String)> {
        self.pinned
            .iter()
            .filter_map(|i| self.msgs.get(*i).map(|m| (*i, m.text().to_string())))
            .collect()
    }

    /// Rebase pin indexes after `dropped` messages were removed from the head.
    fn rebase_pins(&mut self, dropped: usize) {
        if self.pinned.is_empty() {
            return;
        }
        self.pinned = self
            .pinned
            .iter()
            .filter_map(|i| i.checked_sub(dropped))
            .collect();
    }

    /// The turn containing a pinned message starts at this index (the nearest
    /// user message at or before it): pining one line keeps its whole task.
    fn pin_floor(&self) -> Option<usize> {
        let first = *self.pinned.iter().next()?;
        let start = (0..=first.min(self.msgs.len().saturating_sub(1)))
            .rev()
            .find(|i| self.msgs[*i].role == Role::User)?;
        Some(start)
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
            let mut cut = self.turn_cut();
            if let (Some(c), Some(floor)) = (cut, self.pin_floor()) {
                cut = (c <= floor).then_some(c);
            }
            match cut {
                Some(i) if i > 0 => {
                    self.msgs.drain(0..i);
                    self.rebase_pins(i);
                }
                _ => break,
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

    /// Compaction path: pull the oldest droppable messages out (serialized as
    /// a transcript for summarization) until the history fits `target_tokens`
    /// — or nothing is left to drop.
    ///
    /// Two granularities, oldest first:
    /// 1. **Whole completed turns** — everything before the second user
    ///    message (multi-task histories).
    /// 2. **Completed tool ROUNDS inside the current task** — a long single
    ///    task has one user message but many assistant/tool round-trips;
    ///    everything before the *second-to-last* `assistant_with_calls`
    ///    boundary is a finished round whose results the model no longer
    ///    needs verbatim. The latest round (with its pending tool results)
    ///    is always kept intact so the next provider call stays legal.
    ///
    /// `None` = already under `target_tokens` / nothing droppable.
    pub fn take_beyond_text(&mut self, target_tokens: u64) -> Option<String> {
        if self.estimate_tokens() <= target_tokens {
            return None;
        }
        let mut dropped: Vec<Message> = Vec::new();
        loop {
            if self.estimate_tokens() <= target_tokens {
                break;
            }
            // turns first, then rounds of the one remaining task. `cut > 0`
            // guards the mid-task case where the newest round *starts* at the
            // head: nothing older is left to drop, so stop instead of spinning.
            let mut cut = self
                .turn_cut()
                .or_else(|| self.last_round_cut())
                .filter(|cut| *cut > 0);
            // never cut into (or past) a pinned turn: trimming stops at its
            // start, so the pinned constraint stays in the window
            if let (Some(c), Some(floor)) = (cut, self.pin_floor()) {
                cut = (c <= floor).then_some(c);
            }
            let Some(cut) = cut.filter(|cut| *cut > 0) else {
                break;
            };
            dropped.extend(self.msgs.drain(0..cut));
            self.rebase_pins(cut);
        }
        if dropped.is_empty() {
            None
        } else {
            Some(serialize_turns(&dropped))
        }
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
    fn take_beyond_text_terminates_on_a_mid_task_head() {
        // A history that starts mid-task (an older drop removed the head) and
        // holds only the newest round: nothing older is droppable, and asking
        // for target 0 must say so instead of looping forever.
        let mut h = History::new(Some("S".into()));
        h.push(Message::assistant_with_calls(None, vec![call("a")]));
        h.push(Message::tool_result("a", "x".repeat(5000), false));
        h.push(Message::assistant_with_calls(None, vec![call("b")]));
        h.push(Message::tool_result("b", "y".repeat(5000), false));

        assert!(h.take_beyond_text(0).is_none());
        assert_eq!(h.msgs_len(), 4, "the newest round stays intact");
    }

    #[test]
    fn take_beyond_text_stops_at_the_target() {
        let big = "x".repeat(400); // ≈ 100 est. tokens per message
        let mut h = History::new(Some("S".into()));
        for i in 0..4 {
            h.push(Message::user(format!("task {i} {big}")));
            h.push(Message::assistant_with_calls(
                None,
                vec![call(&format!("c{i}"))],
            ));
            h.push(Message::tool_result(format!("c{i}"), big.clone(), false));
            h.push(Message::assistant(big.clone()));
        }
        // dropping toward a target leaves the newest turn in place
        let dropped = h.take_beyond_text(200).expect("over target");
        assert!(dropped.contains("task 0"), "oldest turns go first");
        assert!(
            !dropped.contains("task 3"),
            "the newest turn must survive: {dropped}"
        );
        assert_eq!(h.msgs_len(), 4, "only the newest turn is left");
    }

    #[test]
    fn pins_can_be_listed_and_cleared() {
        let mut h = History::new(None);
        h.push(Message::user("first"));
        h.push(Message::assistant("answer"));
        assert!(h.pin(1));
        let list = h.pins_list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, 1);
        assert_eq!(list[0].1, "answer");
        assert_eq!(h.pins_len(), 1);
        h.clear_pins();
        assert!(h.pins_list().is_empty());
        // pinning out of range is refused, not silently accepted
        assert!(!h.pin(99));
        // /clear drops the pins with the messages
        h.pin(0);
        h.reset_msgs();
        assert_eq!(h.pins_len(), 0);
    }

    #[test]
    fn pinned_message_survives_trimming() {
        let big = "x".repeat(400);
        let mut h = History::new(Some("S".into()));
        // turn 1 carries a constraint the user pins
        h.push(Message::user(format!("task 0 {big}")));
        h.push(Message::assistant("constraint: always use tabs"));
        // turns 2..4 are filler
        for i in 1..4 {
            h.push(Message::user(format!("task {i} {big}")));
            h.push(Message::assistant(big.clone()));
        }
        assert!(h.pin(1), "pin the constraint line");
        h.trim_to_tokens(300);
        let texts: Vec<&str> = h.msgs.iter().map(|m| m.text()).collect();
        assert!(
            texts.iter().any(|t| t.contains("always use tabs")),
            "the pinned turn must survive: {texts:?}"
        );
        // the pin index followed the remaining messages
        assert!(
            h.is_pinned(1) || h.is_pinned(0),
            "pin rebased: {:?}",
            h.pinned
        );

        // unpinned, the same history is trimmed away entirely
        let mut h2 = History::new(Some("S".into()));
        h2.push(Message::user(format!("task 0 {big}")));
        h2.push(Message::assistant("constraint: always use tabs"));
        for i in 1..4 {
            h2.push(Message::user(format!("task {i} {big}")));
            h2.push(Message::assistant(big.clone()));
        }
        h2.trim_to_tokens(300);
        assert!(!h2.msgs.iter().any(|m| m.text().contains("always use tabs")));
    }

    #[test]
    fn pinned_turn_is_never_cut_by_take_beyond_text() {
        let big = "x".repeat(400);
        let mut h = History::new(Some("S".into()));
        h.push(Message::user("keep me"));
        h.push(Message::assistant("ok"));
        for i in 0..4 {
            h.push(Message::user(format!("task {i} {big}")));
            h.push(Message::assistant(big.clone()));
        }
        h.pin(0);
        let dropped = h.take_beyond_text(10).unwrap_or_default();
        assert!(
            !dropped.contains("keep me"),
            "compaction must stop before the pinned turn: {dropped}"
        );
        assert!(h.msgs.iter().any(|m| m.text() == "keep me"));
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
