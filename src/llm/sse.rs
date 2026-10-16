//! Minimal hand-written SSE (Server-Sent Events) decoder.
//!
//! Both provider protocols stream over HTTP as SSE; we deliberately avoid an
//! SSE crate because the agent's streaming needs are small and the parsing
//! rules are easy to get right — and seeing the parser in ~80 lines is part of
//! what makes this project readable.
//!
//! The subset we support (all that OpenAI-compat and Anthropic use):
//! - `data:` payload lines (possibly multi-line, joined with `\n`)
//! - `event:` named events (Anthropic uses `message_start`, `content_block_delta`, ...)
//! - blank line = event boundary
//! - `:` comment lines (keepalives) are ignored
//! - CRLF line endings

/// One complete SSE event as delivered by [`SseDecoder::push`]/[`finish`].
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    /// Value of the `event:` line, if the peer sent one.
    pub event: Option<String>,
    /// `data:` payload lines joined with `\n`.
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Carry-over text that may contain an incomplete line.
    buf: String,
    cur_event: Option<String>,
    cur_data: Vec<String>,
    /// Seen at least one `data:`/`event:` line for the event being accumulated.
    started: bool,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk (may split lines at arbitrary byte positions); returns
    /// every event that became complete with this chunk, in order.
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find('\n') {
            let mut line: String = self.buf.drain(..=pos).collect();
            line.pop(); // drop '\n'
            if line.ends_with('\r') {
                line.pop();
            }
            self.handle_line(&line, &mut out);
        }
        out
    }

    /// Call once at end-of-stream to flush a trailing event that had no
    /// terminating blank line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let remaining = std::mem::take(&mut self.buf);
        let mut out = Vec::new();
        let line = remaining.trim_end_matches('\r');
        if !line.is_empty() {
            self.handle_line(line, &mut out);
        }
        let last = self.take_event();
        if let Some(ev) = last {
            out.push(ev);
        }
        out
    }

    fn handle_line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        // Empty line terminates the current event.
        if line.is_empty() {
            if let Some(ev) = self.take_event() {
                out.push(ev);
            }
            return;
        }
        // Colon-initial lines are comments/keepalives per the SSE spec.
        if line.starts_with(':') {
            return;
        }
        if let Some(v) = line.strip_prefix("event:") {
            self.cur_event = Some(v.trim_start().to_string());
            self.started = true;
        } else if let Some(v) = line.strip_prefix("data: ") {
            self.cur_data.push(v.to_string());
            self.started = true;
        } else if let Some(v) = line.strip_prefix("data:") {
            self.cur_data.push(v.to_string());
            self.started = true;
        }
        // Other field lines (id:, retry:, unknown fields) are ignored on purpose.
    }

    fn take_event(&mut self) -> Option<SseEvent> {
        self.started = false;
        if self.cur_data.is_empty() && self.cur_event.is_none() {
            self.cur_data.clear();
            self.cur_event = None;
            return None;
        }
        let ev = SseEvent {
            event: self.cur_event.take(),
            data: std::mem::take(&mut self.cur_data).join("\n"),
        };
        self.cur_data.clear();
        Some(ev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datas(evs: &[SseEvent]) -> Vec<&str> {
        evs.iter().map(|e| e.data.as_str()).collect()
    }

    #[test]
    fn classic_data_lines() {
        let mut d = SseDecoder::new();
        let evs = d.push("data: {\"a\":1}\n\ndata: {\"b\":2}\n\n");
        assert_eq!(datas(&evs), ["{\"a\":1}", "{\"b\":2}"]);
        assert!(d.finish().is_empty());
    }

    #[test]
    fn chunk_fragmentation_across_pushes() {
        let mut d = SseDecoder::new();
        assert!(d.push("data: {\"a\"").is_empty());
        assert!(d.push(":1}\n").is_empty()); // delimiter arrives with payload
        let evs = d.push("\ndata: [DONE]\n\n");
        assert_eq!(datas(&evs), ["{\"a\":1}", "[DONE]"]);
    }

    #[test]
    fn crlf_line_endings() {
        let mut d = SseDecoder::new();
        let evs = d.push("data: x\r\n\r\n");
        assert_eq!(datas(&evs), ["x"]);
    }

    #[test]
    fn anthropic_named_events() {
        let mut d = SseDecoder::new();
        let evs = d.push(
            "event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"hel\"}}\n\n\
             event: message_stop\ndata: {}\n\n",
        );
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].event.as_deref(), Some("content_block_delta"));
        assert_eq!(evs[1].event.as_deref(), Some("message_stop"));
    }

    #[test]
    fn multiline_data_and_comments() {
        // multi-line data: every line must carry its own `data:` prefix
        let mut d = SseDecoder::new();
        let evs = d.push(": keepalive\ndata: {\ndata: \"x\":1\ndata: }\n\n");
        assert_eq!(datas(&evs), ["{\n\"x\":1\n}"]);
    }

    #[test]
    fn trailing_event_without_blank_line() {
        let mut d = SseDecoder::new();
        assert!(d.push("data: {\"z\":0}").is_empty());
        let evs = d.finish();
        assert_eq!(datas(&evs), ["{\"z\":0}"]);
    }
}
