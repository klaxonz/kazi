//! A tiny syntax highlighter for fenced code in the transcript.
//!
//! Not a parser: a line-oriented tokenizer with keyword/type/constant tables,
//! string + comment recognition (including multi-line block comments), and a
//! couple of language-specific niceties (JSON keys, diff rows). That is enough
//! to make code in a terminal readable — and it costs no dependency.

use ratatui::style::Style;
use ratatui::text::Span;

use super::theme::{Syn, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    Js,
    Json,
    Toml,
    Yaml,
    Shell,
    CLike,
    Go,
    Java,
    Sql,
    Html,
    Css,
    Diff,
    Plain,
}

impl Lang {
    /// Map a fence tag / file extension to a language.
    pub fn parse(tag: &str) -> Self {
        let t = tag.trim().trim_start_matches('.').to_ascii_lowercase();
        match t.as_str() {
            "rust" | "rs" => Self::Rust,
            "python" | "py" | "python3" => Self::Python,
            "js" | "javascript" | "jsx" | "ts" | "typescript" | "tsx" | "node" => Self::Js,
            "json" | "jsonc" => Self::Json,
            "toml" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "sh" | "bash" | "zsh" | "shell" | "console" | "powershell" | "ps1" | "bat" | "cmd" => {
                Self::Shell
            }
            "c" | "h" | "cpp" | "c++" | "cc" | "hpp" | "cs" | "csharp" => Self::CLike,
            "go" | "golang" => Self::Go,
            "java" | "kt" | "kotlin" => Self::Java,
            "sql" => Self::Sql,
            "html" | "htm" | "xml" | "svg" | "vue" => Self::Html,
            "css" | "scss" | "less" => Self::Css,
            "diff" | "patch" => Self::Diff,
            _ => Self::Plain,
        }
    }

    pub fn is_known(self) -> bool {
        self != Self::Plain
    }

    fn line_comments(self) -> &'static [&'static str] {
        match self {
            Self::Rust | Self::Js | Self::CLike | Self::Go | Self::Java => &["//"],
            Self::Python | Self::Toml | Self::Yaml | Self::Shell => &["#"],
            Self::Sql => &["--"],
            Self::Css => &[],
            _ => &[],
        }
    }

    fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Rust | Self::Js | Self::CLike | Self::Go | Self::Java | Self::Css | Self::Sql => {
                Some(("/*", "*/"))
            }
            Self::Html => Some(("<!--", "-->")),
            _ => None,
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "unsafe", "use", "where", "while", "yield",
            ],
            Self::Python => &[
                "and", "as", "assert", "async", "await", "break", "class", "continue", "def",
                "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if",
                "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise",
                "return", "True", "try", "while", "with", "yield", "match", "case",
            ],
            Self::Js => &[
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "debugger",
                "default",
                "delete",
                "do",
                "else",
                "export",
                "extends",
                "finally",
                "for",
                "function",
                "if",
                "import",
                "in",
                "instanceof",
                "let",
                "new",
                "of",
                "return",
                "static",
                "super",
                "switch",
                "this",
                "throw",
                "try",
                "typeof",
                "var",
                "void",
                "while",
                "with",
                "yield",
                "interface",
                "type",
                "enum",
                "implements",
            ],
            Self::Json => &["true", "false", "null"],
            Self::Toml => &["true", "false"],
            Self::Yaml => &["true", "false", "null", "yes", "no", "on", "off"],
            Self::Shell => &[
                "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function",
                "if", "in", "local", "return", "then", "until", "while", "set", "unset", "echo",
                "cd", "source", "sudo", "exit",
            ],
            Self::CLike => &[
                "auto",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "constexpr",
                "continue",
                "default",
                "delete",
                "do",
                "else",
                "enum",
                "explicit",
                "extern",
                "false",
                "for",
                "friend",
                "goto",
                "if",
                "inline",
                "namespace",
                "new",
                "nullptr",
                "operator",
                "private",
                "protected",
                "public",
                "return",
                "sizeof",
                "static",
                "struct",
                "switch",
                "template",
                "this",
                "throw",
                "true",
                "try",
                "typedef",
                "typename",
                "union",
                "using",
                "virtual",
                "void",
                "while",
                "int",
                "char",
                "float",
                "double",
                "long",
                "short",
                "unsigned",
                "signed",
                "bool",
            ],
            Self::Go => &[
                "break",
                "case",
                "chan",
                "const",
                "continue",
                "default",
                "defer",
                "else",
                "fallthrough",
                "for",
                "func",
                "go",
                "goto",
                "if",
                "import",
                "interface",
                "map",
                "package",
                "range",
                "return",
                "select",
                "struct",
                "switch",
                "type",
                "var",
                "nil",
                "true",
                "false",
            ],
            Self::Java => &[
                "abstract",
                "assert",
                "boolean",
                "break",
                "byte",
                "case",
                "catch",
                "char",
                "class",
                "const",
                "continue",
                "default",
                "do",
                "double",
                "else",
                "enum",
                "extends",
                "final",
                "finally",
                "float",
                "for",
                "if",
                "implements",
                "import",
                "instanceof",
                "int",
                "interface",
                "long",
                "new",
                "null",
                "package",
                "private",
                "protected",
                "public",
                "return",
                "short",
                "static",
                "super",
                "switch",
                "this",
                "throw",
                "throws",
                "try",
                "true",
                "false",
                "void",
                "while",
                "fun",
                "val",
                "var",
            ],
            Self::Sql => &[
                "select",
                "from",
                "where",
                "insert",
                "into",
                "values",
                "update",
                "set",
                "delete",
                "create",
                "table",
                "drop",
                "alter",
                "join",
                "left",
                "right",
                "inner",
                "outer",
                "on",
                "group",
                "by",
                "order",
                "having",
                "limit",
                "offset",
                "as",
                "and",
                "or",
                "not",
                "null",
                "is",
                "in",
                "like",
                "between",
                "distinct",
                "union",
                "all",
                "select",
                "case",
                "when",
                "then",
                "else",
                "end",
                "with",
                "primary",
                "key",
                "foreign",
                "references",
                "index",
                "default",
                "true",
                "false",
            ],
            Self::Html => &[
                "html", "head", "body", "div", "span", "script", "style", "class", "id", "href",
                "src", "type", "name", "value", "meta", "link", "title", "section", "header",
                "footer", "main", "nav", "button", "input", "form", "table", "tr", "td", "th",
            ],
            Self::Css => &[
                "color",
                "background",
                "display",
                "flex",
                "grid",
                "margin",
                "padding",
                "border",
                "font",
                "width",
                "height",
                "position",
                "top",
                "left",
                "right",
                "bottom",
                "important",
                "media",
                "keyframes",
                "from",
                "to",
                "transform",
                "transition",
            ],
            Self::Diff => &[],
            Self::Plain => &[],
        }
    }

    fn builtin_types(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "bool", "char", "str", "String", "u8", "u16", "u32", "u64", "u128", "usize", "i8",
                "i16", "i32", "i64", "i128", "isize", "f32", "f64", "Vec", "Option", "Result",
                "Box", "Arc", "Rc", "HashMap", "HashSet", "BTreeMap", "Path", "PathBuf",
                "Duration", "Instant", "Self",
            ],
            Self::Python => &[
                "int", "float", "str", "bytes", "list", "dict", "set", "tuple", "object", "bool",
            ],
            Self::Js => &[
                "number", "string", "boolean", "object", "bigint", "symbol", "any", "unknown",
                "never", "void",
            ],
            Self::Go => &[
                "string", "int", "int64", "float64", "bool", "byte", "rune", "error", "any",
            ],
            Self::Java => &[
                "String", "Integer", "Long", "Double", "Boolean", "List", "Map", "Set", "Object",
            ],
            Self::CLike => &[
                "size_t", "int32_t", "int64_t", "uint8_t", "uint32_t", "uint64_t", "std", "string",
                "vector", "map",
            ],
            _ => &[],
        }
    }

    fn constants(self) -> &'static [&'static str] {
        &[
            "true",
            "false",
            "null",
            "None",
            "nil",
            "TRUE",
            "FALSE",
            "NULL",
            "NaN",
            "undefined",
        ]
    }

    fn quotes(self) -> &'static [char] {
        match self {
            Self::Shell => &['"', '\'', '`'],
            Self::Js => &['"', '\'', '`'],
            Self::Sql => &['\'', '"', '`'],
            Self::Html | Self::Css => &['"', '\''],
            _ => &['"', '\''],
        }
    }

    /// `identifier:` counts as a key in these formats (styled as `Tag`).
    fn keys_before(self) -> &'static [char] {
        match self {
            Self::Json => &[':'],
            Self::Toml | Self::Yaml => &['=', ':'],
            _ => &[],
        }
    }

    fn case_insensitive_keywords(self) -> bool {
        matches!(self, Self::Sql)
    }
}

/// Per-line tokenizer with the small amount of cross-line state that matters
/// (an open block comment or multi-line string).
#[derive(Debug, Clone)]
pub struct Highlighter {
    lang: Lang,
    in_block: Option<&'static str>,
    in_string: Option<char>,
}

impl Highlighter {
    pub fn new(lang: Lang) -> Self {
        Self {
            lang,
            in_block: None,
            in_string: None,
        }
    }

    pub fn line(&mut self, text: &str, theme: &Theme) -> Vec<Span<'static>> {
        if self.lang == Lang::Diff {
            return vec![Span::styled(text.to_string(), theme.diff_style(text))];
        }
        if self.lang == Lang::Plain {
            return vec![Span::raw(text.to_string())];
        }
        let chars: Vec<char> = text.chars().collect();
        let mut out: Vec<Span<'static>> = Vec::new();
        let mut buf = String::new();
        let mut buf_style: Option<Style> = None;
        let mut i = 0usize;
        let n = chars.len();

        macro_rules! flush {
            () => {
                if !buf.is_empty() {
                    let style = buf_style.unwrap_or_else(|| theme.body());
                    out.push(Span::styled(std::mem::take(&mut buf), style));
                }
            };
        }
        macro_rules! push {
            ($text:expr, $style:expr) => {{
                let style = $style;
                if buf_style != Some(style) {
                    flush!();
                    buf_style = Some(style);
                }
                buf.push_str(&$text);
            }};
        }

        // already inside a block comment?
        if let Some(end) = self.in_block {
            if let Some(pos) = find_str(&chars, i, end) {
                let text: String = chars[i..pos + end.chars().count()].iter().collect();
                push!(text, theme.syn(Syn::Comment));
                i = pos + end.chars().count();
                self.in_block = None;
            } else {
                let text: String = chars[i..].iter().collect();
                push!(text, theme.syn(Syn::Comment));
                flush!();
                return out;
            }
        }
        // ... or inside a multi-line string?
        if let Some(quote) = self.in_string {
            let start = i;
            let mut j = i;
            let mut escaped = false;
            let mut closed = false;
            while j < n {
                let c = chars[j];
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == quote {
                    j += 1;
                    closed = true;
                    break;
                }
                j += 1;
            }
            let text: String = chars[start..j].iter().collect();
            push!(text, theme.syn(Syn::String));
            i = j;
            if closed {
                self.in_string = None;
            }
        }

        while i < n {
            let c = chars[i];
            // line comment
            let mut hit_comment = None;
            for marker in self.lang.line_comments() {
                if starts_with(&chars, i, marker) {
                    hit_comment = Some(marker);
                }
            }
            if let Some(_marker) = hit_comment {
                let text: String = chars[i..].iter().collect();
                push!(text, theme.syn(Syn::Comment));
                i = n;
                continue;
            }
            // block comment open
            if let Some((open, close)) = self.lang.block_comment() {
                if starts_with(&chars, i, open) {
                    if let Some(pos) = find_str(&chars, i + open.chars().count(), close) {
                        let end = pos + close.chars().count();
                        let text: String = chars[i..end].iter().collect();
                        push!(text, theme.syn(Syn::Comment));
                        i = end;
                    } else {
                        let text: String = chars[i..].iter().collect();
                        push!(text, theme.syn(Syn::Comment));
                        self.in_block = Some(close);
                        i = n;
                    }
                    continue;
                }
            }
            // string
            if self.lang.quotes().contains(&c) {
                let quote = c;
                let start = i;
                let mut j = i + 1;
                let mut escaped = false;
                while j < n {
                    let ch = chars[j];
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == quote {
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                let closed = j <= n && j > i + 1 && chars[j - 1] == quote;
                let text: String = chars[start..j].iter().collect();
                // `"key":` / `"key" =` — a quoted key, not a value
                let next_sig = chars[j..].iter().find(|c| !c.is_whitespace()).copied();
                let key_like =
                    closed && self.lang.keys_before().iter().any(|k| next_sig == Some(*k));
                push!(
                    text,
                    theme.syn(if key_like { Syn::Tag } else { Syn::String })
                );
                if !closed {
                    self.in_string = Some(quote);
                }
                i = j;
                continue;
            }
            // number
            if c.is_ascii_digit() {
                let start = i;
                while i < n
                    && (chars[i].is_ascii_alphanumeric()
                        || matches!(chars[i], '.' | '_' | 'x' | 'X' | 'e' | 'E' | '+' | '-'))
                {
                    // don't swallow `1-2` as one number
                    if matches!(chars[i], '+' | '-') && i > start {
                        break;
                    }
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                push!(text, theme.syn(Syn::Number));
                continue;
            }
            // identifier
            if c.is_alphabetic() || c == '_' || (c == '$' && self.lang == Lang::Js) {
                let start = i;
                while i < n
                    && (chars[i].is_alphanumeric()
                        || chars[i] == '_'
                        || (chars[i] == '$' && self.lang == Lang::Js))
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let lower = word.to_ascii_lowercase();
                let is_kw = self.lang.keywords().iter().any(|k| {
                    if self.lang.case_insensitive_keywords() {
                        k.eq_ignore_ascii_case(&word)
                    } else {
                        *k == word
                    }
                });
                let next_sig = chars[i..].iter().find(|c| !c.is_whitespace()).copied();
                let class = if is_kw {
                    Syn::Keyword
                } else if self.lang.constants().contains(&word.as_str()) {
                    Syn::Constant
                } else if self.lang.builtin_types().contains(&word.as_str()) {
                    Syn::Type
                } else if self.lang.keys_before().iter().any(|k| next_sig == Some(*k)) {
                    Syn::Tag
                } else if word.chars().next().is_some_and(char::is_uppercase)
                    && !matches!(
                        self.lang,
                        Lang::Shell | Lang::Json | Lang::Yaml | Lang::Toml
                    )
                    && !lower.is_empty()
                {
                    Syn::Type
                } else if next_sig == Some('(') || next_sig == Some('!') {
                    Syn::Function
                } else {
                    Syn::Punct
                };
                push!(word, theme.syn(class));
                continue;
            }
            // everything else: punctuation/whitespace
            let text: String = c.to_string();
            push!(text, theme.body());
            i += 1;
        }
        flush!();
        out
    }
}

fn starts_with(chars: &[char], at: usize, needle: &str) -> bool {
    needle
        .chars()
        .enumerate()
        .all(|(k, c)| chars.get(at + k) == Some(&c))
}

fn find_str(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() {
        return Some(from);
    }
    let mut i = from;
    while i + n.len() <= chars.len() {
        if chars[i..i + n.len()] == n[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Highlight a whole code block: one span list per line.
#[cfg(test)]
pub fn highlight(code: &str, lang_tag: &str, theme: &Theme) -> Vec<Vec<Span<'static>>> {
    let mut h = Highlighter::new(Lang::parse(lang_tag));
    code.lines().map(|line| h.line(line, theme)).collect()
}

/// Convenience: highlight one line with a fresh highlighter.
#[cfg(test)]
pub fn one_line(line: &str, lang_tag: &str, theme: &Theme) -> Vec<Span<'static>> {
    Highlighter::new(Lang::parse(lang_tag)).line(line, theme)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn text_of(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn rust_tokens_get_classes() {
        let theme = Theme::default();
        let mut h = Highlighter::new(Lang::Rust);
        let spans = h.line("fn main() { let x = \"hi\"; } // done", &theme);
        assert_eq!(text_of(&spans), "fn main() { let x = \"hi\"; } // done");
        let kw = spans
            .iter()
            .find(|s| s.content == "fn")
            .expect("keyword span");
        assert_eq!(kw.style.fg, Some(theme.keyword));
        let s = spans
            .iter()
            .find(|s| s.content.contains("hi"))
            .expect("string span");
        assert_eq!(s.style.fg, Some(theme.string));
        let c = spans
            .iter()
            .find(|s| s.content.contains("done"))
            .expect("comment span");
        assert_eq!(c.style.fg, Some(theme.comment));
    }

    #[test]
    fn json_keys_and_values() {
        let theme = Theme::default();
        let mut h = Highlighter::new(Lang::Json);
        let spans = h.line("  \"name\": \"kazi\", \"n\": 3", &theme);
        let key = spans
            .iter()
            .find(|s| s.content.contains("name"))
            .expect("key span");
        assert_eq!(key.style.fg, Some(theme.key));
        let num = spans.iter().find(|s| s.content == "3").expect("number");
        assert_eq!(num.style.fg, Some(theme.number));
    }

    #[test]
    fn block_comments_span_lines() {
        let theme = Theme::default();
        let mut h = Highlighter::new(Lang::Rust);
        let first = h.line("let a = 1; /* start", &theme);
        assert!(first.iter().any(|s| s.content.contains("start")));
        let second = h.line("still comment */ let b = 2;", &theme);
        let joined = text_of(&second);
        assert_eq!(joined, "still comment */ let b = 2;");
        let comment = second
            .iter()
            .find(|s| s.content.contains("still"))
            .expect("comment span");
        assert_eq!(comment.style.fg, Some(theme.comment));
    }

    #[test]
    fn plain_language_is_one_span() {
        let theme = Theme::default();
        let spans = one_line("anything at all", "", &theme);
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn diff_lines_use_diff_colours() {
        let theme = Theme::default();
        let add = one_line("+added", "diff", &theme);
        assert_eq!(add[0].style.fg, Some(theme.add));
        let del = one_line("-removed", "diff", &theme);
        assert_eq!(del[0].style.fg, Some(theme.del));
        assert_eq!(theme.add, Color::Rgb(0x9e, 0xce, 0x6a));
    }

    #[test]
    fn python_hash_comment_and_triple_quote() {
        let theme = Theme::default();
        let mut h = Highlighter::new(Lang::Python);
        let spans = h.line("def f(x):  # hi", &theme);
        assert!(spans
            .iter()
            .any(|s| s.content == "def" && s.style.fg == Some(theme.keyword)));
        assert!(spans
            .iter()
            .any(|s| s.content.contains("# hi") && s.style.fg == Some(theme.comment)));
        // an unterminated triple quote keeps the tokenizer inside a string
        let _ = h.line("s = \"\"\"start", &theme);
        let next = h.line("middle", &theme);
        assert_eq!(next[0].style.fg, Some(theme.string));
    }

    #[test]
    fn unicode_text_roundtrips() {
        let theme = Theme::default();
        let spans = one_line("let 变量 = \"值\";", "rust", &theme);
        assert!(text_of(&spans).contains("变量"));
    }

    #[test]
    fn no_span_drops_characters() {
        let theme = Theme::default();
        for (lang, code) in [
            ("rust", "fn f() -> Result<(), E> { Ok(()) }"),
            ("json", "{\"a\": [1, 2.5, true, null]}"),
            ("sh", "echo \"hi\" # there"),
            ("html", "<div class=\"x\">text</div>"),
            ("sql", "SELECT * FROM t WHERE a = 'b';"),
        ] {
            let lines = highlight(code, lang, &theme);
            assert_eq!(lines.len(), 1);
            assert_eq!(text_of(&lines[0]), code, "lang {lang} lost characters");
        }
    }
}
