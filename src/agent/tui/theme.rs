//! Colour + glyph palette for the TUI.
//!
//! Three bundled themes: `dark` (the default), `light` for bright terminals,
//! and `mono` which uses no colour at all (everything is text weight/dim).
//! `/theme` switches at runtime; `[ui] theme = "light"` picks the startup one.
//!
//! Keeping every colour in one place is what makes the UI look intentional:
//! widgets ask for "the accent" or "the error colour" instead of hard-coding
//! `Color::Cyan` in twenty places.

use ratatui::style::{Color, Modifier, Style};

/// Named theme, as typed in `/theme` or config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeName {
    #[default]
    Dark,
    Light,
    Nord,
    Solarized,
    Mono,
}

impl ThemeName {
    /// Every theme, in `Ctrl+T` cycle order.
    pub const ALL: [ThemeName; 5] = [
        Self::Dark,
        Self::Light,
        Self::Nord,
        Self::Solarized,
        Self::Mono,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dark" | "default" => Some(Self::Dark),
            "light" | "bright" => Some(Self::Light),
            "nord" | "arctic" => Some(Self::Nord),
            "solarized" | "solarised" | "solar" => Some(Self::Solarized),
            "mono" | "none" | "plain" => Some(Self::Mono),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Nord => "nord",
            Self::Solarized => "solarized",
            Self::Mono => "mono",
        }
    }

    /// Every name, for `/theme` completion and error hints.
    pub fn names() -> Vec<&'static str> {
        Self::ALL.iter().map(|t| t.as_str()).collect()
    }

    /// Next theme in the `Ctrl+T` / `/theme` cycle.
    pub fn cycle(self) -> Self {
        let idx = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }
}

/// Token classes the syntax highlighter can style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syn {
    Keyword,
    Type,
    Function,
    String,
    Comment,
    Number,
    Constant,
    Punct,
    /// JSON/TOML key, HTML tag, markdown emphasis — the "structural" colour.
    Tag,
}

/// Text glyphs, so a theme can fall back to plain ASCII on limited terminals.
#[derive(Debug, Clone, Copy)]
pub struct Glyphs {
    pub prompt: &'static str,
    pub ok: &'static str,
    pub err: &'static str,
    pub bullet: &'static str,
    pub vbar: &'static str,
    pub hbar: &'static str,
    pub ellipsis: &'static str,
    pub bar_full: &'static str,
    pub bar_empty: &'static str,
    /// Tool card opened / folded (the card's own affordance).
    pub fold_open: &'static str,
    pub fold_closed: &'static str,
}

const UNICODE: Glyphs = Glyphs {
    prompt: "❯",
    ok: "✓",
    err: "✗",
    bullet: "•",
    vbar: "│",
    hbar: "─",
    ellipsis: "…",
    bar_full: "█",
    bar_empty: "░",
    fold_open: "▾",
    fold_closed: "▸",
};

const ASCII: Glyphs = Glyphs {
    prompt: ">",
    ok: "+",
    err: "x",
    bullet: "*",
    vbar: "|",
    hbar: "-",
    ellipsis: "...",
    bar_full: "#",
    bar_empty: ".",
    fold_open: "v",
    fold_closed: ">",
};

/// The full palette for one theme.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: ThemeName,
    pub glyphs: Glyphs,
    pub text: Color,
    pub dim: Color,
    pub faint: Color,
    pub accent: Color,
    pub border: Color,
    pub border_active: Color,
    pub user: Color,
    pub tool: Color,
    pub ok: Color,
    pub err: Color,
    pub warn: Color,
    pub add: Color,
    pub del: Color,
    pub code: Color,
    pub code_bg: Color,
    pub heading: Color,
    pub link: Color,
    pub quote: Color,
    pub key: Color,
    /// Background of a highlighted row / selection.
    pub sel_bg: Color,
    pub sel_fg: Color,
    pub header_bg: Color,
    pub keyword: Color,
    pub ty: Color,
    pub func: Color,
    pub string: Color,
    pub comment: Color,
    pub number: Color,
    pub constant: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::for_name(ThemeName::Dark)
    }
}

impl Theme {
    pub fn for_name(name: ThemeName) -> Self {
        match name {
            ThemeName::Dark => Self::dark(),
            ThemeName::Light => Self::light(),
            ThemeName::Nord => Self::nord(),
            ThemeName::Solarized => Self::solarized(),
            ThemeName::Mono => Self::mono(),
        }
    }

    fn dark() -> Self {
        Self {
            name: ThemeName::Dark,
            glyphs: UNICODE,
            text: Color::Gray,
            dim: Color::DarkGray,
            faint: Color::Rgb(0x58, 0x5b, 0x70),
            accent: Color::Rgb(0x7a, 0xa2, 0xf7),
            border: Color::Rgb(0x3b, 0x40, 0x52),
            border_active: Color::Rgb(0x6f, 0x8c, 0xc9),
            user: Color::Rgb(0x7a, 0xa2, 0xf7),
            tool: Color::Rgb(0x9c, 0xdc, 0xfe),
            ok: Color::Rgb(0x9e, 0xce, 0x6a),
            err: Color::Rgb(0xf7, 0x76, 0x8e),
            warn: Color::Rgb(0xe0, 0xaf, 0x68),
            add: Color::Rgb(0x9e, 0xce, 0x6a),
            del: Color::Rgb(0xf7, 0x76, 0x8e),
            code: Color::Rgb(0x7d, 0xcf, 0xff),
            code_bg: Color::Rgb(0x24, 0x27, 0x33),
            heading: Color::Rgb(0xbb, 0x9a, 0xf7),
            link: Color::Rgb(0x7d, 0xcf, 0xff),
            quote: Color::Rgb(0x73, 0x7a, 0x8f),
            key: Color::Rgb(0xe0, 0xaf, 0x68),
            sel_bg: Color::Rgb(0x2f, 0x3f, 0x63),
            sel_fg: Color::Rgb(0xd5, 0xe0, 0xff),
            header_bg: Color::Rgb(0x1d, 0x20, 0x29),
            keyword: Color::Rgb(0xbb, 0x9a, 0xf7),
            ty: Color::Rgb(0x7a, 0xa2, 0xf7),
            func: Color::Rgb(0x9e, 0xc1, 0xff),
            string: Color::Rgb(0x9e, 0xce, 0x6a),
            comment: Color::Rgb(0x63, 0x68, 0x7d),
            number: Color::Rgb(0xe0, 0xaf, 0x68),
            constant: Color::Rgb(0xe0, 0xaf, 0x68),
        }
    }

    fn light() -> Self {
        Self {
            name: ThemeName::Light,
            glyphs: UNICODE,
            text: Color::Rgb(0x24, 0x29, 0x36),
            dim: Color::Rgb(0x5d, 0x64, 0x74),
            faint: Color::Rgb(0x8a, 0x91, 0xa1),
            accent: Color::Rgb(0x3d, 0x59, 0xa1),
            border: Color::Rgb(0xc4, 0xc9, 0xd6),
            border_active: Color::Rgb(0x3d, 0x59, 0xa1),
            user: Color::Rgb(0x2b, 0x6c, 0xb0),
            tool: Color::Rgb(0x0f, 0x6c, 0x91),
            ok: Color::Rgb(0x3f, 0x7d, 0x14),
            err: Color::Rgb(0xb4, 0x23, 0x18),
            warn: Color::Rgb(0x8f, 0x5f, 0x02),
            add: Color::Rgb(0x3f, 0x7d, 0x14),
            del: Color::Rgb(0xb4, 0x23, 0x18),
            code: Color::Rgb(0x0f, 0x6c, 0x91),
            code_bg: Color::Rgb(0xef, 0xf1, 0xf5),
            heading: Color::Rgb(0x6f, 0x42, 0xc1),
            link: Color::Rgb(0x2b, 0x6c, 0xb0),
            quote: Color::Rgb(0x5d, 0x64, 0x74),
            key: Color::Rgb(0x8f, 0x5f, 0x02),
            sel_bg: Color::Rgb(0xd7, 0xe3, 0xff),
            sel_fg: Color::Rgb(0x15, 0x1a, 0x26),
            header_bg: Color::Rgb(0xee, 0xf1, 0xf6),
            keyword: Color::Rgb(0x6f, 0x42, 0xc1),
            ty: Color::Rgb(0x2b, 0x6c, 0xb0),
            func: Color::Rgb(0x2b, 0x6c, 0xb0),
            string: Color::Rgb(0x3f, 0x7d, 0x14),
            comment: Color::Rgb(0x8a, 0x91, 0xa1),
            number: Color::Rgb(0x8f, 0x5f, 0x02),
            constant: Color::Rgb(0x8f, 0x5f, 0x02),
        }
    }

    fn nord() -> Self {
        Self {
            name: ThemeName::Nord,
            glyphs: UNICODE,
            text: Color::Rgb(0xd8, 0xde, 0xe9),
            dim: Color::Rgb(0x7b, 0x85, 0x98),
            faint: Color::Rgb(0x61, 0x6e, 0x88),
            accent: Color::Rgb(0x88, 0xc0, 0xd0),
            border: Color::Rgb(0x43, 0x4c, 0x5e),
            border_active: Color::Rgb(0x81, 0xa1, 0xc1),
            user: Color::Rgb(0x88, 0xc0, 0xd0),
            tool: Color::Rgb(0x8f, 0xbc, 0xbb),
            ok: Color::Rgb(0xa3, 0xbe, 0x8c),
            err: Color::Rgb(0xbf, 0x61, 0x6a),
            warn: Color::Rgb(0xeb, 0xcb, 0x8b),
            add: Color::Rgb(0xa3, 0xbe, 0x8c),
            del: Color::Rgb(0xbf, 0x61, 0x6a),
            code: Color::Rgb(0x8f, 0xbc, 0xbb),
            code_bg: Color::Rgb(0x2b, 0x30, 0x3b),
            heading: Color::Rgb(0xb4, 0x8e, 0xad),
            link: Color::Rgb(0x88, 0xc0, 0xd0),
            quote: Color::Rgb(0x7b, 0x85, 0x98),
            key: Color::Rgb(0xeb, 0xcb, 0x8b),
            sel_bg: Color::Rgb(0x3b, 0x42, 0x52),
            sel_fg: Color::Rgb(0xec, 0xef, 0xf4),
            header_bg: Color::Rgb(0x23, 0x28, 0x31),
            keyword: Color::Rgb(0xb4, 0x8e, 0xad),
            ty: Color::Rgb(0x8f, 0xbc, 0xbb),
            func: Color::Rgb(0x88, 0xc0, 0xd0),
            string: Color::Rgb(0xa3, 0xbe, 0x8c),
            comment: Color::Rgb(0x61, 0x6e, 0x88),
            number: Color::Rgb(0xd0, 0x87, 0x70),
            constant: Color::Rgb(0xd0, 0x87, 0x70),
        }
    }

    fn solarized() -> Self {
        Self {
            name: ThemeName::Solarized,
            glyphs: UNICODE,
            text: Color::Rgb(0x58, 0x6e, 0x75),
            dim: Color::Rgb(0x65, 0x7b, 0x83),
            faint: Color::Rgb(0x93, 0xa1, 0xa1),
            accent: Color::Rgb(0x26, 0x8b, 0xd2),
            border: Color::Rgb(0xd3, 0xcb, 0xb7),
            border_active: Color::Rgb(0x26, 0x8b, 0xd2),
            user: Color::Rgb(0x26, 0x8b, 0xd2),
            tool: Color::Rgb(0x2a, 0xa1, 0x98),
            ok: Color::Rgb(0x85, 0x99, 0x00),
            err: Color::Rgb(0xdc, 0x32, 0x2f),
            warn: Color::Rgb(0xb5, 0x89, 0x00),
            add: Color::Rgb(0x85, 0x99, 0x00),
            del: Color::Rgb(0xdc, 0x32, 0x2f),
            code: Color::Rgb(0x2a, 0xa1, 0x98),
            code_bg: Color::Rgb(0xee, 0xe8, 0xd5),
            heading: Color::Rgb(0x6c, 0x71, 0xc4),
            link: Color::Rgb(0x26, 0x8b, 0xd2),
            quote: Color::Rgb(0x93, 0xa1, 0xa1),
            key: Color::Rgb(0xb5, 0x89, 0x00),
            sel_bg: Color::Rgb(0xe4, 0xdd, 0xc8),
            sel_fg: Color::Rgb(0x07, 0x36, 0x42),
            header_bg: Color::Rgb(0xee, 0xe8, 0xd5),
            keyword: Color::Rgb(0x6c, 0x71, 0xc4),
            ty: Color::Rgb(0x26, 0x8b, 0xd2),
            func: Color::Rgb(0x26, 0x8b, 0xd2),
            string: Color::Rgb(0x85, 0x99, 0x00),
            comment: Color::Rgb(0x93, 0xa1, 0xa1),
            number: Color::Rgb(0xb5, 0x89, 0x00),
            constant: Color::Rgb(0xcb, 0x4b, 0x16),
        }
    }

    fn mono() -> Self {
        let c = Color::Reset;
        Self {
            name: ThemeName::Mono,
            glyphs: ASCII,
            text: c,
            dim: c,
            faint: c,
            accent: c,
            border: c,
            border_active: c,
            user: c,
            tool: c,
            ok: c,
            err: c,
            warn: c,
            add: c,
            del: c,
            code: c,
            code_bg: c,
            heading: c,
            link: c,
            quote: c,
            key: c,
            sel_bg: c,
            sel_fg: c,
            header_bg: c,
            keyword: c,
            ty: c,
            func: c,
            string: c,
            comment: c,
            number: c,
            constant: c,
        }
    }

    /// Main body text style.
    pub fn body(self) -> Style {
        Style::default().fg(self.text)
    }

    pub fn heading_style(self) -> Style {
        Style::default()
            .fg(self.heading)
            .add_modifier(Modifier::BOLD)
    }

    pub fn selection(self) -> Style {
        if self.name == ThemeName::Mono {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(self.sel_fg).bg(self.sel_bg)
        }
    }

    /// Highlighted key hints (`Enter` in the footer).
    pub fn key_style(self) -> Style {
        Style::default().fg(self.key)
    }

    pub fn syn(self, class: Syn) -> Style {
        let color = match class {
            Syn::Keyword => self.keyword,
            Syn::Type => self.ty,
            Syn::Function => self.func,
            Syn::String => self.string,
            Syn::Comment => self.comment,
            Syn::Number => self.number,
            Syn::Constant => self.constant,
            Syn::Punct => self.text,
            Syn::Tag => self.key,
        };
        if self.name == ThemeName::Mono {
            return match class {
                Syn::Comment => Style::default().add_modifier(Modifier::DIM),
                Syn::Keyword | Syn::Tag => Style::default().add_modifier(Modifier::BOLD),
                _ => Style::default(),
            };
        }
        match class {
            Syn::Comment => Style::default().fg(color).add_modifier(Modifier::ITALIC),
            Syn::Keyword | Syn::Tag => Style::default().fg(color).add_modifier(Modifier::BOLD),
            _ => Style::default().fg(color),
        }
    }

    /// Diff row style: `+` rows green, `-` rows red, hunk headers accent.
    pub fn diff_style(self, line: &str) -> Style {
        if line.starts_with("@@") {
            Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::BOLD)
        } else if line.starts_with('+') {
            Style::default().fg(self.add)
        } else if line.starts_with('-') {
            Style::default().fg(self.del)
        } else {
            Style::default().fg(self.dim)
        }
    }

    /// Spinner frame (`◐`-family; ASCII fallback for mono).
    pub fn spinner(self, tick: usize) -> &'static str {
        const BRAILLE: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        const PLAIN: [&str; 4] = ["-", "\\", "|", "/"];
        if self.name == ThemeName::Mono {
            PLAIN[tick % PLAIN.len()]
        } else {
            BRAILLE[tick % BRAILLE.len()]
        }
    }

    /// Context-usage colour: green while comfortable, yellow near the budget,
    /// red when auto-compaction is about to fire.
    pub fn usage_color(self, used: u64, budget: u64) -> Color {
        if budget == 0 {
            return self.ok;
        }
        let pct = used.saturating_mul(100) / budget;
        if pct >= 85 {
            self.err
        } else if pct >= 60 {
            self.warn
        } else {
            self.ok
        }
    }
}

/// Whether to use the conservative ASCII glyph set (`KAZI_ASCII=1`).
pub fn ascii_requested() -> bool {
    std::env::var_os("KAZI_ASCII").is_some()
}

/// Strip terminal control sequences from untrusted text (tool output, model
/// answers) so a stray ANSI colour can never corrupt the screen. Tabs become
/// four spaces; newlines survive.
pub(crate) fn sanitize_terminal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                // CSI: ESC [ ... final byte 0x40..=0x7e
                Some('[') => {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... BEL or ESC \
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // lone escape: drop it
                _ => {}
            }
        } else if c == '\t' {
            out.push_str("    ");
        } else if c == '\n' {
            out.push('\n');
        } else if c.is_control() {
            // other control bytes (CR, BEL, backspace, ...) are noise
        } else {
            out.push(c);
        }
    }
    out
}

/// Resolve a theme from a config string + env override.
pub fn theme_from_config(name: Option<&str>) -> Theme {
    let mut theme = name
        .and_then(ThemeName::parse)
        .map(Theme::for_name)
        .unwrap_or_default();
    if ascii_requested() {
        theme.glyphs = ASCII;
    }
    theme
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_parse_and_cycle() {
        assert_eq!(ThemeName::parse("LIGHT"), Some(ThemeName::Light));
        assert_eq!(ThemeName::parse("plain"), Some(ThemeName::Mono));
        assert_eq!(ThemeName::parse("Nord"), Some(ThemeName::Nord));
        assert_eq!(ThemeName::parse("solarised"), Some(ThemeName::Solarized));
        assert_eq!(ThemeName::parse("neon"), None);
        assert_eq!(ThemeName::Dark.cycle(), ThemeName::Light);
        // every theme is reachable by cycling and parses back to itself
        let mut t = ThemeName::Dark;
        for _ in 0..ThemeName::ALL.len() {
            assert_eq!(ThemeName::parse(t.as_str()), Some(t));
            t = t.cycle();
        }
        assert_eq!(t, ThemeName::Dark);
        assert_eq!(ThemeName::Mono.cycle(), ThemeName::Dark);
        assert_eq!(ThemeName::names().len(), ThemeName::ALL.len());
    }

    #[test]
    fn mono_uses_ascii_and_has_no_colour() {
        let t = Theme::for_name(ThemeName::Mono);
        assert_eq!(t.glyphs.prompt, ">");
        assert_eq!(t.accent, Color::Reset);
        assert_eq!(
            t.syn(Syn::Comment),
            Style::default().add_modifier(Modifier::DIM)
        );
    }

    #[test]
    fn usage_colour_has_three_bands() {
        let t = Theme::default();
        assert_eq!(t.usage_color(0, 100), t.ok);
        assert_eq!(t.usage_color(65, 100), t.warn);
        assert_eq!(t.usage_color(90, 100), t.err);
    }

    #[test]
    fn diff_rows_are_classified() {
        let t = Theme::default();
        assert_eq!(t.diff_style("+new"), Style::default().fg(t.add));
        assert_eq!(t.diff_style("-old"), Style::default().fg(t.del));
        assert_eq!(t.diff_style(" ctx"), Style::default().fg(t.dim));
    }

    #[test]
    fn config_theme_resolution() {
        assert_eq!(theme_from_config(Some("light")).name, ThemeName::Light);
        assert_eq!(theme_from_config(Some("bogus")).name, ThemeName::Dark);
    }
}
