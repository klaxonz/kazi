//! Skills: reusable procedures the model loads on demand.
//!
//! A skill is a directory with a `SKILL.md` inside:
//!
//! ```text
//! .kazi/skills/release/SKILL.md
//! ---
//! description: Cut a release — bump, changelog, tag, push
//! ---
//! 1. cargo fmt / clippy / test ...
//! ```
//!
//! Only the name and description ride in the system prompt (a catalog); the
//! body is fetched with the `use_skill` tool when the task actually matches.
//! That keeps a repository with thirty skills from spending thirty pages of
//! context on every task.

use std::path::{Path, PathBuf};

/// Bodies are capped: a skill is a procedure, not a book.
pub const MAX_BODY_CHARS: usize = 32 * 1024;
/// More than this and the catalog itself becomes the context problem.
pub const MAX_SKILLS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// From the user's config dir (every project).
    Global,
    /// From `.kazi/skills` in the repository.
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub path: PathBuf,
    pub scope: Scope,
    /// `true` when the body was longer than the cap.
    pub truncated: bool,
}

impl Skill {
    /// One line for the system-prompt catalog.
    pub fn catalog_line(&self) -> String {
        let desc = self.description.lines().next().unwrap_or("").trim();
        if desc.is_empty() {
            format!("- {}", self.name)
        } else {
            format!("- {} — {}", self.name, crate::tools::clip_line(desc, 160))
        }
    }
}

/// The catalog block appended to the system prompt (`None` without skills).
pub fn catalog(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from(
        "\n\n# Available skills\n\
         The project ships procedures you can load with the `use_skill` tool.\n\
         Load one when the task clearly matches its description; do not load\n\
         skills \"just in case\".\n",
    );
    for s in skills {
        out.push_str(&s.catalog_line());
        out.push('\n');
    }
    Some(out)
}

/// Discover skills visible from `cwd`: global first, then `.kazi/skills` from
/// the outermost ancestor inward (nearer overrides a farther skill of the same
/// name).
pub fn discover(cwd: &Path) -> Vec<Skill> {
    let mut dirs: Vec<(PathBuf, Scope)> = Vec::new();
    if let Some(dir) = dirs::config_dir() {
        dirs.push((dir.join("kazi").join("skills"), Scope::Global));
    }
    let mut chain: Vec<&Path> = cwd.ancestors().collect();
    chain.reverse();
    for dir in chain {
        dirs.push((dir.join(".kazi").join("skills"), Scope::Project));
    }

    let mut out: Vec<Skill> = Vec::new();
    for (dir, scope) in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<Skill> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = skill_name(&path) else {
                continue;
            };
            let file = path.join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let parsed = parse(&text);
            let (body, truncated) = if parsed.body.chars().count() > MAX_BODY_CHARS {
                (
                    parsed.body.chars().take(MAX_BODY_CHARS).collect::<String>(),
                    true,
                )
            } else {
                (parsed.body, false)
            };
            if body.trim().is_empty() {
                continue;
            }
            found.push(Skill {
                name,
                description: parsed.description,
                body,
                path: file,
                scope,
                truncated,
            });
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        for skill in found {
            match out.iter_mut().find(|s| s.name == skill.name) {
                Some(existing) => *existing = skill,
                None => out.push(skill),
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.truncate(MAX_SKILLS);
    out
}

/// `skills/release/SKILL.md` → `release` when the directory name is sane.
fn skill_name(dir: &Path) -> Option<String> {
    let name = dir.file_name()?.to_str()?.to_ascii_lowercase();
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then_some(name)
}

struct Parsed {
    description: String,
    body: String,
}

/// Front matter carries `description` (and an optional `name` that we ignore:
/// the directory is the identity, so renaming the folder is the rename).
fn parse(text: &str) -> Parsed {
    let mut description = String::new();
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
                    if k.trim().eq_ignore_ascii_case("description") {
                        description = v.trim().to_string();
                    }
                }
            }
            if closed {
                body = &rest[consumed..];
            }
        }
    }
    let body = body.trim_start_matches(['\r', '\n']).to_string();
    // no front matter: the first non-empty line is the best description we have
    let description = if description.is_empty() {
        body.lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| crate::tools::clip_line(l, 160))
            .unwrap_or_default()
    } else {
        description
    };
    Parsed { description, body }
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
        let d = std::env::temp_dir().join(format!("kazi-skill-{name}-{id}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_skill(dir: &Path, name: &str, text: &str) {
        let d = dir.join("skills").join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), text).unwrap();
    }

    fn discover_in(root: &Path) -> Vec<Skill> {
        // discover() also walks the real filesystem above the temp dir, so the
        // tests assert on their own skills rather than the exact list
        discover(root)
    }

    #[test]
    fn front_matter_is_parsed_and_the_directory_is_the_name() {
        let root = tempdir("parse");
        let kazi = root.join(".kazi");
        write_skill(
            &kazi,
            "release",
            "---\ndescription: Cut a release\n---\n1. bump\n2. tag",
        );
        let skills = discover_in(&root);
        let s = skills
            .iter()
            .find(|s| s.name == "release")
            .expect("release");
        assert_eq!(s.description, "Cut a release");
        assert!(s.body.starts_with("1. bump"), "{:?}", s.body);
        assert_eq!(s.path, kazi.join("skills/release/SKILL.md"));
    }

    #[test]
    fn description_falls_back_to_the_first_content_line() {
        let root = tempdir("fallback");
        write_skill(
            &root.join(".kazi"),
            "deploy",
            "# Deploy\nPush to staging safely",
        );
        let s = &discover_in(&root)
            .into_iter()
            .find(|s| s.name == "deploy")
            .unwrap();
        assert_eq!(s.description, "Push to staging safely");
    }

    #[test]
    fn nearer_skills_override_and_bad_names_are_skipped() {
        let root = tempdir("override");
        write_skill(&root.join(".kazi"), "style", "outer");
        std::fs::write(root.join(".kazi/skills/style/SKILL.md"), "outer body").unwrap();
        let nested = root.join("app");
        write_skill(&nested.join(".kazi"), "style", "inner body");
        write_skill(&nested.join(".kazi"), "Bad Name", "nope");
        // a directory without SKILL.md is not a skill
        std::fs::create_dir_all(nested.join(".kazi/skills/empty")).unwrap();

        let skills = discover_in(&nested);
        let style = skills.iter().find(|s| s.name == "style").unwrap();
        assert_eq!(style.body, "inner body");
        assert!(!skills.iter().any(|s| s.name.contains(' ')));
        assert!(!skills.iter().any(|s| s.name == "empty"));
    }

    #[test]
    fn oversized_bodies_are_truncated_and_the_catalog_is_compact() {
        let root = tempdir("cap");
        write_skill(
            &root.join(".kazi"),
            "big",
            &format!(
                "---\ndescription: Big skill\n---\n{}",
                "x".repeat(MAX_BODY_CHARS + 10)
            ),
        );
        let skills = discover_in(&root);
        let big = skills.iter().find(|s| s.name == "big").unwrap();
        assert!(big.truncated);
        assert_eq!(big.body.chars().count(), MAX_BODY_CHARS);

        let cat = catalog(&skills).unwrap();
        assert!(cat.contains("# Available skills"));
        assert!(cat.contains("- big — Big skill"));
        assert!(catalog(&[]).is_none());
    }
}
