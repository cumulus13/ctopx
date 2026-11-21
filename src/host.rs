//! Host metrics via sysinfo.

use crate::model::*;
use sysinfo::{Disks, System};

pub struct HostCollector {
    sys: System,
    disks: Disks,
    cpu_hist: std::collections::VecDeque<f32>,
    mem_hist: std::collections::VecDeque<f32>,
}

impl HostCollector {
    pub fn new() -> Self {
        let mut sys = System::new().expect("sysinfo: cannot initialise system info");
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        Self {
            sys,
            disks: Disks::new_with_refreshed_list().expect("sysinfo: cannot list disks"),
            cpu_hist: Default::default(),
            mem_hist: Default::default(),
        }
    }

    pub fn collect(&mut self) -> HostView {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.disks.refresh(true);
        let pct = |used: u64, total: u64| {
            if total > 0 {
                used as f64 / total as f64 * 100.0
            } else {
                0.0
            }
        };
        let mem_total = self.sys.total_memory();
        let mem_used = self.sys.used_memory();
        let swap_total = self.sys.total_swap();
        let swap_used = self.sys.used_swap();
        let load = System::load_average().unwrap_or(sysinfo::LoadAvg {
            one: 0.0,
            five: 0.0,
            fifteen: 0.0,
        });

        let mut seen = std::collections::HashSet::new();
        let mut disks = vec![];
        for d in self.disks.iter() {
            let fs = d.file_system().to_string_lossy().to_lowercase();
            if matches!(
                fs.as_str(),
                "tmpfs"
                    | "devtmpfs"
                    | "overlay"
                    | "squashfs"
                    | "proc"
                    | "sysfs"
                    | "cgroup"
                    | "cgroup2"
            ) {
                continue;
            }
            let total = d.total_space();
            if total == 0 {
                continue;
            }
            let mount = d.mount_point().to_string_lossy().to_string();
            if !seen.insert(mount.clone()) {
                continue;
            }
            let used = total.saturating_sub(d.available_space());
            disks.push(DiskView {
                mount,
                total,
                used,
                percent: pct(used, total),
            });
        }
        let disk_percent_max = disks.iter().map(|d| d.percent).fold(0.0, f64::max);

        let cpu = self.sys.global_cpu_usage() as f64;
        let mem = pct(mem_used, mem_total);
        for (q, v) in [
            (&mut self.cpu_hist, cpu as f32),
            (&mut self.mem_hist, mem as f32),
        ] {
            if q.len() >= HISTORY_LEN {
                q.pop_front();
            }
            q.push_back(v);
        }
        HostView {
            hostname: System::host_name().unwrap_or_default(),
            os: System::long_os_version()
                .or_else(|_| System::name())
                .unwrap_or_default(),
            cpus: self.sys.cpus().len(),
            uptime_secs: System::uptime().unwrap_or(0),
            cpu_percent: cpu,
            mem_total,
            mem_used,
            mem_percent: mem,
            swap_total,
            swap_used,
            swap_percent: pct(swap_used, swap_total),
            load1: load.one,
            load5: load.five,
            load15: load.fifteen,
            disks,
            disk_percent_max,
            cpu_hist: self.cpu_hist.clone(),
            mem_hist: self.mem_hist.clone(),
        }
    }
}
