//! The machine the bots run on: memory, swap, disk, and whether processes get the CPU on time.
//!
//! 2026-11-18 18:01Z: with 12 of 13.3 GB of swap used and the disk at 98% (so swap could not
//! grow), every socket's pong took 0.5 to 26 s for two and a half minutes and most games were
//! disqualified, while the network answered ICMP in 25 ms. The guard (`online::overload`)
//! pauses batch work when the machine is short of memory or disk, and the watchdog writes
//! the reading as the `machine` row of `gateway_state`, which `fleet health` and the status
//! page show.

use crate::config::Config;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Machine {
    /// `kern.memorystatus_level`: the share of memory the kernel counts as available
    pub memory_level_pct: Option<u32>,
    pub swap_used_mb: Option<u64>,
    pub swap_total_mb: Option<u64>,
    /// free space on the volume that holds `data/` (and the swap files)
    pub disk_free_gb: Option<f64>,
}

fn sysctl_raw(name: &str, buf: *mut libc::c_void, len: usize) -> bool {
    let Ok(c) = std::ffi::CString::new(name) else { return false };
    let mut l = len;
    unsafe { libc::sysctlbyname(c.as_ptr(), buf, &mut l, std::ptr::null_mut(), 0) == 0 && l == len }
}

#[cfg(target_os = "macos")]
fn swap() -> Option<(u64, u64)> {
    let mut x: libc::xsw_usage = unsafe { std::mem::zeroed() };
    sysctl_raw("vm.swapusage", &mut x as *mut _ as *mut libc::c_void, std::mem::size_of::<libc::xsw_usage>()).then(|| (x.xsu_used >> 20, x.xsu_total >> 20))
}

#[cfg(not(target_os = "macos"))]
fn swap() -> Option<(u64, u64)> {
    None
}

fn memory_level() -> Option<u32> {
    let mut v: libc::c_int = 0;
    sysctl_raw("kern.memorystatus_level", &mut v as *mut _ as *mut libc::c_void, std::mem::size_of::<libc::c_int>()).then_some(v as u32)
}

pub fn disk_free_gb(path: &Path) -> Option<f64> {
    // The volume of the nearest folder that exists (a fresh root has no `data/` yet).
    let mut path = path;
    while !path.exists() {
        path = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        if path == Path::new(".") {
            break;
        }
    }
    let c = std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes()).ok()?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some(s.f_bavail as f64 * s.f_bsize as f64 / 1e9)
}

pub fn read(root: &Path) -> Machine {
    let (used, total) = swap().map(|(u, t)| (Some(u), Some(t))).unwrap_or((None, None));
    Machine { memory_level_pct: memory_level(), swap_used_mb: used, swap_total_mb: total, disk_free_gb: disk_free_gb(&root.join("data")) }
}

impl Machine {
    /// What is wrong, if anything, against the guard's limits in `fleet/bots.json`
    /// (`guard.min_disk_gb`, `guard.max_swap_used_gb`, `guard.min_memory_level`).
    pub fn problems(&self, cfg: &Config) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(d) = self.disk_free_gb {
            if d < cfg.guard_min_disk_gb {
                out.push(format!("disk has {d:.1} GB free, under {:.0} GB (swap cannot grow)", cfg.guard_min_disk_gb));
            }
        }
        if let Some(u) = self.swap_used_mb {
            if u as f64 / 1024.0 > cfg.guard_max_swap_used_gb {
                out.push(format!("swap {:.1} GB used, over {:.0} GB", u as f64 / 1024.0, cfg.guard_max_swap_used_gb));
            }
        }
        if let Some(l) = self.memory_level_pct {
            if (l as f64) < cfg.guard_min_memory_level {
                out.push(format!("memory level {l}%, under {:.0}%", cfg.guard_min_memory_level));
            }
        }
        out
    }

    pub fn summary(&self) -> String {
        let gb = |mb: Option<u64>| mb.map(|m| format!("{:.1}", m as f64 / 1024.0)).unwrap_or_else(|| "?".into());
        format!(
            "memory level {}%, swap {} of {} GB used, disk {} GB free",
            self.memory_level_pct.map(|l| l.to_string()).unwrap_or_else(|| "?".into()),
            gb(self.swap_used_mb),
            gb(self.swap_total_mb),
            self.disk_free_gb.map(|d| format!("{d:.1}")).unwrap_or_else(|| "?".into())
        )
    }

    pub fn detail(&self, worst_stall_ms: u64) -> Value {
        json!({
            "memory_level_pct": self.memory_level_pct,
            "swap_used_mb": self.swap_used_mb,
            "swap_total_mb": self.swap_total_mb,
            "disk_free_gb": self.disk_free_gb.map(|d| (d * 10.0).round() / 10.0),
            "worst_stall_ms": worst_stall_ms,
        })
    }
}

/// The longest time, in ms, a thread asleep for 50 ms waited beyond that, since the last
/// `take_worst_stall`: how late this machine runs a process that wants the CPU.
static WORST_STALL_MS: AtomicU64 = AtomicU64::new(0);

pub fn start_stall_probe() {
    std::thread::Builder::new()
        .name("stall-probe".into())
        .spawn(|| loop {
            let t = Instant::now();
            std::thread::sleep(Duration::from_millis(50));
            let late = t.elapsed().as_millis().saturating_sub(50) as u64;
            WORST_STALL_MS.fetch_max(late, Ordering::Relaxed);
        })
        .ok();
}

pub fn take_worst_stall() -> u64 {
    WORST_STALL_MS.swap(0, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_machine_and_judges_it() {
        let m = read(Path::new("."));
        assert!(m.disk_free_gb.is_some());
        let mut cfg = Config::default();
        cfg.guard_min_disk_gb = 1e9;
        assert!(m.problems(&cfg).iter().any(|p| p.starts_with("disk")));
        let quiet = Machine { memory_level_pct: Some(50), swap_used_mb: Some(1024), swap_total_mb: Some(2048), disk_free_gb: Some(50.0) };
        assert!(quiet.problems(&Config::default()).is_empty());
        let short = Machine { memory_level_pct: Some(5), swap_used_mb: Some(12 * 1024), swap_total_mb: Some(13 * 1024), disk_free_gb: Some(3.0) };
        assert_eq!(short.problems(&Config::default()).len(), 3);
    }
}
