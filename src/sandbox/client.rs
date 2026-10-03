//! The harness's handle on a session sandbox: starts `tursi __sandbox`, sends
//! spawn/kill requests, and routes the init's events back to waiting callers
//! from a reader thread (seqpacket sockets have no tokio type; sends are
//! short, so they block inline).

use anyhow::{Context, Result, anyhow, bail};
use nix::libc;
use nix::sys::socket::{AddressFamily, SockFlag, SockType, setsockopt, socketpair, sockopt};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

use super::protocol::{self, Event, INIT_FD, Request, Spec};

/// How a spawn ended: `code` is None when a signal killed it.
#[derive(Debug, Clone, Copy)]
pub struct Exit {
    pub code: Option<i32>,
}

/// A running spawn: `exited` resolves when it ends (or errors if the sandbox
/// dies).
pub struct Proc {
    pub id: u64,
    pub exited: oneshot::Receiver<Exit>,
}

#[derive(Default)]
struct Waiters {
    alive: bool,
    spawned: HashMap<u64, oneshot::Sender<Result<(), String>>>,
    exited: HashMap<u64, oneshot::Sender<Exit>>,
}

pub struct Namespaced {
    sock: OwnedFd,
    send_lock: Mutex<()>,
    waiters: Arc<Mutex<Waiters>>,
    next_id: AtomicU64,
    init: Mutex<std::process::Child>,
}

/// Setup must finish within this; mounting is fast, so this only bounds a hang.
const SETUP_TIMEOUT: Duration = Duration::from_secs(20);

impl Namespaced {
    /// Start the init and wait for it to report the view built. An error means
    /// no sandbox (namespaces unavailable, a mount refused, …) — never a
    /// half-built one.
    pub fn start(spec: &Spec, log: &Path) -> Result<Namespaced> {
        let (ours, theirs) = socketpair(AddressFamily::Unix, SockType::SeqPacket, None, SockFlag::SOCK_CLOEXEC)?;
        let exe = std::env::current_exe().context("locating the tursi binary")?;
        let theirs_fd = theirs.as_raw_fd();
        let mut cmd = std::process::Command::new(exe);
        // The init needs nothing from our environment; keep keys out of it.
        cmd.arg("__sandbox")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(log)?);
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                let ok = if theirs_fd == INIT_FD {
                    libc::fcntl(INIT_FD, libc::F_SETFD, 0) == 0
                } else {
                    libc::dup2(theirs_fd, INIT_FD) == INIT_FD
                };
                if ok { Ok(()) } else { Err(std::io::Error::last_os_error()) }
            });
        }
        let mut init = cmd.spawn().context("starting the sandbox init")?;
        drop(theirs);

        let setup = (|| -> Result<()> {
            protocol::send(ours.as_raw_fd(), &Request::Setup(spec.clone()), &[])?;
            setsockopt(&ours, sockopt::ReceiveTimeout, &nix::sys::time::TimeVal::new(SETUP_TIMEOUT.as_secs() as _, 0))?;
            let reply = protocol::recv::<Event>(ours.as_raw_fd());
            setsockopt(&ours, sockopt::ReceiveTimeout, &nix::sys::time::TimeVal::new(0, 0))?;
            match reply? {
                Some((Event::Ready, _)) => Ok(()),
                Some((Event::Fatal(message), _)) => bail!("{message}"),
                Some((other, _)) => bail!("unexpected first event from the sandbox: {other:?}"),
                None => bail!("the sandbox init exited during setup (see {})", log.display()),
            }
        })();
        if let Err(e) = setup {
            let _ = init.kill();
            let _ = init.wait();
            return Err(e);
        }

        let waiters = Arc::new(Mutex::new(Waiters { alive: true, ..Default::default() }));
        let reader_sock = ours.try_clone()?;
        let reader_waiters = waiters.clone();
        std::thread::Builder::new()
            .name("sandbox-events".into())
            .spawn(move || read_events(reader_sock, reader_waiters))?;
        Ok(Namespaced {
            sock: ours,
            send_lock: Mutex::new(()),
            waiters,
            next_id: AtomicU64::new(1),
            init: Mutex::new(init),
        })
    }

    /// Run `argv` inside the sandbox with the given stdio. Returns once it has
    /// exec'd; our copies of the descriptors are closed here.
    pub async fn spawn(
        &self,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: PathBuf,
        stdio: [OwnedFd; 3],
    ) -> Result<Proc> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (spawned_tx, spawned_rx) = oneshot::channel();
        let (exited_tx, exited_rx) = oneshot::channel();
        {
            let mut w = self.waiters.lock().unwrap();
            if !w.alive {
                bail!("the sandbox is no longer running");
            }
            w.spawned.insert(id, spawned_tx);
            w.exited.insert(id, exited_tx);
        }
        let raw: Vec<_> = stdio.iter().map(|fd| fd.as_raw_fd()).collect();
        {
            let _guard = self.send_lock.lock().unwrap();
            protocol::send(self.sock.as_raw_fd(), &Request::Spawn { id, argv, env, cwd }, &raw)?;
        }
        drop(stdio);
        match spawned_rx.await {
            Ok(Ok(())) => Ok(Proc { id, exited: exited_rx }),
            Ok(Err(error)) => Err(anyhow!("{error}")),
            Err(_) => Err(anyhow!("the sandbox stopped while starting the command")),
        }
    }

    /// SIGKILL the spawn's process group. Its `exited` resolves when it's gone.
    pub fn kill(&self, id: u64) {
        let _guard = self.send_lock.lock().unwrap();
        let _ = protocol::send(self.sock.as_raw_fd(), &Request::Kill { id }, &[]);
    }

    /// Stop the init — and with it every process inside. Idempotent.
    pub fn shutdown(&self) {
        let mut init = self.init.lock().unwrap();
        let _ = init.kill();
        let _ = init.wait();
    }
}

impl Drop for Namespaced {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn read_events(sock: OwnedFd, waiters: Arc<Mutex<Waiters>>) {
    loop {
        let event = match protocol::recv::<Event>(sock.as_raw_fd()) {
            Ok(Some((event, _))) => event,
            Ok(None) | Err(_) => break,
        };
        let mut w = waiters.lock().unwrap();
        match event {
            Event::Spawned { id } => {
                if let Some(tx) = w.spawned.remove(&id) {
                    let _ = tx.send(Ok(()));
                }
            }
            Event::SpawnFailed { id, error } => {
                w.exited.remove(&id);
                if let Some(tx) = w.spawned.remove(&id) {
                    let _ = tx.send(Err(error));
                }
            }
            Event::Exited { id, code, .. } => {
                if let Some(tx) = w.exited.remove(&id) {
                    let _ = tx.send(Exit { code });
                }
            }
            Event::Ready | Event::Fatal(_) => {}
        }
    }
    // The init is gone: wake every waiter with an error (dropped senders).
    let mut w = waiters.lock().unwrap();
    w.alive = false;
    w.spawned.clear();
    w.exited.clear();
}
