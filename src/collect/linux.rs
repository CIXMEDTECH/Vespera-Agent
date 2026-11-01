use std::ffi::CString;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use vespera_proto::{BasicInfo, Report};

use super::procfs::{self, CpuTimes, Mount, ProcFile};
use super::{Filters, now_ms, sysinfo};

const MOUNT_REFRESH: Duration = Duration::from_secs(60);
const DISK_REFRESH: Duration = Duration::from_secs(10);

/// Host facts that cannot change while the agent runs.
struct Host {
    cpu_name: String,
    os: String,
    kernel: String,
    virtualization: String,
}

pub struct Collector {
    filters: Filters,
    host: Host,
    stat: ProcFile,
    meminfo: ProcFile,
    netdev: ProcFile,
    loadavg: ProcFile,
    uptime: ProcFile,
    sockstat: Option<ProcFile>,
    sockstat6: Option<ProcFile>,
    prev_cpu: Option<CpuTimes>,
    prev_net: Option<(u64, u64, Instant)>,
    disks: Vec<Mount>,
    disks_at: Option<Instant>,
    disk_usage: (u64, u64),
    disk_at: Option<Instant>,
}

impl Collector {
    pub fn new(filters: Filters) -> Result<Self> {
        let mut c = Self {
            filters,
            host: Host {
                cpu_name: sysinfo::cpu_name(),
                os: sysinfo::os_name(),
                kernel: sysinfo::kernel_version(),
                virtualization: sysinfo::virtualization(),
            },
            stat: ProcFile::open("/proc/stat").context("open /proc/stat")?,
            meminfo: ProcFile::open("/proc/meminfo").context("open /proc/meminfo")?,
            netdev: ProcFile::open("/proc/net/dev").context("open /proc/net/dev")?,
            loadavg: ProcFile::open("/proc/loadavg").context("open /proc/loadavg")?,
            uptime: ProcFile::open("/proc/uptime").context("open /proc/uptime")?,
            sockstat: ProcFile::open("/proc/net/sockstat").ok(),
            sockstat6: ProcFile::open("/proc/net/sockstat6").ok(),
            prev_cpu: None,
            prev_net: None,
            disks: Vec::new(),
            disks_at: None,
            disk_usage: (0, 0),
            disk_at: None,
        };
        // Prime the delta-based counters so the first report is meaningful.
        c.cpu();
        c.net();
        Ok(c)
    }

    pub fn basic_info(&mut self) -> BasicInfo {
        let mem = self.meminfo.read().map(procfs::parse_meminfo).unwrap_or_default();
        let (_, disk_total) = self.disk();
        BasicInfo {
            cpu_name: self.host.cpu_name.clone(),
            cpu_cores: sysinfo::cpu_cores(),
            arch: std::env::consts::ARCH.to_string(),
            os: self.host.os.clone(),
            kernel: self.host.kernel.clone(),
            virtualization: self.host.virtualization.clone(),
            mem_total: mem.total,
            swap_total: mem.swap_total,
            disk_total,
            ipv4: None,
            ipv6: None,
            boot_time: self.stat.read().ok().and_then(procfs::parse_btime).unwrap_or(0),
        }
    }

    pub fn report(&mut self) -> Report {
        let cpu = self.cpu();
        let mem = self.meminfo.read().map(procfs::parse_meminfo).unwrap_or_default();
        let (disk_used, disk_total) = self.disk();
        let (net_rx, net_tx, net_rx_total, net_tx_total) = self.net();
        let (load1, load5, load15) = self.loadavg.read().map(procfs::parse_loadavg).unwrap_or_default();
        let uptime = self.uptime.read().map(procfs::parse_uptime).unwrap_or(0);
        let (mut tcp, mut udp) = (0, 0);
        for f in [self.sockstat.as_mut(), self.sockstat6.as_mut()].into_iter().flatten() {
            if let Ok(s) = f.read() {
                let (t, u) = procfs::parse_sockstat(s);
                tcp += t;
                udp += u;
            }
        }
        Report {
            ts_ms: now_ms(),
            cpu,
            mem_used: mem.used,
            mem_total: mem.total,
            swap_used: mem.swap_used,
            swap_total: mem.swap_total,
            disk_used,
            disk_total,
            net_rx,
            net_tx,
            net_rx_total,
            net_tx_total,
            load1,
            load5,
            load15,
            tcp,
            udp,
            procs: count_procs(),
            uptime,
        }
    }

    fn cpu(&mut self) -> f32 {
        let Some(cur) = self.stat.read().ok().and_then(procfs::parse_cpu_stat) else { return 0.0 };
        let usage = self.prev_cpu.map(|p| procfs::cpu_usage(p, cur)).unwrap_or(0.0);
        self.prev_cpu = Some(cur);
        usage
    }

    fn net(&mut self) -> (u64, u64, u64, u64) {
        let filters = &self.filters;
        let Ok(s) = self.netdev.read() else { return (0, 0, 0, 0) };
        let (rx, tx) = procfs::parse_net_dev(s, |n| filters.nic_allowed(n));
        let now = Instant::now();
        let speed = match self.prev_net {
            Some((prx, ptx, at)) => {
                let secs = now.duration_since(at).as_secs_f64().max(0.001);
                ((rx.saturating_sub(prx) as f64 / secs) as u64, (tx.saturating_sub(ptx) as f64 / secs) as u64)
            }
            None => (0, 0),
        };
        self.prev_net = Some((rx, tx, now));
        (speed.0, speed.1, rx, tx)
    }

    fn disk(&mut self) -> (u64, u64) {
        let now = Instant::now();
        if self.disks_at.is_none_or(|t| now.duration_since(t) >= MOUNT_REFRESH) {
            if let Ok(s) = std::fs::read_to_string("/proc/self/mountinfo") {
                self.disks = procfs::select_disks(&procfs::parse_mountinfo(&s), &self.filters.mount_include);
            }
            self.disks_at = Some(now);
            self.disk_at = None;
        }
        if self.disk_at.is_none_or(|t| now.duration_since(t) >= DISK_REFRESH) {
            let (mut used, mut total) = (0u64, 0u64);
            for m in &self.disks {
                if let Some((u, t)) = statvfs(&m.mount_point) {
                    used += u;
                    total += t;
                }
            }
            self.disk_usage = (used, total);
            self.disk_at = Some(now);
        }
        self.disk_usage
    }
}

fn statvfs(path: &str) -> Option<(u64, u64)> {
    let c = CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a valid out pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    let total = st.f_blocks as u64 * frsize;
    let free = st.f_bfree as u64 * frsize;
    Some((total.saturating_sub(free), total))
}

fn count_procs() -> u32 {
    let Ok(rd) = std::fs::read_dir("/proc") else { return 0 };
    rd.filter_map(|e| e.ok()).filter(|e| e.file_name().as_encoded_bytes().iter().all(u8::is_ascii_digit)).count() as u32
}
