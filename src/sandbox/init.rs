//! The sandbox's PID 1 (PERMISSIONS.md §2): `tursi __sandbox`, reached before
//! `main` through an `.init_array` entry — so the very same binary works as the
//! init whether it is tursi or a `cargo test` harness, and the namespace calls
//! run while the process is still single-threaded (unshare(CLONE_NEWUSER)
//! refuses a threaded caller).
//!
//! Process shape:
//!
//! ```text
//! harness ── tursi __sandbox (outer user ns; waits)
//!                └── init, PID 1 (outer user ns; new mount/pid/ipc/uts/net ns; non-dumpable)
//!                      ├── proxy forwarder: loopback :3128 → the harness's Unix socket (§4)
//!                      └── each spawn: joins the workload user ns (nested), CAP_SYS_PTRACE only
//! ```
//!
//! The workload user namespace is made once, before anything runs inside,
//! and every spawn joins it before it execs. Workload processes therefore
//! hold CAP_SYS_PTRACE over each other — gdb can attach (§2.3) — and over
//! nothing else: tursi code is never dumpable while a workload process
//! exists, so neither the init nor a spawn on its way to exec can be traced,
//! and `/proc/<pid>/exe` of a tursi process is never reachable (§2.4).

use anyhow::{Context, Result, anyhow, bail};
use nix::libc;
use nix::errno::Errno;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sched::{CloneFlags, unshare};
use nix::sys::signal::{SigSet, Signal, killpg};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::statvfs::{FsFlags, statvfs};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, fork};
use std::collections::HashMap;
use std::ffi::CString;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::protocol::{self, Event, INIT_FD, Mount, Request, Spec, What};

#[used]
#[unsafe(link_section = ".init_array")]
static ENTRY: extern "C" fn() = entry;

/// Runs before `main` in every tursi process; returns at once unless this
/// process is `tursi __sandbox`, in which case it never returns.
extern "C" fn entry() {
    if !invoked_as_init() {
        return;
    }
    let code = match std::panic::catch_unwind(run) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            let _ = protocol::send(INIT_FD, &Event::Fatal(format!("{e:#}")), &[]);
            1
        }
        Err(_) => {
            let _ = protocol::send(INIT_FD, &Event::Fatal("sandbox init panicked".into()), &[]);
            1
        }
    };
    // SAFETY: plain process exit; nothing here owns state that needs unwinding.
    unsafe { libc::_exit(code) }
}

fn invoked_as_init() -> bool {
    std::fs::read("/proc/self/cmdline")
        .map(|c| c.split(|b| *b == 0).nth(1) == Some(&b"__sandbox"[..]))
        .unwrap_or(false)
}

fn run() -> Result<()> {
    let Some((Request::Setup(spec), _)) = protocol::recv(INIT_FD)? else {
        bail!("expected a setup message");
    };
    let (uid, gid) = (nix::unistd::getuid().as_raw(), nix::unistd::getgid().as_raw());
    unshare(CloneFlags::CLONE_NEWUSER)
        .map_err(|e| anyhow!("unprivileged user namespaces are unavailable ({e})"))?;
    map_ids(uid, gid)?;
    unshare(
        CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_NEWPID
            | CloneFlags::CLONE_NEWIPC
            | CloneFlags::CLONE_NEWUTS
            | CloneFlags::CLONE_NEWNET,
    )
    .map_err(|e| anyhow!("creating the sandbox namespaces failed ({e})"))?;
    // CLONE_NEWPID applies to children: the init is our child.
    // SAFETY: still single-threaded (pre-main), so the child may run anything.
    match unsafe { fork() }? {
        ForkResult::Child => init(spec),
        ForkResult::Parent { child } => {
            // Only the init may hold the socket, so the harness sees EOF the
            // moment the init dies.
            let _ = nix::unistd::close(INIT_FD);
            loop {
                match waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, code)) => unsafe { libc::_exit(code) },
                    Ok(WaitStatus::Signaled(..)) => unsafe { libc::_exit(1) },
                    Err(Errno::EINTR) | Ok(_) => continue,
                    Err(_) => unsafe { libc::_exit(1) },
                }
            }
        }
    }
}

/// Map our own uid/gid 1:1 into the namespace just unshared.
fn map_ids(uid: u32, gid: u32) -> Result<()> {
    std::fs::write("/proc/self/setgroups", "deny").context("writing setgroups")?;
    std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1\n")).context("writing uid_map")?;
    std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1\n")).context("writing gid_map")?;
    Ok(())
}

/// The workload user namespace, nested in the init's, held open by the
/// returned descriptor. A short-lived child makes it: its id maps can only be
/// written while it is dumpable, which is harmless now — nothing runs inside
/// yet — and never again once spawns join by `setns`.
fn workload_userns() -> Result<OwnedFd> {
    let (uid, gid) = (nix::unistd::getuid().as_raw(), nix::unistd::getgid().as_raw());
    let (report_r, report_w) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
    // SAFETY: the init is single-threaded.
    match unsafe { fork() }? {
        ForkResult::Child => {
            drop(report_r);
            let made = (|| -> Result<()> {
                nix::sys::prctl::set_dumpable(true)?;
                unshare(CloneFlags::CLONE_NEWUSER).context("nested user namespace")?;
                map_ids(uid, gid)
            })();
            // Empty report = made. Then wait to be killed: the init opens
            // the namespace through our /proc entry first.
            let report = made.err().map(|e| format!("{e:#}")).unwrap_or_default();
            let _ = nix::unistd::write(&report_w, report.as_bytes());
            drop(report_w);
            loop {
                nix::unistd::pause();
            }
        }
        ForkResult::Parent { child } => {
            drop(report_w);
            let mut report = String::new();
            let read = std::fs::File::from(report_r).read_to_string(&mut report);
            let opened = match (read, report.is_empty()) {
                (Ok(_), true) => std::fs::File::open(format!("/proc/{child}/ns/user")).map(OwnedFd::from).with_context(
                    || format!("opening the workload user namespace via /proc/{child}/ns/user (did the helper child die early?)"),
                ),
                (Ok(_), false) => Err(anyhow!("{report}")),
                (Err(e), _) => Err(anyhow!("reading the namespace child's report: {e}")),
            };
            let _ = nix::sys::signal::kill(child, Signal::SIGKILL);
            let _ = waitpid(child, None);
            opened
        }
    }
}

fn init(spec: Spec) -> ! {
    let setup = (|| -> Result<OwnedFd> {
        // If the outer waiter dies, so does the sandbox.
        nix::sys::prctl::set_pdeathsig(Signal::SIGKILL)?;
        build_root(&spec)?;
        // Locks /proc/1 (and its exe link) against the sandbox (§2.4).
        nix::sys::prctl::set_dumpable(false)?;
        loopback_up()?;
        if let Some(proxy) = &spec.proxy {
            start_forwarder(proxy)?;
        }
        let workload = workload_userns()?;
        std::env::set_current_dir(&spec.cwd).with_context(|| format!("entering {}", spec.cwd.display()))?;
        Ok(workload)
    })();
    let served = setup.and_then(|workload| {
        protocol::send(INIT_FD, &Event::Ready, &[])?;
        serve(workload.as_fd())
    });
    let code = match served {
        Ok(()) => 0,
        Err(e) => {
            let _ = protocol::send(INIT_FD, &Event::Fatal(format!("{e:#}")), &[]);
            1
        }
    };
    unsafe { libc::_exit(code) }
}

// ── the network (PERMISSIONS.md §4) ───────────────────────────────────────

#[repr(C)]
struct IfReq {
    name: [libc::c_char; libc::IFNAMSIZ],
    flags: libc::c_short,
    _pad: [u8; 22],
}

/// The new network namespace has only `lo`, and it starts down.
fn loopback_up() -> Result<()> {
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if sock < 0 {
        bail!("socket: {}", std::io::Error::last_os_error());
    }
    let mut req = IfReq { name: [0; libc::IFNAMSIZ], flags: 0, _pad: [0; 22] };
    for (dst, src) in req.name.iter_mut().zip(b"lo\0") {
        *dst = *src as libc::c_char;
    }
    let result = unsafe {
        if libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut req) < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            req.flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            if libc::ioctl(sock, libc::SIOCSIFFLAGS as _, &req) < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
        }
    };
    unsafe { libc::close(sock) };
    result.context("bringing up loopback in the sandbox")
}

/// A child of the init that accepts on loopback `port` and pipes each
/// connection to the harness's proxy socket. Still tursi code: non-dumpable,
/// in the init's user namespace, and holding no capabilities the workload
/// could use. Dies with the init.
fn start_forwarder(proxy: &protocol::ProxySpec) -> Result<()> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", proxy.port))
        .with_context(|| format!("binding the proxy forwarder on 127.0.0.1:{}", proxy.port))?;
    let socket = proxy.socket.clone();
    // SAFETY: the init is single-threaded; the child never touches init state.
    match unsafe { fork() }? {
        ForkResult::Parent { .. } => Ok(()),
        ForkResult::Child => {
            let _ = nix::unistd::close(INIT_FD);
            let _ = nix::sys::prctl::set_pdeathsig(Signal::SIGKILL);
            for conn in listener.incoming() {
                let Ok(tcp) = conn else { continue };
                let socket = socket.clone();
                std::thread::spawn(move || {
                    if let Ok(unix) = std::os::unix::net::UnixStream::connect(&socket) {
                        pump(tcp, unix);
                    }
                });
            }
            unsafe { libc::_exit(0) }
        }
    }
}

/// Copy both ways until both sides close.
fn pump(tcp: std::net::TcpStream, unix: std::os::unix::net::UnixStream) {
    use std::io::{Read, Write, copy};
    let (Ok(mut tcp_r), Ok(mut unix_r)) = (tcp.try_clone(), unix.try_clone()) else { return };
    let (mut tcp_w, mut unix_w) = (tcp, unix);
    let up = std::thread::spawn(move || {
        let _ = copy(&mut tcp_r, &mut unix_w);
        let _ = unix_w.shutdown(std::net::Shutdown::Write);
    });
    let _ = copy(&mut unix_r, &mut tcp_w);
    let _ = tcp_w.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
    // Silence the unused-import lint on Read/Write when copy covers both.
    let _: fn(&mut dyn Read, &mut dyn Write) = |_, _| {};
}

// ── the filesystem view ────────────────────────────────────────────────────

fn build_root(spec: &Spec) -> Result<()> {
    let none = None::<&str>;
    mount(none, "/", none, MsFlags::MS_REC | MsFlags::MS_PRIVATE, none).context("making / private")?;
    let root = &spec.root;
    mount(Some("tmpfs"), root, Some("tmpfs"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some("mode=0755"))
        .context("mounting the root tmpfs")?;
    for m in &spec.mounts {
        apply(root, m).with_context(|| format!("mounting {}", m.at.display()))?;
    }
    mask(root, &spec.masks)?;

    std::fs::create_dir(root.join(".oldroot"))?;
    std::env::set_current_dir(root)?;
    nix::unistd::pivot_root(".", ".oldroot").context("pivot_root")?;
    std::env::set_current_dir("/")?;
    umount2("/.oldroot", MntFlags::MNT_DETACH).context("detaching the old root")?;
    std::fs::remove_dir("/.oldroot")?;
    // Unlisted paths can't be created either: the root itself goes read-only.
    let flags = MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV;
    mount(none, "/", none, flags, none).context("making / read-only")?;
    Ok(())
}

fn inside(root: &Path, at: &Path) -> PathBuf {
    root.join(at.strip_prefix("/").unwrap_or(at))
}

fn apply(root: &Path, m: &Mount) -> Result<()> {
    let at = inside(root, &m.at);
    let none = None::<&str>;
    match &m.what {
        What::Bind { src, writable } => {
            if std::fs::metadata(src)?.is_dir() {
                std::fs::create_dir_all(&at)?;
            } else if !at.exists() {
                if let Some(parent) = at.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::File::create(&at)?;
            }
            mount(Some(src.as_path()), &at, none, MsFlags::MS_BIND | MsFlags::MS_REC, none)?;
            if !writable {
                remount_tree_read_only(&at)?;
            }
        }
        What::Tmpfs { mode } => {
            std::fs::create_dir_all(&at)?;
            let options = format!("mode={mode:o}");
            mount(Some("tmpfs"), &at, Some("tmpfs"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some(options.as_str()))?;
        }
        What::Symlink { target } => {
            if let Some(parent) = at.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::os::unix::fs::symlink(target, &at)?;
        }
        What::Proc => {
            std::fs::create_dir_all(&at)?;
            let flags = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
            mount(Some("proc"), &at, Some("proc"), flags, none)?;
        }
        What::Dev => build_dev(&at)?,
    }
    Ok(())
}

/// Read-only for a recursive bind and the mounts under it. A remount must
/// repeat the flags the kernel locked on the source mount (nosuid, nodev,
/// noexec, atime) or a user namespace gets EPERM. The listed path itself must
/// succeed; nested mounts (/sys has dozens) are best effort — ordinary file
/// permissions still apply under any that stay writable.
/// The project read-only for this process alone; build-output directories
/// stay writable (bound over themselves first, so the remount skips them).
fn project_read_only(ro: &protocol::ReadOnly) -> Result<()> {
    let none = None::<&str>;
    unshare(CloneFlags::CLONE_NEWNS).context("private mount namespace")?;
    mount(none, "/", none, MsFlags::MS_REC | MsFlags::MS_PRIVATE, none).context("making mounts private")?;
    for dir in ro.writable.iter().filter(|d| d.starts_with(&ro.project) && d.is_dir()) {
        mount(Some(dir.as_path()), dir, none, MsFlags::MS_BIND, none).with_context(|| format!("keeping {} writable", dir.display()))?;
    }
    remount_read_only(&ro.project).with_context(|| format!("read-only remount of {}", ro.project.display()))
}

/// Remount one mount point read-only, keeping the flags a user namespace may
/// not drop (nosuid, nodev, noexec, atime) or the remount fails with EPERM.
fn remount_read_only(point: &Path) -> nix::Result<()> {
    let locked = statvfs(point)?.flags();
    let mut flags = MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY;
    for (st, ms) in [
        (FsFlags::ST_NOSUID, MsFlags::MS_NOSUID),
        (FsFlags::ST_NODEV, MsFlags::MS_NODEV),
        (FsFlags::ST_NOEXEC, MsFlags::MS_NOEXEC),
        (FsFlags::ST_NOATIME, MsFlags::MS_NOATIME),
        (FsFlags::ST_NODIRATIME, MsFlags::MS_NODIRATIME),
        // ST_RELATIME, which nix leaves out on musl.
        (FsFlags::from_bits_retain(0x1000), MsFlags::MS_RELATIME),
    ] {
        if locked.contains(st) {
            flags |= ms;
        }
    }
    mount(None::<&str>, point, None::<&str>, flags, None::<&str>)
}

fn remount_tree_read_only(top: &Path) -> Result<()> {
    let mut points = vec![top.to_path_buf()];
    for line in std::fs::read_to_string("/proc/self/mountinfo")?.lines() {
        if let Some(point) = line.split(' ').nth(4).map(unescape_mountinfo) {
            let point = PathBuf::from(point);
            if point != top && point.starts_with(top) {
                points.push(point);
            }
        }
    }
    for point in points {
        let remounted = remount_read_only(&point);
        if point == top {
            remounted.with_context(|| format!("read-only remount of {}", point.display()))?;
        }
    }
    Ok(())
}

/// mountinfo escapes space, tab, newline, and backslash as `\ooo`.
fn unescape_mountinfo(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1..i + 4].iter().all(|b| (b'0'..=b'7').contains(b)) {
            let n = (bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + (bytes[i + 3] - b'0');
            out.push(n);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn build_dev(dev: &Path) -> Result<()> {
    let none = None::<&str>;
    std::fs::create_dir_all(dev)?;
    mount(Some("tmpfs"), dev, Some("tmpfs"), MsFlags::MS_NOSUID, Some("mode=0755"))?;
    // Device nodes can't be created in a user namespace; bind the host's.
    for name in ["null", "zero", "full", "random", "urandom", "tty"] {
        let node = dev.join(name);
        std::fs::File::create(&node)?;
        mount(Some(Path::new("/dev").join(name).as_path()), &node, none, MsFlags::MS_BIND, none)
            .with_context(|| format!("binding /dev/{name}"))?;
    }
    std::fs::create_dir(dev.join("pts"))?;
    mount(
        Some("devpts"),
        &dev.join("pts"),
        Some("devpts"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=620"),
    )?;
    std::os::unix::fs::symlink("pts/ptmx", dev.join("ptmx"))?;
    std::fs::create_dir(dev.join("shm"))?;
    mount(Some("tmpfs"), &dev.join("shm"), Some("tmpfs"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some("mode=1777"))?;
    for (link, target) in
        [("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2")]
    {
        std::os::unix::fs::symlink(target, dev.join(link))?;
    }
    Ok(())
}

/// Cover each existing path with an empty, mode-000 file (§2.4).
fn mask(root: &Path, masks: &[PathBuf]) -> Result<()> {
    let blank = root.join(".tursi-masked");
    std::fs::File::create(&blank)?;
    std::fs::set_permissions(&blank, std::fs::Permissions::from_mode(0o000))?;
    for path in masks {
        let at = inside(root, path);
        if at.is_file() {
            mount(Some(blank.as_path()), &at, None::<&str>, MsFlags::MS_BIND, None::<&str>)
                .with_context(|| format!("masking {}", path.display()))?;
            remount_tree_read_only(&at)?;
        }
    }
    Ok(())
}

// ── serving spawn requests ─────────────────────────────────────────────────

fn serve(workload: BorrowedFd) -> Result<()> {
    let mut sigchld = SigSet::empty();
    sigchld.add(Signal::SIGCHLD);
    sigchld.thread_block()?;
    let signals = SignalFd::with_flags(&sigchld, SfdFlags::SFD_CLOEXEC | SfdFlags::SFD_NONBLOCK)?;
    // SAFETY: INIT_FD stays open for the life of the init.
    let sock = unsafe { BorrowedFd::borrow_raw(INIT_FD) };
    let mut running: HashMap<Pid, u64> = HashMap::new();
    loop {
        let mut fds = [PollFd::new(sock, PollFlags::POLLIN), PollFd::new(signals.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::NONE) {
            Err(Errno::EINTR) => continue,
            Err(e) => bail!("poll: {e}"),
            Ok(_) => {}
        }
        let sock_ready = fds[0].revents().is_some_and(|r| !r.is_empty());
        let child_ready = fds[1].revents().is_some_and(|r| !r.is_empty());
        if sock_ready {
            match protocol::recv::<Request>(INIT_FD)? {
                // The harness is gone: exiting as PID 1 kills everything inside.
                None => return Ok(()),
                Some((Request::Spawn { id, argv, env, cwd, read_only }, fds)) => {
                    let event = match spawn(&argv, &env, &cwd, fds, workload, &sigchld, read_only.as_ref()) {
                        Ok(pid) => {
                            running.insert(pid, id);
                            Event::Spawned { id }
                        }
                        Err(e) => Event::SpawnFailed { id, error: format!("{e:#}") },
                    };
                    protocol::send(INIT_FD, &event, &[])?;
                }
                Some((Request::Kill { id }, _)) => {
                    if let Some((pid, _)) = running.iter().find(|(_, i)| **i == id) {
                        let _ = killpg(*pid, Signal::SIGKILL);
                    }
                }
                Some((Request::Setup(_), _)) => bail!("setup sent twice"),
            }
        }
        if child_ready {
            while let Ok(Some(_)) = signals.read_signal() {}
            reap(&mut running)?;
        }
    }
}

fn reap(running: &mut HashMap<Pid, u64>) -> Result<()> {
    loop {
        let (pid, code, signal) = match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, code)) => (pid, Some(code), None),
            Ok(WaitStatus::Signaled(pid, sig, _)) => (pid, None, Some(sig as i32)),
            Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => return Ok(()),
            Err(Errno::EINTR) | Ok(_) => continue,
            Err(e) => bail!("waitpid: {e}"),
        };
        if let Some(id) = running.remove(&pid) {
            protocol::send(INIT_FD, &Event::Exited { id, code, signal }, &[])?;
        }
    }
}

/// Fork a workload process; returns once it has exec'd (or reports why not,
/// via a close-on-exec pipe).
fn spawn(
    argv: &[String],
    env: &[(String, String)],
    cwd: &Path,
    fds: Vec<OwnedFd>,
    workload: BorrowedFd,
    sigchld: &SigSet,
    read_only: Option<&protocol::ReadOnly>,
) -> Result<Pid> {
    let [stdin, stdout, stderr]: [OwnedFd; 3] =
        fds.try_into().map_err(|_| anyhow!("spawn needs exactly three descriptors"))?;
    if argv.is_empty() {
        bail!("empty argv");
    }
    let cstr = |s: &str| CString::new(s).map_err(|_| anyhow!("NUL byte in {s:?}"));
    let args = argv.iter().map(|a| cstr(a)).collect::<Result<Vec<_>>>()?;
    let envp = env.iter().map(|(k, v)| cstr(&format!("{k}={v}"))).collect::<Result<Vec<_>>>()?;
    let (err_read, err_write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;

    // SAFETY: the init is single-threaded; the child only makes syscalls and
    // execs (or reports and _exits).
    match unsafe { fork() }? {
        ForkResult::Child => {
            drop(err_read);
            let error = exec_workload(&args, &envp, cwd, [&stdin, &stdout, &stderr], workload, sigchld, read_only);
            let _ = nix::unistd::write(&err_write, format!("{error:#}").as_bytes());
            unsafe { libc::_exit(127) }
        }
        ForkResult::Parent { child } => {
            drop(err_write);
            drop((stdin, stdout, stderr));
            let mut error = String::new();
            std::fs::File::from(err_read).read_to_string(&mut error)?;
            if error.is_empty() {
                Ok(child)
            } else {
                let _ = waitpid(child, None);
                bail!("{error}")
            }
        }
    }
}

/// In the forked child: own process group, the workload user namespace with
/// only CAP_SYS_PTRACE (ambient, so it survives exec), stdio, exec. Returns
/// only on failure. Stays non-dumpable (inherited from the init) until the
/// exec resets it.
fn exec_workload(
    args: &[CString],
    envp: &[CString],
    cwd: &Path,
    stdio: [&OwnedFd; 3],
    workload: BorrowedFd,
    sigchld: &SigSet,
    read_only: Option<&protocol::ReadOnly>,
) -> anyhow::Error {
    let attempt = || -> Result<std::convert::Infallible> {
        sigchld.thread_unblock()?;
        nix::unistd::setsid()?;
        // Before joining the workload namespace, while this child still holds
        // the init's mount rights: a private mount namespace where the
        // project is read-only. The workload has no mount rights over it, so
        // it cannot undo this (§5.7, PERMISSIONS.md §3).
        if let Some(ro) = read_only {
            project_read_only(ro)?;
        }
        nix::sched::setns(workload, CloneFlags::CLONE_NEWUSER).context("joining the workload user namespace")?;
        keep_only_ptrace()?;
        for (fd, target) in stdio.iter().zip(0..) {
            if unsafe { libc::dup2(fd.as_raw_fd(), target) } < 0 {
                bail!("dup2: {}", std::io::Error::last_os_error());
            }
        }
        // Everything else (the socket, the signalfd, received fds) closes on exec.
        let close_rest = unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, libc::CLOSE_RANGE_CLOEXEC) };
        if close_rest != 0 {
            bail!("close_range: {}", std::io::Error::last_os_error());
        }
        std::env::set_current_dir(cwd).with_context(|| format!("cd {}", cwd.display()))?;
        nix::unistd::execvpe(&args[0], args, envp).with_context(|| format!("exec {:?}", args[0]))
    };
    match attempt() {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
const CAP_SYS_PTRACE: u32 = 19;

/// Drop every capability but CAP_SYS_PTRACE and make that one ambient, so the
/// workload keeps it across exec — confined to the workload namespace (§2.3).
fn keep_only_ptrace() -> Result<()> {
    let header = CapHeader { version: LINUX_CAPABILITY_VERSION_3, pid: 0 };
    let bit = 1u32 << CAP_SYS_PTRACE;
    let data = [CapData { effective: bit, permitted: bit, inheritable: bit }, CapData::default()];
    if unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } != 0 {
        bail!("capset: {}", std::io::Error::last_os_error());
    }
    let raised = unsafe {
        libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_RAISE, CAP_SYS_PTRACE as libc::c_ulong, 0, 0)
    };
    if raised != 0 {
        bail!("ambient CAP_SYS_PTRACE: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_octal_escapes_decode() {
        assert_eq!(unescape_mountinfo(r"/home/a\040b"), "/home/a b");
        assert_eq!(unescape_mountinfo("/plain"), "/plain");
        assert_eq!(unescape_mountinfo(r"/x\134y"), r"/x\y");
    }
}
