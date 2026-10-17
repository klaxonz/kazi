//! Project instructions: the files a repository uses to teach the agent how it
//! should work there.
//!
//! Discovery walks from the working directory UP to the filesystem root,
//! collecting `KAZI.md` (or `AGENTS.md` — the cross-tool convention) at each
//! level, plus a global file under the config dir. The nearest file is the most
//! specific one, so it comes last in the prompt and wins on conflicts.
//!
//! Budgets are deliberate: a repository can contain a 2 MB "instructions" file
//! by accident (or on purpose) and it must not eat the context window.

use std::path::{Path, PathBuf};

/// Per-file cap (characters).
pub const MAX_FILE_CHARS: usize = 16 * 1024;
/// Total cap across every discovered file (characters).
pub const MAX_TOTAL_CHARS: usize = 48 * 1024;
/// Never load more than this many files, even if every directory has one.
pub const MAX_FILES: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    /// Absolute path it was read from.
    pub path: PathBuf,
    /// `true` for the user-level file under the config dir.
    pub global: bool,
    /// Characters kept (after per-file truncation).
    pub chars: usize,
    /// `true` when the file was longer than the budget.
    pub truncated: bool,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instructions {
    /// Outer → inner order: global first, then parent dirs, then the cwd.
    pub files: Vec<InstructionFile>,
    /// Things the user should know about (unreadable file, over budget).
    pub notes: Vec<String>,
}

impl Instructions {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// `(path, chars, truncated)` for `/instructions`.
    pub fn sources(&self) -> Vec<(String, usize, bool)> {
        self.files
            .iter()
            .map(|f| (f.path.display().to_string(), f.chars, f.truncated))
            .collect()
    }

    /// The block appended to the system prompt, with provenance and precedence
    /// spelled out. `None` when nothing was found.
    pub fn render(&self) -> Option<String> {
        if self.files.is_empty() {
            return None;
        }
        let mut out = String::from(
            "\n\n# Project instructions\n\
             The user keeps these files in the repository as durable guidance for you.\n\
             Follow them. When two files disagree, the one listed LAST wins (it is the\n\
             closest to the working directory).\n",
        );
        for f in &self.files {
            out.push_str(&format!(
                "\n## {}{}\n{}\n",
                f.path.display(),
                if f.truncated { " (truncated)" } else { "" },
                f.text.trim_end()
            ));
        }
        Some(out)
    }
}

/// Which filenames count, in priority order (`KAZI.md` first, then the
/// cross-tool `AGENTS.md`; a directory with both contributes one file).
pub const FILE_NAMES: [&str; 2] = ["KAZI.md", "AGENTS.md"];

/// Discover the instruction files for `cwd`. Never fails: unreadable files
/// become a note, and a missing file simply contributes nothing.
pub fn discover(cwd: &Path) -> Instructions {
    discover_with(cwd, global_candidates().as_slice())
}

/// Same, but with explicit global paths (tests pass a temp dir).
pub fn discover_with(cwd: &Path, global_paths: &[PathBuf]) -> Instructions {
    let mut out = Instructions::default();
    let mut total = 0usize;

    for path in global_paths {
        // a missing user-level file is the normal case, not a warning
        if !path.is_file() {
            continue;
        }
        if let Some(file) = load(path, true, &mut out.notes) {
            total = push_with_budget(&mut out, file, total);
        }
    }

    // ancestors from the root down to cwd, so the closest file is last
    let mut chain: Vec<&Path> = cwd.ancestors().collect();
    chain.reverse();
    for dir in chain {
        for name in FILE_NAMES {
            let candidate = dir.join(name);
            if !candidate.is_file() {
                continue;
            }
            if let Some(file) = load(&candidate, false, &mut out.notes) {
                total = push_with_budget(&mut out, file, total);
            }
            break; // one file per directory: KAZI.md wins over AGENTS.md
        }
    }
    out
}

/// The user-level file(s), in the platform config dir.
pub fn global_candidates() -> Vec<PathBuf> {
    let Some(dir) = dirs::config_dir() else {
        return Vec::new();
    };
    let base = dir.join("kazi");
    FILE_NAMES.iter().map(|n| base.join(n)).collect()
}

fn push_with_budget(out: &mut Instructions, file: InstructionFile, total: usize) -> usize {
    if out.files.len() >= MAX_FILES {
        out.notes.push(format!(
            "{} not loaded: already at the {MAX_FILES}-file limit",
            file.path.display()
        ));
        return total;
    }
    if total >= MAX_TOTAL_CHARS {
        out.notes.push(format!(
            "{} not loaded: instruction budget of {} KiB exhausted",
            file.path.display(),
            MAX_TOTAL_CHARS / 1024
        ));
        return total;
    }
    let used = total + file.chars;
    out.files.push(file);
    used
}

fn load(path: &Path, global: bool, notes: &mut Vec<String>) -> Option<InstructionFile> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            notes.push(format!("cannot read {}: {e}", path.display()));
            return None;
        }
    };
    if bytes.iter().take(8192).any(|b| *b == 0) {
        notes.push(format!("{} looks binary — skipped", path.display()));
        return None;
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let chars = text.chars().count();
    let (text, truncated) = if chars > MAX_FILE_CHARS {
        (text.chars().take(MAX_FILE_CHARS).collect::<String>(), true)
    } else {
        (text, false)
    };
    if text.trim().is_empty() {
        return None;
    }
    Some(InstructionFile {
        path: path.to_path_buf(),
        global,
        chars: text.chars().count(),
        truncated,
        text,
    })
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
        let d = std::env::temp_dir().join(format!("kazi-instr-{name}-{id}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn walks_up_and_keeps_the_nearest_file_last() {
        let root = tempdir("order");
        let sub = root.join("crate").join("src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("KAZI.md"), "root rules").unwrap();
        std::fs::write(root.join("crate").join("AGENTS.md"), "crate rules").unwrap();

        let found = discover_with(&sub, &[]);
        let texts: Vec<&str> = found.files.iter().map(|f| f.text.trim()).collect();
        // the temp dir's own ancestors may add files, so assert on OUR two
        let root_pos = texts.iter().position(|t| *t == "root rules").expect("root");
        let crate_pos = texts
            .iter()
            .position(|t| *t == "crate rules")
            .expect("crate");
        assert!(
            root_pos < crate_pos,
            "outer file must come first: {texts:?}"
        );
        assert_eq!(
            found.files[crate_pos].path,
            root.join("crate").join("AGENTS.md")
        );
        // KAZI.md wins over AGENTS.md inside one directory
        std::fs::write(root.join("crate").join("src").join("KAZI.md"), "near rules").unwrap();
        std::fs::write(root.join("crate").join("src").join("AGENTS.md"), "shadowed").unwrap();
        let found = discover_with(&sub, &[]);
        assert!(found.files.iter().any(|f| f.text.trim() == "near rules"));
        assert!(!found.files.iter().any(|f| f.text.trim() == "shadowed"));
    }

    #[test]
    fn global_file_comes_first_and_can_be_absent() {
        let root = tempdir("global");
        let global = root.join("global.md");
        std::fs::write(&global, "be kind").unwrap();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("KAZI.md"), "local").unwrap();

        let found = discover_with(&cwd, std::slice::from_ref(&global));
        assert!(found.files[0].global);
        assert_eq!(found.files[0].text, "be kind");
        assert_eq!(found.files.last().unwrap().text.trim(), "local");

        // a missing global path is silently ignored (no note noise)
        let found = discover_with(&cwd, &[root.join("nope.md")]);
        assert_eq!(found.files.len(), 1);
        assert!(found.notes.is_empty());
    }

    #[test]
    fn oversized_files_are_truncated_with_a_note() {
        let root = tempdir("cap");
        std::fs::write(root.join("KAZI.md"), "x".repeat(MAX_FILE_CHARS + 500)).unwrap();
        let found = discover_with(&root, &[]);
        let file = found
            .files
            .iter()
            .find(|f| f.path == root.join("KAZI.md"))
            .expect("loaded");
        assert!(file.truncated);
        assert_eq!(file.chars, MAX_FILE_CHARS);
        assert!(found.render().unwrap().contains("(truncated)"));
    }

    #[test]
    fn render_is_empty_without_files_and_names_every_source() {
        let empty = Instructions::default();
        assert!(empty.render().is_none());
        assert!(empty.sources().is_empty());

        let root = tempdir("render");
        std::fs::write(root.join("KAZI.md"), "use tabs").unwrap();
        let found = discover_with(&root, &[]);
        let text = found.render().unwrap();
        assert!(text.contains("# Project instructions"));
        assert!(text.contains("use tabs"));
        assert!(text.contains("KAZI.md"));
        assert!(text.contains("listed LAST wins"));
    }
}
