//! Harness ⇄ sandbox-init messages (PERMISSIONS.md §2.1) over a SOCK_SEQPACKET
//! socketpair: one JSON message per packet, so framing is the kernel's. A spawn
//! carries the child's stdin/stdout/stderr as SCM_RIGHTS descriptors.

use anyhow::{Result, bail};
use nix::errno::Errno;
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

/// Where the init finds its end of the socket.
pub const INIT_FD: RawFd = 3;
/// Spawn requests carry environments; nothing legitimate comes close.
const MAX_PACKET: usize = 1 << 20;

/// The filesystem view and starting point, sent once before anything else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    /// An empty host directory the init mounts the new root's tmpfs on.
    pub root: PathBuf,
    /// Applied in order; `mounts::plan` sorts parents before children.
    pub mounts: Vec<Mount>,
    /// Paths covered with an empty, non-executable file — tursi's own binary
    /// (PERMISSIONS.md §2.4).
    pub masks: Vec<PathBuf>,
    pub cwd: PathBuf,
    /// The network gate (PERMISSIONS.md §4): the init forwards loopback
    /// `port` to the harness's proxy at `socket` (a path inside the view).
    pub proxy: Option<ProxySpec>,
}

/// A per-spawn read-only project: everything under `project` is read-only to
/// the process except the `writable` directories (build output), which must
/// exist and lie inside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadOnly {
    pub project: PathBuf,
    pub writable: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxySpec {
    pub socket: PathBuf,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mount {
    /// The path inside the sandbox.
    pub at: PathBuf,
    pub what: What,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum What {
    /// A host path at `at`; read-only unless `writable`.
    Bind { src: PathBuf, writable: bool },
    /// An empty, writable, throwaway directory.
    Tmpfs { mode: u32 },
    Symlink { target: PathBuf },
    /// A fresh procfs for the sandbox's pid namespace.
    Proc,
    /// A minimal /dev: null zero full random urandom tty, pts, shm.
    Dev,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Setup(Spec),
    /// Exactly three descriptors ride along: stdin, stdout, stderr.
    Spawn {
        id: u64,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: PathBuf,
        /// Run this one with the project read-only (the lead, §5.7).
        #[serde(default)]
        read_only: Option<ReadOnly>,
    },
    /// SIGKILL the spawn's whole process group.
    Kill { id: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Event {
    Ready,
    /// Setup failed; the init exits after sending this.
    Fatal(String),
    Spawned { id: u64 },
    SpawnFailed { id: u64, error: String },
    Exited { id: u64, code: Option<i32>, signal: Option<i32> },
}

pub fn send<T: Serialize>(sock: RawFd, message: &T, fds: &[RawFd]) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    if bytes.len() > MAX_PACKET {
        bail!("sandbox message too large ({} bytes)", bytes.len());
    }
    let iov = [IoSlice::new(&bytes)];
    let rights = [ControlMessage::ScmRights(fds)];
    let cmsgs: &[ControlMessage] = if fds.is_empty() { &[] } else { &rights };
    loop {
        match sendmsg::<()>(sock, &iov, cmsgs, MsgFlags::MSG_NOSIGNAL, None) {
            Ok(_) => return Ok(()),
            Err(Errno::EINTR) => continue,
            Err(e) => bail!("sandbox socket send: {e}"),
        }
    }
}

/// One message and its descriptors; None once the peer has closed.
pub fn recv<T: DeserializeOwned>(sock: RawFd) -> Result<Option<(T, Vec<OwnedFd>)>> {
    let mut buf = vec![0u8; MAX_PACKET];
    let mut cmsg = nix::cmsg_space!([RawFd; 3]);
    let (len, fds) = loop {
        let mut iov = [IoSliceMut::new(&mut buf)];
        match recvmsg::<()>(sock, &mut iov, Some(&mut cmsg), MsgFlags::MSG_CMSG_CLOEXEC) {
            Err(Errno::EINTR) => continue,
            Err(e) => bail!("sandbox socket recv: {e}"),
            Ok(msg) => {
                let mut fds = Vec::new();
                for c in msg.cmsgs()? {
                    if let ControlMessageOwned::ScmRights(raw) = c {
                        // SAFETY: the kernel just installed these descriptors for us.
                        fds.extend(raw.into_iter().map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }));
                    }
                }
                if msg.flags.intersects(MsgFlags::MSG_TRUNC | MsgFlags::MSG_CTRUNC) {
                    bail!("sandbox message truncated");
                }
                break (msg.bytes, fds);
            }
        }
    };
    // SEQPACKET reads 0 bytes only at end of stream: we never send empty packets.
    if len == 0 && fds.is_empty() {
        return Ok(None);
    }
    Ok(Some((serde_json::from_slice(&buf[..len])?, fds)))
}
