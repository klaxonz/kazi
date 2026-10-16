//! `fetch_url` — read-only web access over HTTPS.
//!
//! Conservative by design: https only, personal/local networks rejected
//! (basic SSRF guard), 1 MiB read cap, 30 s cap. HTML pages are reduced to
//! rough plain text (tags + script/style dropped) because the model is the
//! consumer, not a browser.

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};

use super::{arg_str, Tool, ToolCtx, ToolError};

pub struct FetchUrl;

const MAX_BYTES: usize = 1024 * 1024;
const RETURN_CAP: usize = 20_000;
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn is_private_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => match ip {
            std::net::IpAddr::V4(v4) => {
                if v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
                {
                    return true;
                }
                // RFC 5737/3849/6598/6666 and friends: not routable for users.
                let o = v4.octets();
                let docs_a = o[0] == 192 && o[1] == 0 && o[2] == 2;
                let test_1 = o[0] == 192 && o[1] == 0 && o[2] == 0;
                let benchmarking = o[0] == 198 && (o[1] == 18 || o[1] == 19);
                let docs_b = o[0] == 198 && o[1] == 51 && o[2] == 100;
                let docs_c = o[0] == 203 && o[1] == 0 && o[2] == 113;
                let carrier_nat = o[0] == 100 && (o[1] & 0xc0) == 0x40;
                let this_net = o[0] == 0;
                docs_a || test_1 || benchmarking || docs_b || docs_c || carrier_nat || this_net
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unspecified() || is_ula_or_linklocal_v6(&v6)
            }
        },
        Err(_) => false, // hostnames are fine; DNS rebinding is out of scope (README)
    }
}

fn is_ula_or_linklocal_v6(ip: &std::net::Ipv6Addr) -> bool {
    let seg = ip.octets();
    // fc00::/7 (ULA) or fe80::/10 (link-local)
    (seg[0] & 0xfe) == 0xfc || seg[0] == 0xfe && (seg[1] & 0xc0) == 0x80
}

/// Rough HTML → text: drop `<script>`/`<style>` blocks, then strip tags.
fn html_to_text(html: &str) -> String {
    let mut text = html.to_string();
    for tag in ["script", "style"] {
        text = strip_block(&text, tag);
    }
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Case-insensitively remove `<tag ...> ... </tag>` blocks.
fn strip_block(s: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.to_lowercase().find(&open) {
        let end_rel = match rest[start..].to_lowercase().find(&close) {
            Some(e) => e + close.len(),
            None => break,
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + end_rel..];
    }
    out.push_str(rest);
    out
}

#[async_trait]
impl Tool for FetchUrl {
    fn name(&self) -> &'static str {
        "fetch_url"
    }
    fn description(&self) -> &str {
        "Fetch a public https:// URL and return its plain text (HTML reduced to text). Local/private addresses are blocked. Use for documentation and API lookups."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"url": {"type": "string", "description": "Public https:// URL"}},
            "required": ["url"]
        })
    }

    async fn run(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, ToolError> {
        let raw = arg_str(args, "url")?;
        let url =
            reqwest::Url::parse(raw).map_err(|e| ToolError::new(format!("invalid url: {e}")))?;
        if url.scheme() != "https" {
            return Err(ToolError::new("only https:// URLs are accepted"));
        }
        let host = url
            .host_str()
            .ok_or_else(|| ToolError::new("URL has no host"))?;
        if is_private_host(host) {
            return Err(ToolError::new(format!(
                "host {host:?} looks local/private and is blocked"
            )));
        }

        let http = reqwest::Client::builder()
            .user_agent(concat!("kazi/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ToolError::new(format!("http client: {e}")))?;
        let resp = http
            .get(url)
            .header(
                reqwest::header::ACCEPT,
                "text/plain, text/*;q=0.9, */*;q=0.5",
            )
            .send()
            .await
            .map_err(|e| ToolError::new(format!("fetch failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::new(format!("HTTP {}", resp.status().as_u16())));
        }
        let is_html = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("html"))
            .unwrap_or(false);

        // Cap bytes AND wall time: read the stream until 1 MiB or 30 s,
        // whichever comes first, regardless of claimed Content-Length.
        let deadline = tokio::time::Instant::now() + FETCH_TIMEOUT;
        let mut stream = resp.bytes_stream();
        let mut bytes: Vec<u8> = Vec::with_capacity(8192);
        // 4-arm match (timeout / transport error / chunk / EOF) — a `while
        // let` rewrite would lose the diagnostics, so keep the long form.
        #[allow(clippy::while_let_loop)]
        'done: loop {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Err(_) => {
                    return Err(ToolError::new(format!(
                        "fetch timed out after {}s",
                        FETCH_TIMEOUT.as_secs()
                    )))
                }
                Ok(Some(Err(e))) => return Err(ToolError::new(format!("fetch read failed: {e}"))),
                Ok(Some(Ok(c))) => {
                    bytes.extend_from_slice(&c);
                    if bytes.len() >= MAX_BYTES {
                        bytes.truncate(MAX_BYTES);
                        break 'done;
                    }
                }
                Ok(None) => break 'done,
            }
        }
        let body = String::from_utf8_lossy(&bytes).into_owned();
        let text = if is_html { html_to_text(&body) } else { body };
        if text.chars().count() <= RETURN_CAP {
            Ok(text)
        } else {
            Ok(format!(
                "{}\n...[truncated at {RETURN_CAP} chars; try a more specific URL or ?range=]",
                text.chars().take(RETURN_CAP).collect::<String>()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_blocks_and_tags_stripped() {
        let html = r#"<html><head><style>body{}</style></head><body><h1>Hi</h1><script>alert(1)</script><p>plain text</p></body></html>"#;
        let t = html_to_text(html);
        assert!(t.contains("Hi"));
        assert!(t.contains("plain text"));
        assert!(!t.contains("alert"));
        assert!(!t.contains('<'));
    }

    #[test]
    fn private_hosts_detected() {
        assert!(is_private_host("localhost"));
        assert!(is_private_host("127.0.0.1"));
        assert!(is_private_host("10.1.2.3"));
        assert!(is_private_host("192.168.0.5"));
        assert!(is_private_host("172.31.9.1"));
        assert!(is_private_host("169.254.7.7"));
        assert!(is_private_host("::1"));
        assert!(is_private_host("fe80::1"));
        assert!(is_private_host("fd00::1"));
        assert!(!is_private_host("crates.io"));
        assert!(!is_private_host("8.8.8.8"));
    }
}
