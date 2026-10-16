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
    pub fn estimate_tokens(&self) -> u64 {
        let mut n: u64 = self.system.as_deref().map(estimate_str).unwrap_or(0);
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
            let cut = match self.trim_point() {
                Some(i) => i,
                None => break,
            };
            self.msgs.drain(0..cut);
        }
    }

    /// Index of the *second* user message, i.e. the boundary after the first
    /// complete turn, or `None` when fewer than two turns exist.
    fn trim_point(&self) -> Option<usize> {
        let users: Vec<usize> = self
            .msgs
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User)
            .map(|(i, _)| i)
            .collect();
        users.get(1).copied()
    }
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
