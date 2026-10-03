//! The sandbox's filesystem view (PERMISSIONS.md §3) as an ordered mount list.
//! An allowlist: anything not listed here does not exist inside.

use std::path::{Path, PathBuf};

use super::protocol::{Mount, ProxySpec, Spec, What};

/// System directories, read-only. Merged-/usr symlinks (`/bin -> usr/bin`)
/// are recreated as symlinks.
const SYSTEM: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/opt", "/etc", "/sys"];

/// Toolchains and the config they need, read-only, relative to $HOME.
const HOME_READ_ONLY: &[&str] = &[
    ".rustup",
    ".cargo/bin",
    ".cargo/config.toml",
    ".cargo/config",
    ".local/bin",
    ".local/lib",
    ".pyenv",
    ".nvm",
    "go/bin",
    ".gitconfig",
    ".config/git",
    ".tursi/tools/bin",
];

/// Package-manager caches, writable (accepted risk, PERMISSIONS.md §3).
const HOME_CACHES: &[&str] =
    &[".cargo/registry", ".cargo/git", ".npm", ".cache/pip", ".cache/uv", "go/pkg/mod"];

/// Host directories for one session, all under `/tmp/tursi-<uid>/<session>`.
pub struct Session {
    /// The sandbox's /tmp.
    pub tmp: PathBuf,
    /// Harness I/O (step output files), mounted at `IO_INSIDE`.
    pub io: PathBuf,
    /// Empty mountpoint for the new root.
    pub root: PathBuf,
    /// Holds the proxy socket; mounted read-only at `PROXY_INSIDE` (connecting
    /// to a socket needs no write access, and the workload can't swap it).
    pub proxy: PathBuf,
}

/// Where the harness I/O directory appears inside the sandbox.
pub const IO_INSIDE: &str = "/run/tursi/io";
/// Where the proxy socket's directory appears inside the sandbox.
pub const PROXY_INSIDE: &str = "/run/tursi/proxy";
pub const PROXY_SOCKET: &str = "proxy.sock";

pub struct Extra<'a> {
    pub read_only: &'a [PathBuf],
    pub read_write: &'a [PathBuf],
}

/// Build the mount list. Sources that don't exist are skipped. Host
/// mountpoints the view needs (`.tursi/worktrees`, `.git/hooks`) must already
/// exist — `prepare_project` makes them.
pub fn plan(project: &Path, home: Option<&Path>, session: &Session, extra: &Extra, masks: Vec<PathBuf>) -> Spec {
    let mut mounts = Vec::new();
    let bind = |mounts: &mut Vec<Mount>, at: &Path, writable: bool| {
        if at.exists() {
            mounts.push(Mount { at: at.to_path_buf(), what: What::Bind { src: at.to_path_buf(), writable } });
        }
    };

    for dir in SYSTEM {
        let path = Path::new(dir);
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => {
                if let Ok(target) = std::fs::read_link(path) {
                    mounts.push(Mount { at: path.to_path_buf(), what: What::Symlink { target } });
                }
            }
            Ok(m) if m.is_dir() => bind(&mut mounts, path, false),
            _ => {}
        }
    }
    mounts.push(Mount { at: "/proc".into(), what: What::Proc });
    mounts.push(Mount { at: "/dev".into(), what: What::Dev });
    mounts.push(Mount { at: "/tmp".into(), what: What::Bind { src: session.tmp.clone(), writable: true } });
    mounts.push(Mount { at: IO_INSIDE.into(), what: What::Bind { src: session.io.clone(), writable: true } });
    mounts.push(Mount { at: PROXY_INSIDE.into(), what: What::Bind { src: session.proxy.clone(), writable: false } });

    if let Some(home) = home {
        // An empty, writable, throwaway $HOME: tools may write dotfiles and
        // caches there, nothing of the real home shows, nothing persists.
        mounts.push(Mount { at: home.to_path_buf(), what: What::Tmpfs { mode: 0o700 } });
        for rel in HOME_READ_ONLY {
            bind(&mut mounts, &home.join(rel), false);
        }
        for rel in HOME_CACHES {
            bind(&mut mounts, &home.join(rel), true);
        }
    }
    for path in extra.read_only {
        bind(&mut mounts, path, false);
    }
    for path in extra.read_write {
        bind(&mut mounts, path, true);
    }

    bind(&mut mounts, project, true);
    bind(&mut mounts, &project.join(".tursi"), false);
    bind(&mut mounts, &project.join(".tursi/worktrees"), true);
    bind(&mut mounts, &project.join(".tursi/profiles"), true);
    if project.join(".git").is_dir() {
        bind(&mut mounts, &project.join(".git/hooks"), false);
        bind(&mut mounts, &project.join(".git/config"), false);
    }

    // Parents before children; ties keep insertion order (stable sort).
    mounts.sort_by_key(|m| m.at.components().count());
    let proxy = Some(ProxySpec { socket: Path::new(PROXY_INSIDE).join(PROXY_SOCKET), port: super::proxy::PORT });
    Spec { root: session.root.clone(), mounts, masks, cwd: project.to_path_buf(), proxy }
}

/// Host-side mountpoints the view needs inside the project.
pub fn prepare_project(project: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(project.join(".tursi/worktrees"))?;
    std::fs::create_dir_all(project.join(".tursi/profiles"))?;
    // A missing hooks dir would let the agent create one — and its hooks would
    // run the next time the user runs git (PERMISSIONS.md §5.4).
    if project.join(".git").is_dir() {
        std::fs::create_dir_all(project.join(".git/hooks"))?;
    }
    Ok(())
}

/// tursi's own executables, to be masked inside (PERMISSIONS.md §2.4): the
/// running binary, and every `tursi` on PATH — each as found and resolved.
pub fn self_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut add = |p: PathBuf| {
        if let Ok(real) = p.canonicalize()
            && !paths.contains(&real)
        {
            paths.push(real);
        }
        if !paths.contains(&p) {
            paths.push(p);
        }
    };
    if let Ok(exe) = std::env::current_exe() {
        add(exe);
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let candidate = dir.join("tursi");
        if candidate.is_file() {
            add(candidate);
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil;

    fn session(dir: &Path) -> Session {
        Session { tmp: dir.join("tmp"), io: dir.join("io"), root: dir.join("root"), proxy: dir.join("proxy") }
    }

    #[test]
    fn the_view_is_ordered_parents_first_and_protects_tursi_and_git() {
        let dir = testutil::tmp("mounts-plan");
        let project = dir.join("proj");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::write(project.join(".git/config"), "").unwrap();
        prepare_project(&project).unwrap();
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let spec = plan(&project, Some(&home), &session(&dir), &Extra { read_only: &[], read_write: &[] }, vec![]);

        let find = |p: &Path| spec.mounts.iter().position(|m| m.at == p);
        let what = |p: &Path| spec.mounts.iter().find(|m| m.at == p).map(|m| m.what.clone());
        // Depth order: home tmpfs before the toolchain inside it; project before .tursi.
        assert!(find(&home) < find(&home.join(".cargo/bin")));
        assert!(find(&project) < find(&project.join(".tursi")));
        assert!(find(&project.join(".tursi")) < find(&project.join(".tursi/worktrees")));
        // Writability per the allowlist.
        assert_eq!(what(&project), Some(What::Bind { src: project.clone(), writable: true }));
        assert!(matches!(what(&project.join(".tursi")), Some(What::Bind { writable: false, .. })));
        assert!(matches!(what(&project.join(".tursi/worktrees")), Some(What::Bind { writable: true, .. })));
        assert!(matches!(what(&project.join(".git/hooks")), Some(What::Bind { writable: false, .. })));
        assert!(matches!(what(&project.join(".git/config")), Some(What::Bind { writable: false, .. })));
        assert!(matches!(what(&home.join(".cargo/bin")), Some(What::Bind { writable: false, .. })));
        assert_eq!(what(&home), Some(What::Tmpfs { mode: 0o700 }));
        // Never mounted: secrets simply don't exist inside.
        assert!(find(&home.join(".ssh")).is_none());
    }
}
