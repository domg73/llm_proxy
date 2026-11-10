// System monitor: samples the CPU, the GPU (nvidia-smi) and the RAM,
// publishing a snapshot the dashboard reads on every refresh. Runs on a
// tokio task; the external calls are offloaded to spawn_blocking
// (nvidia-smi ~0.1 s).
//
// Windows: CPU via GetSystemTimes, RAM via GlobalMemoryStatusEx, CPU name
// from the registry. Linux: CPU via /proc/stat, RAM via /proc/meminfo, CPU
// name from /proc/cpuinfo. GPU: nvidia-smi on both platforms.
//
// Poll cadence: every 2 s.
// Multi-GPU: nvidia-smi returns one CSV row per GPU; all of them are
// parsed. Missing GPU (no nvidia-smi / no NVIDIA card) => gpus is empty,
// the dashboard shows "n/a".

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows::Win32::Foundation::FILETIME;
#[cfg(windows)]
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
#[cfg(windows)]
use windows::Win32::System::Threading::GetSystemTimes;

#[derive(Default, Clone, Serialize)]
pub struct GpuStats {
    pub name: String,
    /// GPU core clock, MHz.
    pub core_clock_mhz: u32,
    /// Memory (VRAM bus) clock, MHz.
    pub mem_clock_mhz: u32,
    /// GPU temperature, °C (N/A degrades to 0).
    pub temp_c: u32,
    pub power_w: f64,
    pub vram_total_mb: u32,
    pub vram_used_mb: u32,
    pub util_pct: u32,
}

#[derive(Default, Clone, Copy, Serialize)]
pub struct RamStats {
    pub total_mb: u64,
    pub used_mb: u64,
}

#[derive(Default, Clone, Serialize)]
pub struct SysSnapshot {
    /// Overall CPU usage, percent (delta between two samples).
    pub cpu_pct: u32,
    /// CPU model name (registry /proc/cpuinfo, static, read once at startup).
    pub cpu_name: String,
    pub gpus: Vec<GpuStats>,
    pub ram: RamStats,
}

/// CPU usage percent since the previous sample: usage = 1 - Δidle/Δtotal.
/// First sample has no baseline = 0. Counters never go backwards
/// (saturating sub).
fn pct_between(prev: Option<(u128, u128)>, cur: (u128, u128)) -> u32 {
    let Some((pi, pt)) = prev else { return 0 };
    let di = cur.0.saturating_sub(pi);
    let dt = cur.1.saturating_sub(pt);
    if dt == 0 { return 0 };
    100u64.saturating_sub((di * 100 / dt) as u64) as u32
}

/// Cumulative CPU counters as (idle, total) ticks. The tick unit differs
/// per platform (100-ns on Windows, jiffies on Linux) — only the ratio
/// matters.
#[cfg(windows)]
fn system_times() -> Option<(u128, u128)> {
    unsafe {
        let mut idle: FILETIME = core::mem::zeroed();
        let mut kern: FILETIME = core::mem::zeroed();
        let mut user: FILETIME = core::mem::zeroed();
        if GetSystemTimes(
            Some(&mut idle as *mut FILETIME),
            Some(&mut kern as *mut FILETIME),
            Some(&mut user as *mut FILETIME),
        )
        .is_err()
        {
            return None;
        }
        Some((
            filetime_ticks(&idle),
            filetime_ticks(&kern) + filetime_ticks(&user),
        ))
    }
}

#[cfg(windows)]
fn filetime_ticks(ft: &FILETIME) -> u128 {
    let low = ft.dwLowDateTime as u64;
    let high = ft.dwHighDateTime as u64;
    u128::from((high << 32) | low)
}

/// Linux: the aggregate "cpu" line of /proc/stat (jiffies). idle =
/// field 4 (idle) + field 5 (iowait); total = all fields.
#[cfg(not(windows))]
fn system_times() -> Option<(u128, u128)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let fields: Vec<u128> = line
        .split_whitespace()
        .skip(1) // "cpu"
        .map(|s| s.parse().unwrap_or(0))
        .collect();
    if fields.len() < 5 {
        return None;
    }
    let idle = fields[3] + fields[4];
    let total: u128 = fields.iter().sum();
    Some((idle, total))
}

pub fn new_snapshot() -> Arc<Mutex<SysSnapshot>> {
    Arc::new(Mutex::new(SysSnapshot::default()))
}

/// RAM: total physical and used (total - available).
#[cfg(windows)]
fn sample_ram() -> RamStats {
    unsafe {
        let mut ms: MEMORYSTATUSEX = core::mem::zeroed();
        ms.dwLength = core::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut ms).is_ok() {
            let mb = 1024 * 1024;
            RamStats {
                total_mb: (ms.ullTotalPhys as u64) / mb,
                used_mb: (ms.ullTotalPhys - ms.ullAvailPhys) / mb,
            }
        } else {
            RamStats::default()
        }
    }
}

/// Linux: /proc/meminfo, values in kB. "available" is the kernel's
/// estimate of reclaimable memory — closest to Windows' AvailPhys.
#[cfg(not(windows))]
fn sample_ram() -> RamStats {
    let text = match std::fs::read_to_string("/proc/meminfo") {
        Ok(t) => t,
        Err(_) => return RamStats::default(),
    };
    let get = |key: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
            .unwrap_or(0)
    };
    let total_kb = get("MemTotal:");
    let avail_kb = get("MemAvailable:");
    RamStats {
        total_mb: total_kb / 1024,
        used_mb: total_kb.saturating_sub(avail_kb) / 1024,
    }
}

/// nvidia-smi CSV: one row per GPU, fields
/// name, clocks.sm, clocks.mem, temperature.gpu, power.draw,
/// utilization.gpu, memory.total, memory.used.
fn sample_gpus() -> Vec<GpuStats> {
    // Dev/test only: LLM_PROXY_FAKE_GPUS=N simulates N GPUs (a machine
    // without an NVIDIA card would report nothing). Inert unless the
    // variable is set, so the shipped binary is unaffected.
    if let Some(n) = std::env::var("LLM_PROXY_FAKE_GPUS").ok().and_then(|v| v.trim().parse().ok()) {
        if n > 0 {
            return (0..n)
                .map(|i| GpuStats {
                    name: format!("FAKE RTX 5060 Ti #{i}"),
                    core_clock_mhz: 2400 + 100 * i as u32,
                    mem_clock_mhz: 13801,
                    temp_c: 45 + 5 * i as u32,
                    power_w: 60.0 + 10.0 * i as f64,
                    vram_total_mb: 16311,
                    vram_used_mb: 12000 + 500 * i as u32,
                    util_pct: 30 + 15 * i as u32,
                })
                .collect();
        }
    }
    let mut cmd = std::process::Command::new("nvidia-smi");
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: the proxy is a WINDOWS-subsystem exe; without
        // this flag every spawn would allocate a console (a window
        // flashing on every poll).
        cmd.creation_flags(0x0800_0000);
    }
    let out = match cmd
        .args([
            "--query-gpu=name,clocks.sm,clocks.mem,temperature.gpu,power.draw,utilization.gpu,memory.total,memory.used",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_gpu_csv(&String::from_utf8_lossy(&out.stdout))
}

/// All rows of an `nvidia-smi --format=csv,noheader,nounits` output
/// (one per GPU); blank rows are skipped.
fn parse_gpu_csv(text: &str) -> Vec<GpuStats> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(parse_gpu_row)
        .collect()
}

/// One GPU row; returns None for malformed or nameless rows.
fn parse_gpu_row(line: &str) -> Option<GpuStats> {
    let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
    if f.len() < 8 || f[0].is_empty() {
        return None;
    }
    // Individual N/A fields degrade to 0, not "no GPU".
    let n = |s: &str| s.parse::<f64>().unwrap_or(0.0);
    Some(GpuStats {
        name: f[0].to_string(),
        core_clock_mhz: n(f[1]) as u32,
        mem_clock_mhz: n(f[2]) as u32,
        temp_c: n(f[3]) as u32,
        power_w: n(f[4]),
        util_pct: n(f[5]) as u32,
        vram_total_mb: n(f[6]) as u32,
        vram_used_mb: n(f[7]) as u32,
    })
}

/// CPU model name, read once (static info). Empty on failure.
#[cfg(windows)]
fn cpu_name() -> String {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_LOCAL_MACHINE, KEY_READ, REG_VALUE_TYPE,
    };
    unsafe {
        let path = HSTRING::from("HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0");
        let valname = HSTRING::from("ProcessorNameString");
        let mut hkey: windows::Win32::System::Registry::HKEY = core::mem::zeroed();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, &path, None, KEY_READ, &mut hkey).is_err() {
            return String::new();
        }
        // First call: size query (REG_SZ = UTF-16 incl. null terminator).
        let mut len = 0u32;
        let name = if RegQueryValueExW(hkey, &valname, None, None, None, Some(&mut len))
            .is_ok()
            && len >= 2
        {
            let mut buf = vec![0u8; len as usize];
            let mut vtype = REG_VALUE_TYPE(0);
            if RegQueryValueExW(
                hkey, &valname, None,
                Some(&mut vtype),
                Some(buf.as_mut_ptr()),
                Some(&mut len),
            )
            .is_ok()
            {
                let wide: Vec<u16> = buf[..len as usize]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16_lossy(&wide).trim_end_matches('\0').to_string()
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        let _ = RegCloseKey(hkey);
        name
    }
}

/// Linux: the "model name" field of /proc/cpuinfo (first occurrence).
#[cfg(not(windows))]
fn cpu_name() -> String {
    let text = match std::fs::read_to_string("/proc/cpuinfo") {
        Ok(t) => t,
        Err(_) => return String::new(),
    };
    text.lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
        .unwrap_or_default()
}

/// Main loop: one full sample every 2 s.
pub async fn run(snap: Arc<Mutex<SysSnapshot>>) {
    let mut prev_cpu = None;
    let cpu_name = cpu_name();
    loop {
        let gpus = tokio::task::spawn_blocking(sample_gpus)
            .await
            .unwrap_or_default();
        let ram = tokio::task::spawn_blocking(sample_ram)
            .await
            .unwrap_or_default();
        // The CPU counter read is instant — no need to offload it.
        let cpu_pct = match system_times() {
            Some(c) => {
                let p = pct_between(prev_cpu, c);
                prev_cpu = Some(c);
                p
            }
            None => 0,
        };

        *snap.lock().unwrap() = SysSnapshot { cpu_pct, cpu_name: cpu_name.clone(), gpus, ram };
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_pct_full_and_idle() {
        // 10% idle of a 100-tick delta -> 90% usage.
        assert_eq!(pct_between(Some((0, 0)), (10, 100)), 90);
        // No busy ticks at all -> 0%.
        assert_eq!(pct_between(Some((0, 0)), (100, 100)), 0);
        // No baseline or no time passed -> 0%.
        assert_eq!(pct_between(None, (10, 100)), 0);
        assert_eq!(pct_between(Some((0, 0)), (0, 0)), 0);
        // Counters never go backwards.
        assert_eq!(pct_between(Some((100, 100)), (10, 10)), 0);
    }

    fn row(name: &str, clk: &str, mclk: &str, temp: &str, pw: &str,
           util: &str, vt: &str, vu: &str) -> String {
        format!("{},{},{},{},{},{},{},{}", name, clk, mclk, temp, pw, util, vt, vu)
    }

    #[test]
    fn parses_single_gpu_row() {
        let g = parse_gpu_row(&row("NVIDIA GeForce RTX 5060 Ti", "2610", "13801",
            "66", "153.74", "98", "16311", "15106")).unwrap();
        assert_eq!(g.name, "NVIDIA GeForce RTX 5060 Ti");
        assert_eq!(g.core_clock_mhz, 2610);
        assert_eq!(g.temp_c, 66);
        assert_eq!(g.util_pct, 98);
        assert_eq!(g.vram_total_mb, 16311);
        assert!((g.power_w - 153.74).abs() < 1e-9);
    }

    #[test]
    fn parses_multi_gpu_csv() {
        let csv = [
            row("GPU A", "2610", "13801", "66", "153.74", "98", "16311", "15106"),
            row("GPU B", "2400", "13801", "71", "180.00", "12", "16311", "8200"),
        ]
        .join("\n");
        let g = parse_gpu_csv(&csv);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].name, "GPU A");
        assert_eq!(g[1].name, "GPU B");
        assert_eq!(g[1].util_pct, 12);
        assert_eq!(g[1].vram_used_mb, 8200);
    }

    #[test]
    fn skips_blank_and_nameless_rows() {
        let csv = format!("\n\n{},\n\n", row("GPU A", "1", "1", "1", "1", "1", "1", "1"));
        assert_eq!(parse_gpu_csv(&csv).len(), 1);
        assert_eq!(parse_gpu_csv("").len(), 0);
        assert_eq!(parse_gpu_row("N/A,1,1,1,1,1,1,1").unwrap().name, "N/A");
        assert!(parse_gpu_row("1,1,1,1,1,1,1").is_none()); // 7 fields
    }

    #[test]
    fn na_fields_degrade_to_zero() {
        let g = parse_gpu_row(&row("GPU A", "N/A", "13801", "N/A", "N/A",
            "0", "16311", "N/A")).unwrap();
        assert_eq!(g.core_clock_mhz, 0);
        assert_eq!(g.temp_c, 0);
        assert_eq!(g.power_w, 0.0);
        assert_eq!(g.vram_used_mb, 0);
        assert_eq!(g.vram_total_mb, 16311); // valid fields survive
    }

    #[test]
    #[cfg(not(windows))]
    fn linux_proc_sources_and_ram() {
        assert!(std::path::Path::new("/proc/stat").exists());
        assert!(std::path::Path::new("/proc/meminfo").exists());
        let ram = sample_ram();
        assert!(ram.total_mb > 1024); // any real machine has > 1 GB
        assert!(!cpu_name().is_empty());
    }
}
