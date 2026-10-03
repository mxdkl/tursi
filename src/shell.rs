//! POSIX-ish shell lexing for the chaining ban and pipe segmentation (§4.1).
//! A real tokenizer, not a substring scan: `git commit -m "a; b"` is legal.
//!
//! Command substitution (`$(…)`, backticks) is rejected outside single quotes:
//! it would smuggle arbitrary programs past per-segment permission checks.

use anyhow::{Result, bail};

#[derive(Clone, Copy, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

/// What one scan of a step found.
struct Scan {
    /// Byte positions of top-level pipes.
    pipes: Vec<usize>,
    /// A top-level `&>` / `&>>`: bash's both-streams redirect.
    amp_redirect: bool,
}

/// One scan shared by validation and splitting: the step's top-level
/// structure, or the error that makes the step illegal.
fn scan(command: &str) -> Result<Scan> {
    let bytes = command.as_bytes();
    let mut quote = Quote::None;
    let mut pipes = Vec::new();
    let mut amp_redirect = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match quote {
            Quote::Single => {
                if c == '\'' {
                    quote = Quote::None;
                }
            }
            Quote::Double => match c {
                '"' => quote = Quote::None,
                '\\' => i += 1,
                '`' => bail!("command substitution is not allowed — run it as its own step"),
                '$' if bytes.get(i + 1) == Some(&b'(') => {
                    bail!("command substitution is not allowed — run it as its own step")
                }
                _ => {}
            },
            Quote::None => match c {
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '\\' => i += 1,
                '\n' => bail!("newlines are not allowed — use separate steps"),
                ';' => bail!("';' chains commands — use separate steps"),
                '&' => {
                    if bytes.get(i + 1) == Some(&b'&') {
                        bail!("'&&' chains commands — use separate steps");
                    }
                    // fd-dup redirects (`2>&1`, `>&file`, `&>log`) contain '&'
                    // but don't background: '&' touching '>' or a digit.
                    let prev_gt = i > 0 && bytes[i - 1] == b'>';
                    let next = bytes.get(i + 1).copied();
                    amp_redirect |= !prev_gt && next == Some(b'>');
                    let redirect = prev_gt
                        || next == Some(b'>')
                        || next.is_some_and(|c| c.is_ascii_digit());
                    if !redirect {
                        bail!("'&' backgrounds a command — not allowed in the sandbox");
                    }
                }
                '|' => {
                    if bytes.get(i + 1) == Some(&b'|') {
                        bail!("'||' chains commands — use separate steps");
                    }
                    pipes.push(i);
                }
                '`' => bail!("command substitution is not allowed — run it as its own step"),
                '$' if bytes.get(i + 1) == Some(&b'(') => {
                    bail!("command substitution is not allowed — run it as its own step")
                }
                _ => {}
            },
        }
        i += 1;
    }
    if quote != Quote::None {
        bail!("unbalanced quote");
    }
    Ok(Scan { pipes, amp_redirect })
}

/// Reject `&&`, `;`, `||`, `&`, newlines, and command substitution outside
/// quotes, with an error telling the model to use steps instead. Under plain
/// `sh` (dash on Debian/Ubuntu) also reject `&>`: there `cmd &>log` parses as
/// `cmd &` then `>log` — backgrounded, and the step "succeeds" at once.
pub fn validate_step(command: &str, bash: bool) -> Result<()> {
    if scan(command)?.amp_redirect && !bash {
        bail!("`&>` is bash-only and this shell is sh, where it backgrounds the command — use `>file 2>&1`");
    }
    Ok(())
}

/// Split a legal step into pipe segments for per-segment permission checks.
/// All segments must pass for the step to run (§4.1).
pub fn pipe_segments(command: &str) -> Result<Vec<String>> {
    let pipes = scan(command)?.pipes;
    let mut segments = Vec::new();
    let mut start = 0;
    for &pos in pipes.iter().chain(std::iter::once(&command.len())) {
        let seg = command[start..pos].trim();
        if seg.is_empty() {
            bail!("empty pipeline segment");
        }
        segments.push(seg.to_string());
        start = (pos + 1).min(command.len());
    }
    Ok(segments)
}

/// The program name of a segment, for allowlist matching: skips leading
/// `VAR=value` assignments, strips surrounding quotes.
pub fn program(segment: &str) -> Option<&str> {
    let mut rest = segment.trim_start();
    loop {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..end];
        if word.is_empty() {
            return None;
        }
        if is_env_assignment(word) {
            rest = rest[end..].trim_start();
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

    #[test]
    fn chaining_operators_inside_quotes_are_legal() {
        validate_step(r#"git commit -m "fix a; b && c""#, true).unwrap();
        validate_step("echo 'a && b; c | d'", true).unwrap();
        validate_step(r#"rg "unsafe \{" src"#, true).unwrap();
    }

    #[test]
    fn fd_dup_redirects_are_not_backgrounding() {
        validate_step("cargo test 2>&1", true).unwrap();
        validate_step("cmd >&2", true).unwrap();
        validate_step("cmd &>out.log", true).unwrap();
        validate_step("cmd 2>&1 | rg error", true).unwrap();
    }

    #[test]
    fn amp_redirect_is_rejected_only_when_the_shell_is_not_bash() {
        assert!(validate_step("cargo test &>log.txt", true).is_ok());
        assert!(validate_step("cargo test &>>log.txt", false).is_err());
        assert!(validate_step("cargo test >log.txt 2>&1", false).is_ok());
        assert!(validate_step("echo '&>' fine", false).is_ok());
    }

    #[test]
    fn bare_semicolon_and_ampersand_are_rejected() {
        assert!(validate_step("cargo test; ls", true).is_err());
        assert!(validate_step("cargo build && cargo test", true).is_err());
        assert!(validate_step("a || b", true).is_err());
        assert!(validate_step("sleep 5 &", true).is_err());
        assert!(validate_step("a & b", true).is_err());
        assert!(validate_step("echo $(whoami)", true).is_err());
        assert!(validate_step("echo `whoami`", true).is_err());
        assert!(validate_step("echo \"$(whoami)\"", true).is_err());
        assert!(validate_step("echo 'fine: $(inert)'", true).is_ok());
        assert!(validate_step("echo \"unbalanced", true).is_err());
    }

    #[test]
    fn pipeline_splits_into_permission_checkable_segments() {
        let segs = pipe_segments("rg foo | head -5").unwrap();
        assert_eq!(segs, vec!["rg foo", "head -5"]);
        assert_eq!(pipe_segments("cargo test").unwrap(), vec!["cargo test"]);
        assert!(pipe_segments("rg foo | ").is_err());
        assert_eq!(program("FOO=1 BAR=2 rg foo"), Some("rg"));
        assert_eq!(program(r#""weird cmd" --flag"#), Some("weird"));
        assert_eq!(program("head -5"), Some("head"));
    }
}
