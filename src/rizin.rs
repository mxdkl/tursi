//! Reverse-engineering backend: a persistent rizin session over the rzpipe
//! protocol (`rizin -0`, NUL-delimited command output). Analysis (`aa`/`aaa`)
//! runs once at open; the model then issues many commands cheaply — the same
//! session model as the debugger (§4.3), since RE is iterative.
//!
//! rizin only parses/analyzes the target statically; it does not run it, so
//! this is read-only w.r.t. the system (unlike `debug`).

use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::sandbox::{In, Out, Sandbox, Spawn};

pub struct Session {
    _child: crate::sandbox::Child,
    stdin: In,
    reader: BufReader<Out>,
    pub binary: String,
}

impl Session {
    /// Spawn rizin in rzpipe mode and analyze. `deep` runs `aaa` (thorough,
    /// slow on large statics); otherwise `aa` (basic functions).
    pub async fn open(sandbox: &Sandbox, binary: &Path, deep: bool) -> Result<Session> {
        let argv = ["rizin", "-0", "-e", "scr.color=0", "-e", "scr.interactive=false"]
            .into_iter()
            .map(String::from)
            .chain(std::iter::once(binary.display().to_string()))
            .collect();
        let mut child = sandbox.spawn(Spawn::piped(argv)).await.context("spawning rizin — is it installed?")?;
        let stdin = child.stdin.take().context("rizin stdin")?;
        let reader = BufReader::new(child.stdout.take().context("rizin stdout")?);
        let mut session = Session { _child: child, stdin, reader, binary: binary.display().to_string() };
        // The initial NUL is rizin's "ready" signal.
        read_until_nul(&mut session.reader, Duration::from_secs(15)).await?;
        session.cmd(if deep { "aaa" } else { "aa" }).await?;
        Ok(session)
    }

    /// One rizin command; output is NUL-delimited, ANSI-stripped.
    pub async fn cmd(&mut self, command: &str) -> Result<String> {
        self.stdin.write_all(command.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        let raw = read_until_nul(&mut self.reader, Duration::from_secs(180)).await?;
        Ok(strip_ansi(&raw))
    }

    pub async fn close(mut self) -> Result<()> {
        let _ = self.stdin.write_all(b"q\n").await;
        Ok(())
    }
}

/// rzpipe framing: read up to and including the NUL, then drop it.
async fn read_until_nul(reader: &mut BufReader<Out>, timeout: Duration) -> Result<String> {
    let mut buf = Vec::new();
    let n = tokio::time::timeout(timeout, reader.read_until(0u8, &mut buf))
        .await
        .map_err(|_| anyhow!("rizin command timed out"))??;
    if n == 0 {
        bail!("rizin closed its stdout");
    }
    if buf.last() == Some(&0) {
        buf.pop();
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Strip ANSI/CSI escapes — rizin emits progress control sequences (e.g. the
/// `\x1b[2K` line-clear from analysis) even with color disabled.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip the escape body up to and including its terminator letter.
            while let Some(n) = chars.next() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_analysis_control_sequences() {
        assert_eq!(strip_ansi("\u{1b}[2Kformat elf64"), "format elf64");
        assert_eq!(strip_ansi("plain text"), "plain text");
    }

    #[tokio::test]
    #[ignore = "live: needs rizin and the chal1 binary"]
    async fn analyzes_a_stripped_binary_and_disassembles() {
        let bin = "/home/player1/challenges/rev/chal1/chal.enc";
        if !Path::new(bin).exists() {
            eprintln!("skipping: no chal1");
            return;
        }
        let dir = crate::tools::testutil::tmp("rizin-live");
        let mut s = Session::open(&Sandbox::for_tests(&dir), Path::new(bin), false).await.expect("open");
        let functions = s.cmd("afl").await.expect("afl");
        eprintln!("functions (head):\n{}", functions.lines().take(3).collect::<Vec<_>>().join("\n"));
        assert!(functions.contains("0x004"), "afl should list function addresses");
        let disasm = s.cmd("pd 5 @ entry0").await.expect("pd");
        eprintln!("entry disasm:\n{disasm}");
        assert!(disasm.contains("endbr64") || disasm.contains("0x004"), "disasm at entry: {disasm}");
        s.close().await.unwrap();
    }
}
