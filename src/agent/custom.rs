//! Custom slash commands: the project's own verbs.
//!
//! A file under `.kazi/commands/<name>.md` (or the global
//! `<config>/kazi/commands/`) becomes `/<name>` in the TUI. The file is a
//! prompt template: its body is sent as the task, with `$ARGUMENTS` (the raw
//! argument text), `$1`…`$9` (positional) substituted. Optional front matter
//! carries the description and an argument hint for the command list.
//!
//! Deliberately small: no shell execution, no scripting — a custom command is
//! a saved prompt that lives with the repository and is reviewable like any
//! other text file.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// From the user's config dir (available in every project).
    Global,
    /// From a `.kazi/commands` directory in the repository.
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    /// With the leading slash, e.g. `/review`.
    pub name: String,
    pub description: String,
    /// Shown after the name in help/completion, e.g. `<path>`.
    pub argument_hint: String,
    pub body: String,
    pub path: PathBuf,
    pub scope: Scope,
}

impl CustomCommand {
    /// `name + hint` as shown by the dropdown and `/help`.
    pub fn usage(&self) -> String {
        if self.argument_hint.is_empty() {
            self.name.clone()
        } else {
            format!("{} {}", self.name, self.argument_hint)
        }
    }

    /// The prompt for one invocation.
    ///
    /// `$ARGUMENTS` is the raw text after the command; `$1..$9` are positional.
    /// With neither placeholder the arguments are appended as a final line, so
    /// a template like "review this diff:" still receives them.
    pub fn expand(&self, args: &str) -> String {
        let args = args.trim();
        let positional: Vec<&str> = args.split_whitespace().collect();
        let mut used_placeholder = false;
        let mut out = String::with_capacity(self.body.len() + args.len());
        let chars: Vec<char> = self.body.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] != '$' {
                out.push(chars[i]);
                i += 1;
                continue;
            }
            // $ARGUMENTS
            if i + 1 < chars.len()
                && chars[i + 1..].starts_with(&"ARGUMENTS".chars().collect::<Vec<_>>())
            {
                out.push_str(args);
                i += 1 + "ARGUMENTS".len();
                used_placeholder = true;
                continue;
            }
            // $1..$9
            if let Some(d) = chars.get(i + 1).and_then(|c| c.to_digit(10)) {
                if (1..=9).contains(&d) {
                    out.push_str(positional.get(d as usize - 1).copied().unwrap_or(""));
                    i += 2;
                    used_placeholder = true;
                    continue;
                }
            }
            out.push('$');
            i += 1;
        }
        if !used_placeholder && !args.is_empty() {
            out.push_str("\n\n");
            out.push_str(args);
        }
        out
    }
}

/// Discover every custom command visible from `cwd`: global first, then
/// `.kazi/commands` from the outermost ancestor inward, so a nearer file with
/// the same name wins.
pub fn discover(cwd: &Path) -> Vec<CustomCommand> {
    let mut dirs: Vec<(PathBuf, Scope)> = Vec::new();
    if let Some(dir) = dirs::config_dir() {
        dirs.push((dir.join("kazi").join("commands"), Scope::Global));
    }
    let mut chain: Vec<&Path> = cwd.ancestors().collect();
    chain.reverse();
    for dir in chain {
        dirs.push((dir.join(".kazi").join("commands"), Scope::Project));
    }

    let mut out: Vec<CustomCommand> = Vec::new();
    for (dir, scope) in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("md"))
            .collect();
        files.sort();
        for path in files {
            let Some(name) = command_name(&path) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let parsed = parse(&text);
            if parsed.body.trim().is_empty() {
                continue;
            }
            let cmd = CustomCommand {
                name,
                description: parsed.description,
                argument_hint: parsed.argument_hint,
                body: parsed.body,
                path,
                scope,
            };
            // the nearer file replaces the earlier one
            match out.iter_mut().find(|c| c.name == cmd.name) {
                Some(existing) => *existing = cmd,
                None => out.push(cmd),
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Expand a whole input line when it starts with a known custom command
/// (`/review src/main.rs`). `None` when the line is not a custom command, so
/// callers can fall through to normal task handling. Built-ins always win.
pub fn expand_line(line: &str, cwd: &Path) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('/')?;
    let (verb, args) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None => (rest, ""),
    };
    let name = format!("/{}", verb.to_ascii_lowercase());
    if crate::agent::console::is_builtin_command(&name) {
        return None;
    }
    let found = discover(cwd);
    let cmd = found.iter().find(|c| c.name == name)?;
    Some(cmd.expand(args))
}

/// `file.md` → `/file` when the stem is a sane command name.
fn command_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?.to_ascii_lowercase();
    let ok = !stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then(|| format!("/{stem}"))
}

struct Parsed {
    description: String,
    argument_hint: String,
    body: String,
}

/// Split optional `---` front matter from the body. Unknown keys are ignored
/// (a typo must not make the command silently empty).
fn parse(text: &str) -> Parsed {
    let mut description = String::new();
    let mut argument_hint = String::new();
    let mut body = text;
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text);
    if let Some(rest) = trimmed.strip_prefix("---") {
        let rest = rest.strip_prefix('\r').unwrap_or(rest);
        if let Some(rest) = rest.strip_prefix('\n') {
            let mut consumed = 0usize;
            let mut closed = false;
            for line in rest.split_inclusive('\n') {
                let clean = line.trim_end();
                if clean == "---" {
                    consumed += line.len();
                    closed = true;
                    break;
                }
                consumed += line.len();
                if let Some((k, v)) = clean.split_once(':') {
                    match k.trim().to_ascii_lowercase().as_str() {
                        "description" => description = v.trim().to_string(),
                        "argument-hint" | "argument_hint" | "args" => {
                            argument_hint = v.trim().to_string()
                        }
                        _ => {}
                    }
                }
            }
            if closed {
                body = &rest[consumed..];
            }
        }
    }
    Parsed {
        description,
        argument_hint,
        body: body.trim_start_matches(['\r', '\n']).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let id = std::process::id() as u64
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;
        let d = std::env::temp_dir().join(format!("kazi-cmd-{name}-{id}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn front_matter_is_parsed_and_body_kept() {
        let parsed = parse(
            "---\ndescription: Review a diff\nargument-hint: <path>\nunknown: ignored\n---\nLook at $ARGUMENTS",
        );
        assert_eq!(parsed.description, "Review a diff");
        assert_eq!(parsed.argument_hint, "<path>");
        assert_eq!(parsed.body, "Look at $ARGUMENTS");

        // no front matter: the whole file is the body
        let parsed = parse("just a prompt");
        assert_eq!(parsed.body, "just a prompt");
        assert!(parsed.description.is_empty());
        // an unterminated front matter is NOT treated as metadata
        let parsed = parse("---\ndescription: x\nno close");
        assert!(parsed.body.starts_with("---"));
    }

    #[test]
    fn expansion_handles_arguments_and_positionals() {
        let cmd = |body: &str| CustomCommand {
            name: "/x".into(),
            description: String::new(),
            argument_hint: String::new(),
            body: body.into(),
            path: PathBuf::new(),
            scope: Scope::Project,
        };
        assert_eq!(
            cmd("fix $ARGUMENTS now").expand("the test"),
            "fix the test now"
        );
        assert_eq!(cmd("$1 then $2").expand("a b"), "a then b");
        assert_eq!(cmd("$9").expand("a"), "", "missing positional is empty");
        // no placeholder -> arguments appended
        assert_eq!(
            cmd("review:").expand("src/main.rs"),
            "review:\n\nsrc/main.rs"
        );
        // empty args never leave a dangling suffix
        assert_eq!(cmd("review:").expand("   "), "review:");
        // $5 is a placeholder too (empty without five arguments) ...
        assert_eq!(cmd("cost is $5").expand(""), "cost is ");
        assert_eq!(cmd("$5").expand("a b c d e"), "e");
        // ... while a bare dollar stays literal
        assert_eq!(cmd("cost is $ ").expand(""), "cost is $ ");
    }

    #[test]
    fn discovery_prefers_the_nearest_file_and_skips_bad_names() {
        let root = tempdir("discover");
        let nested = root.join("app");
        let dir = nested.join(".kazi").join("commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(root.join(".kazi").join("commands")).unwrap();
        std::fs::write(dir.join("review.md"), "inner $ARGUMENTS").unwrap();
        std::fs::write(dir.join("Bad Name.md"), "nope").ok();
        std::fs::write(dir.join("notes.txt"), "not a command").ok();
        std::fs::write(dir.join("empty.md"), "   ").ok();

        std::fs::write(
            root.join(".kazi").join("commands").join("review.md"),
            "outer $ARGUMENTS",
        )
        .unwrap();
        std::fs::write(
            root.join(".kazi").join("commands").join("only-root.md"),
            "root",
        )
        .unwrap();

        let cmds = discover(&nested);
        let review = cmds.iter().find(|c| c.name == "/review").expect("review");
        assert_eq!(review.body, "inner $ARGUMENTS", "nearest wins");
        assert!(
            cmds.iter().any(|c| c.name == "/only-root"),
            "outer still visible"
        );
        assert!(!cmds.iter().any(|c| c.name == "/bad name"));
        assert!(!cmds.iter().any(|c| c.name == "/notes"));
        assert!(!cmds.iter().any(|c| c.name == "/empty"));

        // the hint shows up in the usage line
        assert_eq!(
            CustomCommand {
                argument_hint: "<path>".into(),
                ..review.clone()
            }
            .usage(),
            "/review <path>"
        );
    }

    #[test]
    fn expand_line_only_touches_project_commands() {
        let root = tempdir("line");
        let dir = root.join(".kazi").join("commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("review.md"), "review $ARGUMENTS").unwrap();
        std::fs::write(dir.join("undo.md"), "hijack").unwrap();

        assert_eq!(
            expand_line("/review src/main.rs", &root).as_deref(),
            Some("review src/main.rs")
        );
        // names are case-insensitive; with no arguments the placeholder empties
        assert_eq!(
            expand_line("/REVIEW", &root)
                .map(|s| s.trim().to_string())
                .as_deref(),
            Some("review")
        );
        // built-ins never lose, and unknown names are not ours
        assert_eq!(expand_line("/undo", &root), None);
        assert_eq!(expand_line("/nope x", &root), None);
        assert_eq!(expand_line("plain task", &root), None);
    }
}
