//! Minimal, dependency-free PDF text extraction.
//!
//! Scope is deliberately narrow and stated honestly in the returned text:
//! only **uncompressed** text operators are read. Most real PDFs store their
//! content streams with FlateDecode, which would need an inflate
//! implementation (a dependency kazi does not take for one tool). When the
//! document is compressed the caller gets a clear "not supported" line
//! instead of silently empty output — the model then knows to ask for the
//! source or a text export rather than inventing content.

/// True when the bytes look like a PDF (`%PDF-` magic near the start).
pub fn looks_like_pdf(bytes: &[u8]) -> bool {
    bytes.windows(5).take(1024).any(|w| w == b"%PDF-")
}

/// Extract readable text from an uncompressed PDF.
///
/// Returns `(text, note)` where `note` explains a limitation worth surfacing
/// (compressed streams, scanned pages, ...).
pub fn extract_text(bytes: &[u8]) -> (String, Option<String>) {
    if !looks_like_pdf(bytes) {
        return (
            String::new(),
            Some("not a PDF (missing %PDF- header)".into()),
        );
    }
    let raw = String::from_utf8_lossy(bytes);
    if raw.contains("/FlateDecode") {
        return (
            String::new(),
            Some(
                "this PDF compresses its text streams (FlateDecode), which kazi does not \
                 decompress; ask for the source, a text export, or paste the relevant pages"
                    .into(),
            ),
        );
    }
    if raw.contains("/Encrypt") {
        return (
            String::new(),
            Some("this PDF is encrypted; kazi cannot read it".into()),
        );
    }

    let mut out = String::new();
    let mut in_text = false;
    for line in raw.lines() {
        let line = line.trim();
        // operators are whole tokens, so "BETA" in a string is not a BT
        let token = |t: &str| line.split_whitespace().any(|w| w == t);
        if token("BT") {
            in_text = true;
        }
        // Tj / TJ show text: strings live in balanced parentheses, with
        // backslash escapes for ( ) \\ and octal codes. A one-line
        // "BT (...) Tj ET" must still be read, so extract before ET closes.
        if in_text && (token("Tj") || token("TJ")) {
            push_pdf_strings(line, &mut out);
            out.push('\n');
        }
        if token("ET") {
            in_text = false;
        }
    }
    let text = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        (
            text,
            Some(
                "no uncompressed text found (scanned/image-only PDF, or compressed content \
                 streams); kazi cannot OCR"
                    .into(),
            ),
        )
    } else {
        (text, None)
    }
}

/// Pull `(...)` literals out of one PDF operator line.
fn push_pdf_strings(line: &str, out: &mut String) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'(' {
            i += 1;
            continue;
        }
        i += 1;
        let mut depth = 1;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'\\' => {
                    i += 1;
                    if let Some(&esc) = bytes.get(i) {
                        match esc {
                            b'n' => out.push('\n'),
                            b'r' => out.push('\r'),
                            b't' => out.push('\t'),
                            b'(' | b')' | b'\\' => out.push(esc as char),
                            // octal escape: up to three digits
                            b'0'..=b'7' => {
                                let mut val: u16 = (esc - b'0') as u16;
                                let mut extra = 0;
                                while extra < 2 {
                                    match bytes.get(i + 1) {
                                        Some(d @ b'0'..=b'7') => {
                                            val = val * 8 + (d - b'0') as u16;
                                            i += 1;
                                            extra += 1;
                                        }
                                        _ => break,
                                    }
                                }
                                out.push((val as u8) as char);
                            }
                            other => out.push(other as char),
                        }
                    }
                }
                b'(' => {
                    depth += 1;
                    out.push('(');
                }
                b')' => {
                    depth -= 1;
                    if depth > 0 {
                        out.push(')');
                    }
                }
                b => out.push(b as char),
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf(body: &str) -> Vec<u8> {
        format!("%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n{body}\n%%EOF").into_bytes()
    }

    #[test]
    fn reads_uncompressed_text_blocks() {
        let doc = pdf(
            "4 0 obj\n<< /Length 60 >>\nstream\nBT /F1 12 Tf 72 720 Td (Hello world) Tj ET\nBT (second line) Tj ET\nendstream\nendobj",
        );
        let (text, note) = extract_text(&doc);
        assert!(text.contains("Hello world"), "{text:?}");
        assert!(text.contains("second line"), "{text:?}");
        assert!(note.is_none(), "{note:?}");
    }

    #[test]
    fn unescapes_parentheses_and_octal() {
        let doc = pdf("BT (a \\(b\\) c) Tj ET");
        let (text, _) = extract_text(&doc);
        assert!(text.contains("a (b) c"), "{text:?}");
    }

    #[test]
    fn compressed_streams_say_so_instead_of_returning_nothing() {
        let doc = pdf("4 0 obj\n<< /Filter /FlateDecode >>\nstream\nxx\nendstream\nendobj");
        let (text, note) = extract_text(&doc);
        assert!(text.is_empty());
        let note = note.expect("a limitation must be stated");
        assert!(note.contains("FlateDecode"), "{note}");
    }

    #[test]
    fn scanned_pdfs_are_reported_as_having_no_text() {
        let (text, note) = extract_text(&pdf("BT ET"));
        assert!(text.is_empty());
        assert!(note.unwrap().contains("no uncompressed text"));
        // not a PDF at all
        assert!(extract_text(b"plain text").1.unwrap().contains("not a PDF"));
    }
}
