//! debug tool backend: DAP over `gdb -i dap` (GDB ≥ 14; this host runs 17.2).
//! rr provides recording + reverse execution when installed (untested here —
//! no rr on this machine; the plumbing follows `rr replay -- -i dap`).
//!
//! Lockstep client like the LSP one (§4.3): requests read until their own
//! response, buffering out-of-order responses and absorbing events (stopped,
//! output, exited) into session state.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::BufReader;

use crate::sandbox::{In, Out, Sandbox, Spawn, Stream};

use crate::wire::{read_msg, write_msg};

pub struct Session {
    _child: crate::sandbox::Child,
    stdin: In,
    reader: BufReader<Out>,
    seq: i64,
    /// Out-of-order responses parked until their request asks.
    pending: HashMap<i64, Value>,
    stopped_thread: Option<i64>,
    last_stop: Option<(String, Option<String>)>,
    exited: Option<i32>,
    /// Accumulated output events; the debugger tool flushes to debug.log.
    output: String,
    /// DAP set*Breakpoints calls REPLACE the set — full lists are tracked.
    src_bps: HashMap<PathBuf, Vec<(u32, Option<String>)>>,
    fn_bps: Vec<(String, Option<String>)>,
    insn_bps: Vec<String>,
    /// Read once the UI badges rr replays (reverse steps available).
    #[allow(dead_code)]
    pub recording: bool,
}

pub enum StepKind {
    Continue,
    Step,
    Next,
    Finish,
    /// rr replays only.
    ReverseContinue,
    ReverseStep,
}

pub enum StopReason {
    Breakpoint,
    Watchpoint,
    Signal(String),
    Exited(i32),
    /// Hit the resume timeout; the harness paused the target (§4.3).
    Timeout,
}

/// The compact stop report (§4.3) — never the raw debugger transcript.
pub struct StopReport {
    pub reason: StopReason,
    pub thread: u32,
    pub location: String,
    pub frames: Vec<String>,
    pub watched: Vec<(String, String)>,
}

impl Session {
    /// `record: true` first runs `rr record`, then debugs the deterministic
    /// replay (reverse steps become available). `stop_at`: "main" (default),
    /// or "entry" — the entry point, which is the right choice for stripped
    /// binaries that have no `main` symbol (§4.3).
    pub async fn launch(
        sandbox: &Sandbox,
        program: &str,
        args: Vec<String>,
        record: bool,
        stop_at: Option<&str>,
    ) -> Result<Session> {
        if record {
            let mut argv = vec!["rr".to_string(), "record".to_string(), program.to_string()];
            argv.extend(args.iter().cloned());
            let mut rr = sandbox
                .spawn(Spawn { argv, env: vec![], cwd: None, stdin: Stream::Null, stdout: Stream::Null, stderr: Stream::Null })
                .await
                .context("rr not installed — recording needs it")?;
            match rr.wait().await {
                Some(0) => {}
                status => bail!("rr record failed with exit {status:?}"),
            }
        }
        let mut session = Self::spawn(sandbox, record).await?;
        session.initialize().await?;
        let mut launch = json!({ "program": program, "args": args });
        match stop_at {
            Some("entry") => launch["stopOnEntry"] = json!(true),
            _ => launch["stopAtBeginningOfMainSubprogram"] = json!(true),
        }
        let launch_seq = session.send("launch", launch).await?;
        session.finish_configuration(launch_seq).await?;
        session.wait_stop(Duration::from_secs(15)).await?;
        Ok(session)
    }

    /// Outside-PID attach — the caller (debugger tool) ALWAYS prompts (§4.3).
    pub async fn attach(sandbox: &Sandbox, pid: u32) -> Result<Session> {
        let mut session = Self::spawn(sandbox, false).await?;
        session.initialize().await?;
        let attach_seq = session.send("attach", json!({"pid": pid})).await?;
        session.finish_configuration(attach_seq).await?;
        session.wait_stop(Duration::from_secs(10)).await?;
        Ok(session)
    }

    /// Post-mortem. gdb's DAP has no core-file launch argument; this drives
    /// the console directly and is best-effort across gdb versions.
    pub async fn open_core(sandbox: &Sandbox, program: &str, core: &str) -> Result<Session> {
        let mut session = Self::spawn(sandbox, false).await?;
        session.initialize().await?;
        session.command(&format!("file {program}")).await?;
        session.command(&format!("core-file {core}")).await?;
        session.stopped_thread = Some(1);
        session.last_stop = Some(("core".to_string(), None));
        Ok(session)
    }

    async fn spawn(sandbox: &Sandbox, record: bool) -> Result<Session> {
        let argv: Vec<String> = if record { vec!["rr", "replay", "--", "-i", "dap"] } else { vec!["gdb", "-i", "dap"] }
            .into_iter()
            .map(String::from)
            .collect();
        let mut child = sandbox.spawn(Spawn::piped(argv)).await.context("spawning gdb — the debug tool needs GDB ≥ 14")?;
        Ok(Session {
            stdin: child.stdin.take().context("gdb stdin")?,
            reader: BufReader::new(child.stdout.take().context("gdb stdout")?),
            _child: child,
            seq: 0,
            pending: HashMap::new(),
            stopped_thread: None,
            last_stop: None,
            exited: None,
            output: String::new(),
            src_bps: HashMap::new(),
            fn_bps: Vec::new(),
            insn_bps: Vec::new(),
            recording: record,
        })
    }

    async fn initialize(&mut self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "clientID": "tursi", "adapterID": "gdb",
                "linesStartAt1": true, "columnsStartAt1": true, "pathFormat": "path",
            }),
            Duration::from_secs(10),
        )
        .await
        .context("DAP initialize")?;
        Ok(())
    }

    /// After launch/attach is sent: wait for the `initialized` event, send
    /// configurationDone, then collect the launch/attach response.
    async fn finish_configuration(&mut self, request_seq: i64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("gdb never sent the initialized event");
            }
            let msg = tokio::time::timeout(remaining, read_msg(&mut self.reader))
                .await
                .map_err(|_| anyhow!("gdb never sent the initialized event"))??;
            if msg.get("event").and_then(Value::as_str) == Some("initialized") {
                break;
            }
            self.dispatch(msg).await?;
        }
        self.request("configurationDone", json!({}), Duration::from_secs(10)).await?;
        self.recv_response(request_seq, Duration::from_secs(30)).await?;
        Ok(())
    }

    /// Location: function name, `file:line`, `*0xADDR`, or a bare `0xADDR`.
    /// A raw address (with or without `*`) becomes an instruction breakpoint —
    /// gdb's function/line breakpoints silently never fire on a raw address in
    /// a stripped binary. DAP replaces per-kind sets wholesale, so lists resend.
    pub async fn set_breakpoint(&mut self, location: &str, condition: Option<&str>) -> Result<String> {
        let condition = condition.map(str::to_string);
        let addr_form = location.strip_prefix('*').unwrap_or(location);
        let is_addr = addr_form.starts_with("0x")
            && addr_form.len() > 2
            && addr_form[2..].chars().all(|c| c.is_ascii_hexdigit());
        if is_addr {
            self.insn_bps.push(addr_form.to_string());
            let breakpoints: Vec<Value> = self
                .insn_bps
                .iter()
                .map(|a| json!({"instructionReference": a}))
                .collect();
            let body = self
                .request("setInstructionBreakpoints", json!({"breakpoints": breakpoints}), Duration::from_secs(10))
                .await?;
            let verified = body
                .pointer("/breakpoints")
                .and_then(Value::as_array)
                .and_then(|b| b.last())
                .and_then(|b| b.get("verified"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            return Ok(format!("instruction breakpoint at {addr_form} (verified: {verified})"));
        }
        if let Some((file, line)) = location.rsplit_once(':').filter(|(_, l)| l.parse::<u32>().is_ok()) {
            let line: u32 = line.parse().expect("checked above");
            let path = PathBuf::from(file);
            self.src_bps.entry(path.clone()).or_default().push((line, condition));
            let list = &self.src_bps[&path];
            let breakpoints: Vec<Value> = list
                .iter()
                .map(|(l, c)| match c {
                    Some(c) => json!({"line": l, "condition": c}),
                    None => json!({"line": l}),
                })
                .collect();
            let body = self
                .request(
                    "setBreakpoints",
                    json!({"source": {"path": file}, "breakpoints": breakpoints}),
                    Duration::from_secs(10),
                )
                .await?;
            let verified = body
                .pointer("/breakpoints")
                .and_then(Value::as_array)
                .and_then(|b| b.last())
                .and_then(|b| b.get("verified"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            return Ok(format!("breakpoint at {location} (verified: {verified})"));
        }
        self.fn_bps.push((location.to_string(), condition));
        let breakpoints: Vec<Value> = self
            .fn_bps
            .iter()
            .map(|(name, c)| match c {
                Some(c) => json!({"name": name, "condition": c}),
                None => json!({"name": name}),
            })
            .collect();
        let body = self
            .request("setFunctionBreakpoints", json!({"breakpoints": breakpoints}), Duration::from_secs(10))
            .await?;
        let verified = body
            .pointer("/breakpoints")
            .and_then(Value::as_array)
            .and_then(|b| b.last())
            .and_then(|b| b.get("verified"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(format!("function breakpoint on {location} (verified: {verified})"))
    }

    /// Via the gdb console — DAP data-breakpoint plumbing varies by version;
    /// `watch` is the reliable path and reports old/new values on hit.
    pub async fn set_watchpoint(&mut self, expr: &str) -> Result<String> {
        self.command(&format!("watch {expr}")).await
    }

    /// Resume until stop or timeout. On timeout the target gets paused so
    /// the session stays usable — a runaway continue can't hang the loop.
    pub async fn resume(&mut self, kind: StepKind, timeout: Duration) -> Result<StopReport> {
        if self.exited.is_some() {
            bail!("the process has exited — nothing to resume");
        }
        let thread = self.stopped_thread.unwrap_or(1);
        let command = match kind {
            StepKind::Continue => "continue",
            StepKind::Step => "stepIn",
            StepKind::Next => "next",
            StepKind::Finish => "stepOut",
            StepKind::ReverseContinue => "reverseContinue",
            StepKind::ReverseStep => "stepBack",
        };
        self.last_stop = None;
        self.request(command, json!({"threadId": thread}), Duration::from_secs(10)).await?;
        if self.wait_stop(timeout).await? {
            return self.stop_report().await;
        }
        // Timeout: pause, then report wherever it landed.
        let _ = self.request("pause", json!({"threadId": thread}), Duration::from_secs(5)).await;
        if self.wait_stop(Duration::from_secs(5)).await? {
            let mut report = self.stop_report().await?;
            report.reason = StopReason::Timeout;
            return Ok(report);
        }
        bail!("target did not stop after pause — session is wedged; quit it");
    }

    pub async fn stack(&mut self, max_frames: usize) -> Result<String> {
        let frames = self.frames(self.stopped_thread.unwrap_or(1), max_frames).await?;
        Ok(frames.join("\n"))
    }

    /// Every non-register scope merged — gdb splits arguments out of
    /// "Locals", and callers want both.
    pub async fn locals(&mut self, frame: Option<u32>) -> Result<String> {
        let frame_id = self.frame_id(frame.unwrap_or(0)).await?;
        let scopes = self
            .request("scopes", json!({"frameId": frame_id}), Duration::from_secs(10))
            .await?;
        let scope_refs: Vec<i64> = scopes
            .pointer("/scopes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|s| s.get("name").and_then(Value::as_str) != Some("Registers"))
            .filter_map(|s| s.get("variablesReference").and_then(Value::as_i64))
            .collect();
        let mut out = String::new();
        let mut count = 0usize;
        for scope_ref in scope_refs {
            let vars = self
                .request("variables", json!({"variablesReference": scope_ref}), Duration::from_secs(10))
                .await?;
            for var in vars.pointer("/variables").and_then(Value::as_array).into_iter().flatten() {
                if count >= 40 {
                    break;
                }
                let name = var.get("name").and_then(Value::as_str).unwrap_or("?");
                let value = var.get("value").and_then(Value::as_str).unwrap_or("?");
                out.push_str(&format!("{name} = {}\n", value.chars().take(120).collect::<String>()));
                count += 1;
            }
        }
        Ok(if out.is_empty() { "no locals".to_string() } else { out.trim_end().to_string() })
    }

    pub async fn eval(&mut self, expr: &str, frame: Option<u32>) -> Result<String> {
        let mut args = json!({"expression": expr, "context": "watch"});
        if let Ok(frame_id) = self.frame_id(frame.unwrap_or(0)).await {
            args["frameId"] = json!(frame_id);
        }
        let body = self.request("evaluate", args, Duration::from_secs(10)).await?;
        Ok(body.get("result").and_then(Value::as_str).unwrap_or("").to_string())
    }

    pub async fn registers(&mut self) -> Result<String> {
        let all = self.command("info registers").await?;
        Ok(all.lines().take(24).collect::<Vec<_>>().join("\n"))
    }

    /// Bounded hex+ASCII window via DAP readMemory (base64 on the wire).
    pub async fn read_memory(&mut self, addr: &str, len: usize) -> Result<String> {
        let len = len.min(512);
        let body = self
            .request(
                "readMemory",
                json!({"memoryReference": addr, "count": len}),
                Duration::from_secs(10),
            )
            .await?;
        let data = body.get("data").and_then(Value::as_str).unwrap_or("");
        let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
        let base = body
            .get("address")
            .and_then(Value::as_str)
            .and_then(|a| u64::from_str_radix(a.trim_start_matches("0x"), 16).ok())
            .unwrap_or(0);
        Ok(hexdump(base, &bytes))
    }

    /// DAP `disassemble` request (address + count) — works on raw addresses in
    /// stripped binaries, where gdb's `disassemble <addr>` fails with "no
    /// function contains specified address". Falls back to `x/Ni`.
    pub async fn disassemble(&mut self, at: &str, count: usize) -> Result<String> {
        let count = count.max(4);
        let body = self
            .request(
                "disassemble",
                json!({"memoryReference": at, "instructionCount": count, "resolveSymbols": false}),
                Duration::from_secs(10),
            )
            .await;
        if let Ok(body) = body {
            let mut out = String::new();
            for insn in body.pointer("/instructions").and_then(Value::as_array).into_iter().flatten() {
                let addr = insn.get("address").and_then(Value::as_str).unwrap_or("?");
                let text = insn.get("instruction").and_then(Value::as_str).unwrap_or("");
                out.push_str(&format!("{addr}: {text}\n"));
            }
            if !out.trim().is_empty() {
                return Ok(out.trim_end().to_string());
            }
        }
        // Fallback: the console examine-instructions form.
        self.command(&format!("x/{count}i {at}")).await
    }

    /// Dump a raw memory range to a file (gdb `dump binary memory`) so it can
    /// be examined offline — e.g. JIT'd/self-modifying code that isn't in the
    /// on-disk binary. The file lands on the host filesystem.
    pub async fn dump_memory(&mut self, addr: &str, len: usize, path: &str) -> Result<String> {
        self.command(&format!("dump binary memory {path} {addr} ({addr}+{len})")).await?;
        match std::fs::metadata(path) {
            Ok(m) => Ok(format!("dumped {} bytes from {addr} to {path}", m.len())),
            Err(_) => bail!("dump produced no file at {path} — check the address range"),
        }
    }

    /// Raw gdb console escape hatch (repl-context evaluate).
    pub async fn command(&mut self, text: &str) -> Result<String> {
        let body = self
            .request("evaluate", json!({"expression": text, "context": "repl"}), Duration::from_secs(30))
            .await?;
        Ok(body.get("result").and_then(Value::as_str).unwrap_or("").trim_end().to_string())
    }

    /// Everything the target printed since the last flush (→ debug.log).
    pub fn take_output(&mut self) -> String {
        std::mem::take(&mut self.output)
    }

    pub async fn quit(mut self) -> Result<()> {
        let _ = self
            .request("disconnect", json!({"terminateDebuggee": true}), Duration::from_secs(5))
            .await;
        Ok(())
    }

    // ── plumbing ──────────────────────────────────────────────────────────

    async fn send(&mut self, command: &str, arguments: Value) -> Result<i64> {
        self.seq += 1;
        let seq = self.seq;
        write_msg(
            &mut self.stdin,
            &json!({"seq": seq, "type": "request", "command": command, "arguments": arguments}),
        )
        .await?;
        Ok(seq)
    }

    async fn request(&mut self, command: &str, arguments: Value, timeout: Duration) -> Result<Value> {
        let seq = self.send(command, arguments).await?;
        self.recv_response(seq, timeout).await.with_context(|| format!("DAP {command}"))
    }

    async fn recv_response(&mut self, seq: i64, timeout: Duration) -> Result<Value> {
        if let Some(msg) = self.pending.remove(&seq) {
            return check(msg);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("gdb did not respond in time");
            }
            let msg = tokio::time::timeout(remaining, read_msg(&mut self.reader))
                .await
                .map_err(|_| anyhow!("gdb did not respond in time"))??;
            if msg.get("type").and_then(Value::as_str) == Some("response") {
                let request_seq = msg.get("request_seq").and_then(Value::as_i64).unwrap_or(-1);
                if request_seq == seq {
                    return check(msg);
                }
                self.pending.insert(request_seq, msg);
                continue;
            }
            self.dispatch(msg).await?;
        }
    }

    /// True: stopped or exited. False: timed out (target still running).
    async fn wait_stop(&mut self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.last_stop.is_some() || self.exited.is_some() {
                return Ok(true);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            match tokio::time::timeout(remaining, read_msg(&mut self.reader)).await {
                Err(_) => return Ok(false),
                Ok(msg) => self.dispatch(msg?).await?,
            }
        }
    }

    /// Events update state; stray responses park; server requests get refused.
    async fn dispatch(&mut self, msg: Value) -> Result<()> {
        match msg.get("type").and_then(Value::as_str) {
            Some("event") => {
                match msg.get("event").and_then(Value::as_str) {
                    Some("stopped") => {
                        self.stopped_thread = msg.pointer("/body/threadId").and_then(Value::as_i64);
                        let reason = msg
                            .pointer("/body/reason")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        let description = msg
                            .pointer("/body/description")
                            .or_else(|| msg.pointer("/body/text"))
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        self.last_stop = Some((reason, description));
                    }
                    Some("exited") => {
                        self.exited =
                            Some(msg.pointer("/body/exitCode").and_then(Value::as_i64).unwrap_or(0) as i32);
                    }
                    Some("terminated") => {
                        self.exited.get_or_insert(0);
                    }
                    Some("output") => {
                        if let Some(text) = msg.pointer("/body/output").and_then(Value::as_str) {
                            self.output.push_str(text);
                        }
                    }
                    _ => {}
                }
            }
            Some("request") => {
                // runInTerminal &c — refuse; we run everything headless.
                let seq = msg.get("seq").cloned().unwrap_or(Value::Null);
                let command = msg.get("command").cloned().unwrap_or(Value::Null);
                self.seq += 1;
                write_msg(
                    &mut self.stdin,
                    &json!({"seq": self.seq, "type": "response", "request_seq": seq,
                            "command": command, "success": false, "message": "unsupported"}),
                )
                .await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn stop_report(&mut self) -> Result<StopReport> {
        if let Some(code) = self.exited {
            return Ok(StopReport {
                reason: StopReason::Exited(code),
                thread: 0,
                location: "process exited".to_string(),
                frames: vec![],
                watched: vec![],
            });
        }
        let (reason_text, description) =
            self.last_stop.clone().unwrap_or(("unknown".to_string(), None));
        let thread = self.stopped_thread.unwrap_or(1);
        let frames = self.frames(thread, 5).await.unwrap_or_default();
        let location = frames.first().cloned().unwrap_or_else(|| "?".to_string());
        let is_watch = reason_text.contains("watch") || reason_text.contains("data breakpoint");
        let reason = if is_watch {
            StopReason::Watchpoint
        } else if reason_text.contains("breakpoint") || reason_text == "function breakpoint" {
            StopReason::Breakpoint
        } else if reason_text == "signal" || reason_text == "exception" {
            StopReason::Signal(description.clone().unwrap_or_else(|| reason_text.clone()))
        } else {
            StopReason::Signal(reason_text)
        };
        let watched = match (is_watch, description) {
            (true, Some(d)) => vec![("watch".to_string(), d)],
            _ => vec![],
        };
        Ok(StopReport { reason, thread: thread as u32, location, frames, watched })
    }

    async fn frames(&mut self, thread: i64, levels: usize) -> Result<Vec<String>> {
        let body = self
            .request(
                "stackTrace",
                json!({"threadId": thread, "startFrame": 0, "levels": levels}),
                Duration::from_secs(10),
            )
            .await?;
        Ok(body
            .pointer("/stackFrames")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, f)| {
                let name = f.get("name").and_then(Value::as_str).unwrap_or("?");
                let source = f
                    .pointer("/source/path")
                    .and_then(Value::as_str)
                    .and_then(|p| std::path::Path::new(p).file_name())
                    .and_then(|n| n.to_str());
                let line = f.get("line").and_then(Value::as_u64).unwrap_or(0);
                let addr = f.get("instructionPointerReference").and_then(Value::as_str);
                // Stripped/no-source frames: show the address, not "??? at ?:0".
                match (source, addr) {
                    (Some(file), _) if line > 0 => format!("#{i} {name} at {file}:{line}"),
                    (_, Some(addr)) if name == "??" || name == "???" || name == "?" => {
                        format!("#{i} {addr}")
                    }
                    (_, Some(addr)) => format!("#{i} {addr} {name}"),
                    _ => format!("#{i} {name}"),
                }
            })
            .collect())
    }

    async fn frame_id(&mut self, frame: u32) -> Result<i64> {
        let thread = self.stopped_thread.unwrap_or(1);
        let body = self
            .request(
                "stackTrace",
                json!({"threadId": thread, "startFrame": frame, "levels": 1}),
                Duration::from_secs(10),
            )
            .await?;
        body.pointer("/stackFrames/0/id")
            .and_then(Value::as_i64)
            .context("no such frame")
    }
}

fn check(msg: Value) -> Result<Value> {
    if msg.get("success").and_then(Value::as_bool) == Some(true) {
        Ok(msg.get("body").cloned().unwrap_or(Value::Null))
    } else {
        bail!(
            "{}",
            msg.get("message")
                .and_then(Value::as_str)
                .unwrap_or("request failed")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "repro: needs the stripped chal1 binary"]
    async fn launch_stops_on_a_stripped_binary_without_main() {
        let bin = "/home/player1/challenges/rev/chal1/chal.enc";
        if !std::path::Path::new(bin).exists() {
            eprintln!("skipping: no chal1");
            return;
        }
        let dir = crate::tools::testutil::tmp("dap-live");
        let mut s = Session::launch(
            &Sandbox::for_tests(&dir),
            bin,
            vec!["/home/player1/challenges/rev/chal1/chal.key".into()],
            false,
            Some("entry"),
        )
        .await
        .expect("launch");
        let stack = s.stack(3).await.expect("stack");
        eprintln!("stopped at:\n{stack}");
        // The bug was "??? at ?:0"; a stripped frame must now show an address.
        assert!(stack.contains("0x"), "stripped frame should show an address, got:\n{stack}");

        // disassemble must work on a raw address (was: "no function contains").
        let entry = "0x403360";
        let dis = s.disassemble(entry, 5).await.expect("disassemble");
        eprintln!("disasm:\n{dis}");
        assert!(dis.contains("0x40"), "disassembly should show addresses:\n{dis}");

        // A bare hex address must set a real (verified) instruction breakpoint.
        let bp = s.set_breakpoint(entry, None).await.expect("break");
        eprintln!("break: {bp}");
        assert!(bp.contains("verified: true"), "bare 0xADDR must verify: {bp}");

        // dump_memory writes a real file.
        let path = format!("/tmp/tursi-dump-{}.bin", std::process::id());
        let dumped = s.dump_memory(entry, 64, &path).await.expect("dump");
        eprintln!("{dumped}");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 64);
        let _ = std::fs::remove_file(&path);

        s.quit().await.unwrap();
    }
}

fn hexdump(base: u64, bytes: &[u8]) -> String {
    let mut out = String::new();
    for (i, row) in bytes.chunks(16).enumerate() {
        let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = row
            .iter()
            .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
            .collect();
        out.push_str(&format!("0x{:012x}  {:<47}  |{ascii}|\n", base + (i * 16) as u64, hex.join(" ")));
    }
    out.trim_end().to_string()
}
