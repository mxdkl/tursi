//! Line diffs for the transcript (§2): what an edit/write changed, with line
//! numbers on both sides, hunks separated by gaps — the shape the TUI renders
//! under `⎿ Updated <file> (+a -b)`.

/// One rendered diff row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: Kind,
    /// Line number in the old file (context and deletions).
    pub old_no: Option<usize>,
    /// Line number in the new file (context and additions).
    pub new_no: Option<usize>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Context,
    Added,
    Removed,
    /// Elided unchanged lines between hunks.
    Gap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// As the model named it (project-relative).
    pub file: String,
    pub added: usize,
    pub removed: usize,
    pub lines: Vec<DiffLine>,
}

/// Unchanged lines shown around each change.
const CONTEXT: usize = 3;
/// Beyond this many rows the diff is cut with a final gap — a 2000-line write
/// must not flood the transcript.
const MAX_ROWS: usize = 400;
/// LCS table cells before falling back to prefix/suffix trimming only.
const MAX_CELLS: usize = 4_000_000;

pub fn diff(file: &str, old: &str, new: &str) -> FileDiff {
    let o: Vec<&str> = old.lines().collect();
    let n: Vec<&str> = new.lines().collect();
    let ops = ops(&o, &n);
    let added = ops.iter().filter(|op| matches!(op, Op::Add(_))).count();
    let removed = ops.iter().filter(|op| matches!(op, Op::Del(_))).count();

    // Which op indices are changes; context is a window around them.
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, op)| !matches!(op, Op::Keep(..))).map(|(i, _)| i).collect();
    let mut show = vec![false; ops.len()];
    for &i in &changed {
        let from = i.saturating_sub(CONTEXT);
        for flag in show.iter_mut().skip(from).take(i + CONTEXT + 1 - from) {
            *flag = true;
        }
    }

    let mut lines = Vec::new();
    let mut in_gap = false;
    for (i, op) in ops.iter().enumerate() {
        if !show[i] {
            if !in_gap {
                lines.push(DiffLine { kind: Kind::Gap, old_no: None, new_no: None, text: String::new() });
                in_gap = true;
            }
            continue;
        }
        in_gap = false;
        if lines.len() >= MAX_ROWS {
            lines.push(DiffLine { kind: Kind::Gap, old_no: None, new_no: None, text: String::new() });
            break;
        }
        lines.push(match *op {
            Op::Keep(a, b) => DiffLine { kind: Kind::Context, old_no: Some(a + 1), new_no: Some(b + 1), text: o[a].to_string() },
            Op::Del(a) => DiffLine { kind: Kind::Removed, old_no: Some(a + 1), new_no: None, text: o[a].to_string() },
            Op::Add(b) => DiffLine { kind: Kind::Added, old_no: None, new_no: Some(b + 1), text: n[b].to_string() },
        });
    }
    // Leading/trailing gaps carry no information.
    while lines.first().is_some_and(|l| l.kind == Kind::Gap) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.kind == Kind::Gap) && lines.len() < MAX_ROWS {
        lines.pop();
    }
    FileDiff { file: file.to_string(), added, removed, lines }
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Keep(usize, usize),
    Del(usize),
    Add(usize),
}

/// Edit script old → new: common prefix/suffix trimmed, LCS on the middle
/// (plain deletions + additions when the middle is too big to table).
fn ops(o: &[&str], n: &[&str]) -> Vec<Op> {
    let mut prefix = 0;
    while prefix < o.len() && prefix < n.len() && o[prefix] == n[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < o.len() - prefix && suffix < n.len() - prefix && o[o.len() - 1 - suffix] == n[n.len() - 1 - suffix] {
        suffix += 1;
    }
    let (om, nm) = (&o[prefix..o.len() - suffix], &n[prefix..n.len() - suffix]);

    let mut out: Vec<Op> = (0..prefix).map(|i| Op::Keep(i, i)).collect();
    if om.len() * nm.len() <= MAX_CELLS && !om.is_empty() && !nm.is_empty() {
        // LCS lengths table, then walk back.
        let (h, w) = (om.len(), nm.len());
        let mut table = vec![0u32; (h + 1) * (w + 1)];
        for i in (0..h).rev() {
            for j in (0..w).rev() {
                table[i * (w + 1) + j] = if om[i] == nm[j] {
                    table[(i + 1) * (w + 1) + j + 1] + 1
                } else {
                    table[(i + 1) * (w + 1) + j].max(table[i * (w + 1) + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < h && j < w {
            if om[i] == nm[j] {
                out.push(Op::Keep(prefix + i, prefix + j));
                i += 1;
                j += 1;
            } else if table[(i + 1) * (w + 1) + j] >= table[i * (w + 1) + j + 1] {
                out.push(Op::Del(prefix + i));
                i += 1;
            } else {
                out.push(Op::Add(prefix + j));
                j += 1;
            }
        }
        out.extend((i..h).map(|i| Op::Del(prefix + i)));
        out.extend((j..w).map(|j| Op::Add(prefix + j)));
    } else {
        out.extend((0..om.len()).map(|i| Op::Del(prefix + i)));
        out.extend((0..nm.len()).map(|j| Op::Add(prefix + j)));
    }
    out.extend((0..suffix).map(|k| Op::Keep(o.len() - suffix + k, n.len() - suffix + k)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_middle_change_gets_numbers_context_and_counts() {
        let old = (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let new = old.replace("line 5", "line five").replace("line 6\n", "line 6\nline 6b\n");
        let d = diff("a.txt", &old, &new);
        assert_eq!((d.added, d.removed), (2, 1));
        let removed = d.lines.iter().find(|l| l.kind == Kind::Removed).unwrap();
        assert_eq!((removed.old_no, removed.new_no, removed.text.as_str()), (Some(5), None, "line 5"));
        let added: Vec<_> = d.lines.iter().filter(|l| l.kind == Kind::Added).collect();
        assert_eq!(added[0].new_no, Some(5));
        assert_eq!(added[1].text, "line 6b");
        // Three lines of context on each side, no gaps at the ends.
        assert_eq!(d.lines.first().unwrap().old_no, Some(2));
        assert_eq!(d.lines.last().unwrap().old_no, Some(9));
        assert!(!d.lines.iter().any(|l| l.kind == Kind::Gap));
    }

    #[test]
    fn far_apart_changes_are_separated_by_a_gap() {
        let old = (1..=40).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
        let new = old.replace("l2\n", "L2\n").replace("l38\n", "L38\n");
        let d = diff("b.txt", &old, &new);
        assert_eq!(d.lines.iter().filter(|l| l.kind == Kind::Gap).count(), 1);
        assert_eq!((d.added, d.removed), (2, 2));
    }

    #[test]
    fn new_file_is_all_additions() {
        let d = diff("new.rs", "", "fn main() {}\n");
        assert_eq!((d.added, d.removed), (1, 0));
        assert_eq!(d.lines[0].new_no, Some(1));
    }
}
