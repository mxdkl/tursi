//! The system card (§4.5): probed once at startup, injected into the system
//! prompt so hardware questions never cost a discovery turn.

use std::path::Path;

pub struct SystemCard {
    /// Model, arch, cores/threads (P/E split if hybrid), boost, L1d/L2/L3, NUMA.
    pub cpu: String,
    /// Curated perf-relevant ISA flags only — never the full cpuinfo soup.
    pub isa: Vec<String>,
    pub mem: String,
    pub gpus: Vec<String>,
    /// Distro, kernel, libc, project-dir filesystem.
    pub os: String,
    /// rustc/cargo, cc/clang, python — versions.
    pub toolchains: Vec<String>,
    pub caps: Capabilities,
}

pub struct Capabilities {
    pub sandbox: bool,
    pub perf_event_paranoid: Option<i32>,
    pub ptrace_scope: Option<u32>,
    /// GDB ≥ 14 with native DAP (§4.3); older gdb degrades the debug tool.
    pub gdb_dap: bool,
    pub rr: bool,
    pub profile_backends: Vec<String>,
}

/// Curated perf-relevant ISA flags — never the full cpuinfo soup (§4.5).
const CURATED_ISA: &[&str] = &[
    "avx", "avx2", "avx512f", "avx512bw", "avx512vl", "avx512dq", "avx512vnni",
    "fma", "bmi2", "sse4_2", "aes", "vaes", "sha_ni", "amx_tile", "amx_int8",
];

/// lscpu, /proc/meminfo, /etc/os-release, uname, nvidia-smi (lspci fallback),
/// plus tool probes. Never fails — unknown fields render as "unknown".
pub fn probe(project: &Path, sandbox: bool) -> SystemCard {
    let lscpu = sh("lscpu", &[]).unwrap_or_default();
    let pick = |prefix: &str| -> Option<String> {
        lscpu
            .lines()
            .find(|l| l.starts_with(prefix))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.split('(').next().unwrap_or(v).trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let cpu = {
        let model = pick("Model name").unwrap_or_else(|| "unknown".to_string());
        let cpus = pick("CPU(s)").unwrap_or_else(|| "?".to_string());
        let caches: Vec<String> = ["L1d cache", "L2 cache", "L3 cache"]
            .iter()
            .filter_map(|c| pick(c).map(|v| format!("{} {v}", &c[..c.find(' ').unwrap_or(2)])))
            .collect();
        let numa = pick("NUMA node(s)").unwrap_or_else(|| "?".to_string());
        format!("{model} ({cpus} cpus), {}, NUMA {numa}", caches.join(" "))
    };

    let isa = read("/proc/cpuinfo")
        .and_then(|s| s.lines().find(|l| l.starts_with("flags")).map(str::to_string))
        .map(|flags| {
            CURATED_ISA
                .iter()
                .filter(|f| flags.split_whitespace().any(|x| x == **f))
                .map(|f| f.to_string())
                .collect()
        })
        .unwrap_or_default();

    let mem = read("/proc/meminfo")
        .map(|m| {
            let get = |k: &str| {
                m.lines()
                    .find(|l| l.starts_with(k))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<f64>().ok())
                    .map(|kb| kb / 1024.0 / 1024.0)
            };
            format!(
                "{:.1} GiB RAM, {:.1} GiB swap",
                get("MemTotal").unwrap_or(0.0),
                get("SwapTotal").unwrap_or(0.0)
            )
        })
        .unwrap_or_else(|| "unknown".to_string());

    let gpus = sh("nvidia-smi", &["--query-gpu=name,memory.total", "--format=csv,noheader"])
        .map(|out| out.lines().map(str::to_string).collect::<Vec<_>>())
        .or_else(|| {
            sh("lspci", &[]).map(|out| {
                out.lines()
                    .filter(|l| l.contains("VGA") || l.contains("3D controller"))
                    .filter_map(|l| l.split_once(": ").map(|(_, gpu)| gpu.to_string()))
                    .collect()
            })
        })
        .unwrap_or_default();

    let os = format!(
        "{}, kernel {}, {}, project fs {}",
        read("/etc/os-release")
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("PRETTY_NAME="))
                    .map(|l| l.trim_start_matches("PRETTY_NAME=").trim_matches('"').to_string())
            })
            .unwrap_or_else(|| "unknown".to_string()),
        sh("uname", &["-r"]).unwrap_or_else(|| "?".to_string()),
        sh("getconf", &["GNU_LIBC_VERSION"]).unwrap_or_else(|| "libc ?".to_string()),
        sh("stat", &["-f", "-c", "%T", &project.display().to_string()])
            .unwrap_or_else(|| "?".to_string()),
    );

    let toolchains = [
        ("rustc", vec!["--version"]),
        ("cargo", vec!["--version"]),
        ("cc", vec!["--version"]),
        ("python3", vec!["--version"]),
        ("gdb", vec!["--version"]),
    ]
    .iter()
    .filter_map(|(prog, args)| {
        sh(prog, &args.iter().map(|s| *s).collect::<Vec<_>>())
            .map(|out| out.lines().next().unwrap_or("").to_string())
    })
    .collect();

    let gdb_dap = sh("gdb", &["--version"])
        .and_then(|banner| {
            banner
                .split_whitespace()
                .filter_map(|w| w.split('.').next()?.parse::<u32>().ok())
                .find(|major| *major >= 14)
        })
        .is_some();
    let mut profile_backends: Vec<String> = ["perf", "strace", "hyperfine", "heaptrack"]
        .iter()
        .filter(|b| crate::lsp::which(b).is_some())
        .map(|b| b.to_string())
        .collect();
    if Path::new("/usr/bin/time").exists() {
        profile_backends.push("gnu-time".to_string());
    }

    SystemCard {
        cpu,
        isa,
        mem,
        gpus,
        os,
        toolchains,
        caps: Capabilities {
            sandbox,
            perf_event_paranoid: read("/proc/sys/kernel/perf_event_paranoid")
                .and_then(|s| s.trim().parse().ok()),
            ptrace_scope: read("/proc/sys/kernel/yama/ptrace_scope")
                .and_then(|s| s.trim().parse().ok()),
            gdb_dap,
            rr: crate::lsp::which("rr").is_some(),
            profile_backends,
        },
    }
}

fn sh(program: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_never_fails_and_renders_a_complete_card() {
        let card = probe(Path::new("."), true);
        let rendered = card.render();
        assert!(rendered.contains("CPU:"));
        assert!(rendered.contains("OS:"));
        assert!(rendered.contains("Capabilities:"));
        assert!(!card.mem.is_empty());
        // ~200-token budget (§4.5): keep the card honest as probes grow.
        assert!(rendered.len() < 1200, "card too fat: {} bytes", rendered.len());
    }
}

impl SystemCard {
    /// ~200-token block for the system prompt; also shown by `:sysinfo`.
    pub fn render(&self) -> String {
        let opt_i32 = |v: Option<i32>| v.map_or("?".to_string(), |x| x.to_string());
        let opt_u32 = |v: Option<u32>| v.map_or("?".to_string(), |x| x.to_string());
        let mut s = format!("CPU: {}\n", self.cpu);
        if !self.isa.is_empty() {
            s.push_str(&format!("ISA: {}\n", self.isa.join(" ")));
        }
        s.push_str(&format!("Memory: {}\n", self.mem));
        if !self.gpus.is_empty() {
            s.push_str(&format!("GPU: {}\n", self.gpus.join("; ")));
        }
        s.push_str(&format!("OS: {}\n", self.os));
        if !self.toolchains.is_empty() {
            s.push_str(&format!("Toolchains: {}\n", self.toolchains.join(", ")));
        }
        s.push_str(&format!(
            "Capabilities: sandbox={} perf_event_paranoid={} ptrace_scope={} gdb_dap={} rr={} profilers=[{}]\n",
            self.caps.sandbox,
            opt_i32(self.caps.perf_event_paranoid),
            opt_u32(self.caps.ptrace_scope),
            self.caps.gdb_dap,
            self.caps.rr,
            self.caps.profile_backends.join(", "),
        ));
        s
    }
}
