//! Small, dependency-free line diff used to show file edits in the UI.
//!
//! Edits made by `write_file` / `apply_patch` are captured by the engine as a
//! *unified diff* (the familiar `@@ -a,b +c,d @@` form). The renderer in the
//! TUI turns those lines into a coloured, collapsed card; `kazi --output json`
//! carries the raw text so scripts can audit what changed.
//!
//! The algorithm is deliberately simple: strip the common prefix/suffix, then
//! run an LCS table on the remaining middle (edits are normally small). A
//! pathological middle (thousands of changed lines) falls back to a coarse
//! "delete everything / add everything" hunk rather than burning memory.

/// How many changed lines we are willing to align exactly. Above this the
/// middle block is reported as one replacement — still truthful, just less
/// pretty, and always cheap.
const ALIGN_CAP: usize = 1_200;

/// One rendered unified diff plus the counters a card wants to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnifiedDiff {
    /// The hunk text (`@@`, ` `, `-`, `+` lines), newline-separated.
    pub text: String,
    pub added: usize,
    pub removed: usize,
}

impl UnifiedDiff {
    /// `(added, removed)` for a card header, when only the text survived.
    pub fn counts(text: &str) -> (usize, usize) {
        text.lines().fold((0, 0), |(a, d), line| {
            if line.starts_with("+++") || line.starts_with("---") {
                (a, d)
            } else if line.starts_with('+') {
                (a + 1, d)
            } else if line.starts_with('-') {
                (a, d + 1)
            } else {
                (a, d)
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Eq,
    Del,
    Ins,
}

#[derive(Debug, Clone)]
struct Row<'a> {
    tag: Tag,
    text: &'a str,
    /// 1-based line number in the old file (`0` = not present).
    old_no: usize,
    /// 1-based line number in the new file (`0` = not present).
    new_no: usize,
}

/// Compute a unified diff between two text blobs.
///
/// `path` is accepted for call-site readability; the returned text starts with
/// the hunks (the caller already knows the path). Returns `None` when the
/// contents are identical.
pub fn unified(old: &str, new: &str, path: &str, context: usize) -> Option<UnifiedDiff> {
    let _ = path;
    if old == new {
        return None;
    }
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let rows = build_rows(&old_lines, &new_lines);
    Some(render_hunks(&rows, context.max(1)))
}

/// One-row summary suitable for a log line.
pub fn summary(diff: &UnifiedDiff, path: &str) -> String {
    format!("{} (+{} −{})", path, diff.added, diff.removed)
}

fn build_rows<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Row<'a>> {
    // common prefix / suffix keep the LCS small on typical single-block edits
    let mut pre = 0;
    while pre < old.len() && pre < new.len() && old[pre] == new[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < old.len() - pre
        && suf < new.len() - pre
        && old[old.len() - 1 - suf] == new[new.len() - 1 - suf]
    {
        suf += 1;
    }

    let mut rows: Vec<Row<'a>> = Vec::with_capacity(old.len().max(new.len()));
    for (i, line) in old.iter().take(pre).enumerate() {
        rows.push(Row {
            tag: Tag::Eq,
            text: line,
            old_no: i + 1,
            new_no: i + 1,
        });
    }

    let old_mid = &old[pre..old.len() - suf];
    let new_mid = &new[pre..new.len() - suf];
    let mut old_no = pre + 1;
    let mut new_no = pre + 1;
    if old_mid.len() > ALIGN_CAP || new_mid.len() > ALIGN_CAP {
        // coarse: treat the whole middle as replaced
        for line in old_mid {
            rows.push(Row {
                tag: Tag::Del,
                text: line,
                old_no,
                new_no: 0,
            });
            old_no += 1;
        }
        for line in new_mid {
            rows.push(Row {
                tag: Tag::Ins,
                text: line,
                old_no: 0,
                new_no,
            });
            new_no += 1;
        }
    } else {
        let ops = lcs_ops(old_mid, new_mid);
        for op in ops {
            match op {
                Op::Eq(i) => {
                    rows.push(Row {
                        tag: Tag::Eq,
                        text: old_mid[i],
                        old_no,
                        new_no,
                    });
                    old_no += 1;
                    new_no += 1;
                }
                Op::Del(i) => {
                    rows.push(Row {
                        tag: Tag::Del,
                        text: old_mid[i],
                        old_no,
                        new_no: 0,
                    });
                    old_no += 1;
                }
                Op::Ins(j) => {
                    rows.push(Row {
                        tag: Tag::Ins,
                        text: new_mid[j],
                        old_no: 0,
                        new_no,
                    });
                    new_no += 1;
                }
            }
        }
    }

    let base = old.len() - suf + 1;
    for (k, line) in new.iter().skip(new.len() - suf).enumerate() {
        rows.push(Row {
            tag: Tag::Eq,
            text: line,
            old_no: base + k,
            new_no: base + k,
        });
    }
    rows
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Eq(usize),
    Del(usize),
    Ins(usize),
}

/// Classic LCS backtrack over two small slices.
fn lcs_ops(old: &[&str], new: &[&str]) -> Vec<Op> {
    let n = old.len();
    let m = new.len();
    let mut dp = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old[i] == new[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[i] == new[j] {
            ops.push(Op::Eq(i));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(Op::Del(i));
            i += 1;
        } else {
            ops.push(Op::Ins(j));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Del(i));
        i += 1;
    }
    while j < m {
        ops.push(Op::Ins(j));
        j += 1;
    }
    ops
}

fn render_hunks(rows: &[Row<'_>], context: usize) -> UnifiedDiff {
    let mut out: Vec<String> = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;

    // ranges of rows to include: every change ± context, merged when close
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        if rows[i].tag == Tag::Eq {
            i += 1;
            continue;
        }
        let start = i.saturating_sub(context);
        let mut end = i;
        while end + 1 < rows.len() && rows[end + 1].tag != Tag::Eq {
            end += 1;
        }
        let end = (end + context).min(rows.len() - 1);
        match ranges.last_mut() {
            // overlapping or adjacent windows become one hunk
            Some(last) if start <= last.1 + 1 => last.1 = end,
            _ => ranges.push((start, end)),
        }
        i = end + 1;
    }

    for (start, end) in ranges {
        let (mut old_count, mut new_count) = (0usize, 0usize);
        for row in &rows[start..=end] {
            if row.tag != Tag::Ins {
                old_count += 1;
            }
            if row.tag != Tag::Del {
                new_count += 1;
            }
            match row.tag {
                Tag::Eq => {}
                Tag::Del => removed += 1,
                Tag::Ins => added += 1,
            }
        }
        // A hunk may open on an inserted line (old_no == 0); the old start is
        // then the first row that still has a real old line number.
        let old_start = rows[start..=end]
            .iter()
            .find(|r| r.old_no > 0)
            .map(|r| r.old_no)
            .unwrap_or(0);
        let new_start = rows[start..=end]
            .iter()
            .find(|r| r.new_no > 0)
            .map(|r| r.new_no)
            .unwrap_or(0);
        out.push(format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@"
        ));
        for row in &rows[start..=end] {
            let sign = match row.tag {
                Tag::Eq => ' ',
                Tag::Del => '-',
                Tag::Ins => '+',
            };
            out.push(format!("{sign}{}", row.text));
        }
    }

    UnifiedDiff {
        text: out.join("\n"),
        added,
        removed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_input_has_no_diff() {
        assert!(unified("a\nb\n", "a\nb\n", "x", 3).is_none());
        assert!(unified("", "", "x", 3).is_none());
    }

    #[test]
    fn single_line_change_is_one_hunk() {
        let old = "one\ntwo\nthree\nfour\nfive\n";
        let new = "one\ntwo\nTHREE\nfour\nfive\n";
        let d = unified(old, new, "f.txt", 1).unwrap();
        assert_eq!(d.added, 1);
        assert_eq!(d.removed, 1);
        let text = d.text;
        assert!(text.starts_with("@@ -2,3 +2,3 @@"), "got: {text}");
        assert!(text.contains("-three"));
        assert!(text.contains("+THREE"));
        // context keeps the unchanged neighbours
        assert!(text.contains(" two"));
        assert!(text.contains(" four"));
    }

    #[test]
    fn separated_changes_become_separate_hunks() {
        let old: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        let new = old
            .replace("line2\n", "LINE2\n")
            .replace("line18\n", "LINE18\n");
        let d = unified(&old, &new, "f", 1).unwrap();
        assert_eq!(
            d.text.lines().filter(|l| l.starts_with("@@")).count(),
            2,
            "got: {}",
            d.text
        );
        assert_eq!(d.added, 2);
        assert_eq!(d.removed, 2);
    }

    #[test]
    fn insertion_at_the_top_and_bottom() {
        let d = unified("", "hello\n", "new.txt", 3).unwrap();
        assert_eq!((d.added, d.removed), (1, 0));
        assert!(d.text.contains("+hello"), "got: {}", d.text);

        let d = unified("hello\n", "hello\nworld\n", "f", 3).unwrap();
        assert_eq!((d.added, d.removed), (1, 0));
        assert!(d.text.contains(" hello"));
        assert!(d.text.contains("+world"));
    }

    #[test]
    fn deletion_only() {
        let d = unified("a\nb\nc\n", "a\nc\n", "f", 3).unwrap();
        assert_eq!((d.added, d.removed), (0, 1));
        assert!(d.text.contains("-b"));
    }

    #[test]
    fn counts_read_back_from_the_text() {
        let d = unified("a\nb\n", "a\nc\nd\n", "f", 3).unwrap();
        assert_eq!(UnifiedDiff::counts(&d.text), (d.added, d.removed));
    }

    #[test]
    fn multiline_rewrite_is_aligned_not_whole_file() {
        let old = "keep1\nkeep2\nold_a\nold_b\nkeep3\n";
        let new = "keep1\nkeep2\nnew_a\nnew_b\nkeep3\n";
        let d = unified(old, new, "f", 1).unwrap();
        assert_eq!((d.added, d.removed), (2, 2));
        assert!(d.text.contains(" keep2"));
    }

    #[test]
    fn summary_is_compact() {
        let d = unified("a\n", "b\n", "src/main.rs", 3).unwrap();
        assert_eq!(summary(&d, "src/main.rs"), "src/main.rs (+1 −1)");
    }
}
