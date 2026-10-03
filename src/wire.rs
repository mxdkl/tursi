//! Content-Length framed JSON over child stdio — the base protocol shared by
//! LSP (§8) and DAP (§4.3). Generic over the streams: children come from the
//! sandbox (pipes) or, unsandboxed, from tokio's process API.

use anyhow::{Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub async fn write_msg<W: AsyncWrite + Unpin>(stdin: &mut W, msg: &Value) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    stdin.write_all(&body).await?;
    stdin.flush().await?;
    Ok(())
}

pub async fn read_msg<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<Value> {
    let mut length = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            bail!("server closed its stdout — see its .tursi/*.log");
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(v) = trimmed.strip_prefix("Content-Length:") {
            length = v.trim().parse()?;
        }
    }
    let mut buf = vec![0u8; length];
    reader.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}
