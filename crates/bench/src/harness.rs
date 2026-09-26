//! Machine fingerprinting: enough about the host to tell two runs apart.

use serde::{Deserialize, Serialize};

/// Enough about the machine to know whether two runs are comparable.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Machine {
    pub cpu: String,
    pub cores: usize,
    pub ram_gb: f64,
    pub os: String,
}

impl Machine {
    pub fn detect() -> Self {
        Self {
            cpu: cpu_model(),
            cores: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
            ram_gb: total_ram_gb(),
            os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        }
    }

    /// Stable identity used to refuse cross-machine comparisons.
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|{}c|{:.0}g|{}",
            self.cpu, self.cores, self.ram_gb, self.os
        )
    }
}

/// One `sysctl -n <name>`, trimmed. macOS has no `/proc`, and the alternative
/// to shelling out once per run is a dependency on `libc` for `sysctlbyname`.
#[cfg(target_os = "macos")]
fn sysctl(name: &str) -> Option<String> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", name])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

fn cpu_model() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Some(v) = sysctl("machdep.cpu.brand_string") {
            return v;
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/cpuinfo") {
            for line in s.lines() {
                if let Some(v) = line.strip_prefix("model name") {
                    if let Some(v) = v.split(':').nth(1) {
                        return v.trim().to_string();
                    }
                }
            }
        }
    }
    "unknown".to_string()
}

fn total_ram_gb() -> f64 {
    #[cfg(target_os = "macos")]
    {
        if let Some(b) = sysctl("hw.memsize").and_then(|v| v.parse::<f64>().ok()) {
            return b / 1024.0 / 1024.0 / 1024.0;
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
            for line in s.lines() {
                if let Some(v) = line.strip_prefix("MemTotal:") {
                    let kb: f64 = v.trim().trim_end_matches(" kB").parse().unwrap_or(0.0);
                    return kb / 1024.0 / 1024.0;
                }
            }
        }
    }
    0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn the_machine_this_runs_on_is_described_rather_than_unknown() {
        // A run records its machine so two runs on different hardware can be
        // told apart; "unknown, 0 cores, 0 GB" would tell nothing apart.
        let m = Machine::detect();
        assert_ne!(m.cpu, "unknown", "{m:?}");
        assert!(m.cores > 0, "{m:?}");
        assert!(m.ram_gb > 0.5, "{m:?}");
        assert!(m.fingerprint().contains(std::env::consts::ARCH), "{m:?}");
    }
}
