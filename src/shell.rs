//! Shell lexing for steps (§4.1). Steps are real shell scripts: chains,
//! `$(…)`, heredocs, loops and multi-line bodies are all fine — the sandbox
//! is the security boundary, not this lexer. What it still catches is what
//! would hang or silently misbehave in the step runner (one long-lived shell
//! fed over stdin): a bare `&`, `&>` under plain sh, an unterminated heredoc
//! or quote (either swallows the runner's sentinel and the step hangs until
//! its timeout). Other syntax errors are caught by the shell's own parse-only
//! mode before the step runs. It also splits a step into its simple commands
//! for the run-before-done and lead-mode heuristics.

use anyhow::{Result, bail};

#[derive(Clone, Copy, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

/// What one scan of a step found.
struct Scan {
    /// Byte ranges between simple commands: operators (`|`, `||`, `&&`, `;`,
    /// newlines), comments, and heredoc bodies.
    cuts: Vec<(usize, usize)>,
    /// A top-level `&>` / `&>>`: bash's both-streams redirect.
    amp_redirect: bool,
}

/// Where a `#` starts a comment: at the start of a word.
fn word_start(bytes: &[u8], i: usize) -> bool {
    i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b'(')
}

fn scan(command: &str) -> Result<Scan> {
    let bytes = command.as_bytes();
    let mut quote = Quote::None;
    let mut cuts = Vec::new();
    let mut amp_redirect = false;
    // Heredocs opened on the current line, bodies starting after its newline.
    let mut pending: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Quote::Single => {
                if c == b'\'' {
                    quote = Quote::None;
                }
            }
            Quote::Double => match c {
                b'"' => quote = Quote::None,
                b'\\' => i += 1,
                _ => {}
            },
            Quote::None => match c {
                b'\'' => quote = Quote::Single,
                b'"' => quote = Quote::Double,
                b'\\' => i += 1,
                b'#' if word_start(bytes, i) => {
                    let end = command[i..].find('\n').map_or(bytes.len(), |n| i + n);
                    cuts.push((i, end));
                    i = end;
                    continue;
                }
                b'\n' => {
                    // Heredoc bodies follow the line that opened them, in order.
                    let mut pos = i + 1;
                    for (delim, strip_tabs) in pending.drain(..) {
                        loop {
                            if pos >= bytes.len() {
                                bail!("heredoc `<<{delim}` has no terminator line — the step would hang");
                            }
                            let end = command[pos..].find('\n').map_or(bytes.len(), |n| pos + n);
                            let line = &command[pos..end];
                            let line = if strip_tabs { line.trim_start_matches('\t') } else { line };
                            pos = (end + 1).min(bytes.len());
                            if line == delim {
                                break;
                            }
                        }
                    }
                    cuts.push((i, pos.max(i + 1)));
                    i = pos.max(i + 1);
                    continue;
                }
                b';' => cuts.push((i, i + 1)),
                b'&' => {
                    if bytes.get(i + 1) == Some(&b'&') {
                        cuts.push((i, i + 2));
                        i += 2;
                        continue;
                    }
                    // fd-dup redirects (`2>&1`, `>&file`, `&>log`) contain '&'
                    // but don't background: '&' touching '>' or a digit.
                    let prev_gt = i > 0 && bytes[i - 1] == b'>';
                    let next = bytes.get(i + 1).copied();
                    amp_redirect |= !prev_gt && next == Some(b'>');
                    let redirect = prev_gt || next == Some(b'>') || next.is_some_and(|c| c.is_ascii_digit());
                    if !redirect {
                        bail!("'&' backgrounds a command — set `background: true` on the call instead");
                    }
                }
                b'|' => {
                    // `||`, and bash's `|&` (pipe both streams), are two bytes.
                    let len = if matches!(bytes.get(i + 1), Some(b'|') | Some(b'&')) { 2 } else { 1 };
                    cuts.push((i, i + len));
                    i += len;
                    continue;
                }
                b'<' if bytes.get(i + 1) == Some(&b'<') => {
                    if bytes.get(i + 2) == Some(&b'<') {
                        i += 3; // here-string: no body
                        continue;
                    }
                    let mut j = i + 2;
                    let strip_tabs = bytes.get(j) == Some(&b'-');
                    if strip_tabs {
                        j += 1;
                    }
                    while matches!(bytes.get(j), Some(b' ') | Some(b'\t')) {
                        j += 1;
                    }
                    let begin = j;
                    while j < bytes.len() && !matches!(bytes[j], b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b'<' | b'>' | b'(' | b')') {
                        j += 1;
                    }
                    let delim: String = command[begin..j].chars().filter(|c| !matches!(c, '\'' | '"' | '\\')).collect();
                    if delim.is_empty() {
                        bail!("`<<` needs a heredoc delimiter word");
                    }
                    pending.push((delim, strip_tabs));
                    i = j;
                    continue;
                }
                _ => {}
            },
        }
        i += 1;
    }
    if quote != Quote::None {
        bail!("unbalanced quote — the step would wait forever for the closing quote");
    }
    if let Some((delim, _)) = pending.first() {
        bail!("heredoc `<<{delim}` has no body or terminator line — the step would hang");
    }
    Ok(Scan { cuts, amp_redirect })
}

/// Reject what would hang or misbehave in the step runner (see the module
/// doc), then let `shell` itself parse the step (`-n`: read, never execute)
/// so a syntax error comes back at once instead of as a timeout. Under plain
/// `sh` (dash) also reject `&>`: there `cmd &>log` parses as `cmd &` then
/// `>log` — backgrounded, and the step "succeeds" at once.
pub fn validate_step(command: &str, shell: &str, bash: bool) -> Result<()> {
    if scan(command)?.amp_redirect && !bash {
        bail!("`&>` is bash-only and this shell is sh, where it backgrounds the command — use `>file 2>&1`");
    }
    syntax_check(shell, command)
}

/// `<shell> -n -c <command>`: parse only. No shell to ask (an odd test
/// setup) is not an error — the run itself will report.
fn syntax_check(shell: &str, command: &str) -> Result<()> {
    use std::process::{Command, Stdio};
    let Ok(out) = Command::new(shell)
        .args(["-n", "-c", command])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    else {
        return Ok(());
    };
    let err = String::from_utf8_lossy(&out.stderr);
    // bash only warns (exit 0) on a heredoc cut off by end of input.
    if !out.status.success() || err.contains("here-document") {
        let detail: Vec<String> = err
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(3)
            .map(|l| l.split_once(": -c: ").map_or(l, |(_, rest)| rest).trim().to_string())
            .collect();
        bail!("shell syntax error, step not run: {}", if detail.is_empty() { "(no detail)".to_string() } else { detail.join(" / ") });
    }
    Ok(())
}

/// The simple commands of a legal step, in order: what's between operators,
/// with comments and heredoc bodies left out. A step that doesn't lex yields
/// itself, whole.
pub fn segments(command: &str) -> Vec<String> {
    let Ok(scan) = scan(command) else { return vec![command.trim().to_string()] };
    let mut out = Vec::new();
    let mut start = 0;
    for &(a, b) in scan.cuts.iter().chain(std::iter::once(&(command.len(), command.len()))) {
        if a >= start {
            let seg = command[start..a].trim();
            if !seg.is_empty() {
                out.push(seg.to_string());
            }
        }
        start = start.max(b);
    }
    out
}

/// Shell words that introduce a command rather than being one.
const PREFIX_WORDS: &[&str] = &["if", "then", "else", "elif", "do", "while", "until", "!", "time", "{", "("];

/// The program name of a segment, for allowlist matching: skips leading
/// `VAR=value` assignments, strips surrounding quotes.
pub fn program(segment: &str) -> Option<&str> {
    let mut rest = segment.trim_start().trim_start_matches(['(', '{']).trim_start();
    loop {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..end];
        if word.is_empty() {
            return None;
        }
        if is_env_assignment(word) || PREFIX_WORDS.contains(&word) {
            rest = rest[end..].trim_start().trim_start_matches(['(', '{']).trim_start();
            continue;
        }
        let word = word.trim_matches(|c| c == '"' || c == '\'');
        return if word.is_empty() { None } else { Some(word) };
    }
}

/// `FOO=bar` — a valid identifier followed by '='.
fn is_env_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash() -> String {
        crate::lsp::which("bash").unwrap_or_else(|| "bash".into())
    }

    fn ok(cmd: &str) {
        if let Err(e) = validate_step(cmd, &bash(), true) {
            panic!("{cmd:?} should be legal: {e:#}");
        }
    }

    fn err(cmd: &str) -> String {
        match validate_step(cmd, &bash(), true) {
            Ok(()) => panic!("{cmd:?} should be refused"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn ordinary_shell_is_legal() {
        ok("cargo build && cargo test");
        ok("cargo test; ls");
        ok("rg foo src || true");
        ok("echo $(whoami) `date`");
        ok("for f in src/*.rs; do wc -l \"$f\"; done");
        ok("cd crates/core && cargo check 2>&1 | tail -5");
        ok("python3 - <<'EOF'\nprint(\"don't panic\")\nEOF");
        ok("cat <<-END | wc -l\n\tone\n\ttwo\n\tEND\necho after");
        ok("echo hi # it's a comment");
        ok("echo a#b; echo $#");
        ok("grep -c x <<< \"$var\"");
        ok(r#"git commit -m "fix a; b && c""#);
    }

    #[test]
    fn what_would_hang_or_background_is_refused() {
        assert!(err("sleep 5 &").contains("background: true"));
        assert!(err("a & b").contains("background"));
        assert!(err("cat <<EOF\nno terminator").contains("terminator"));
        assert!(err("cat <<EOF").contains("terminator"));
        assert!(err("echo \"unbalanced").contains("unbalanced"));
        assert!(err("if true; then echo x").contains("syntax error"), "parse-only check catches the rest");
        assert!(err("echo (").contains("syntax error"));
    }

    #[test]
    fn fd_dup_redirects_are_not_backgrounding() {
        ok("cargo test 2>&1");
        ok("cmd >&2");
        ok("cmd &>out.log");
        ok("cmd 2>&1 | rg error");
        ok("cmd |& tee /tmp/log");
    }

    #[test]
    fn amp_redirect_is_rejected_only_when_the_shell_is_not_bash() {
        assert!(validate_step("cargo test &>>log.txt", "sh", false).is_err());
        assert!(validate_step("cargo test >log.txt 2>&1", "sh", false).is_ok());
        assert!(validate_step("echo '&>' fine", "sh", false).is_ok());
    }

    #[test]
    fn steps_split_into_simple_commands() {
        assert_eq!(segments("rg foo | head -5"), vec!["rg foo", "head -5"]);
        assert_eq!(segments("cd x && cargo test; ls || true"), vec!["cd x", "cargo test", "ls", "true"]);
        assert_eq!(segments("python3 - <<'EOF'\nimport os; os.remove('x')\nEOF\nls"), vec!["python3 - <<'EOF'", "ls"], "heredoc bodies are data");
        assert_eq!(segments("echo hi # rm -rf x"), vec!["echo hi"]);
        assert_eq!(segments("cargo test"), vec!["cargo test"]);
        assert_eq!(program("FOO=1 BAR=2 rg foo"), Some("rg"));
        assert_eq!(program(r#""weird cmd" --flag"#), Some("weird"));
        assert_eq!(program("if cargo test"), Some("cargo"));
        assert_eq!(program("(cd x"), Some("cd"));
        assert_eq!(program("head -5"), Some("head"));
    }
}
