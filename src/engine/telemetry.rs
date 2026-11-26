//! Hardware Telemetry Collector (`/sys/class/power_supply`, `/proc/meminfo`, `/sys/class/thermal`).

use std::fs;
use std::path::Path;

use crate::engine::context::{read_battery_from_dir, BatteryInfo};

/// Point-in-time hardware telemetry snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct HardwareSnapshot {
    pub battery: Option<BatteryInfo>,
    pub ram_used_pct: Option<u8>,
    pub ram_used_mib: Option<u64>,
    pub ram_total_mib: Option<u64>,
    pub cpu_temp_c: Option<f32>,
}

impl HardwareSnapshot {
    pub fn collect() -> Self {
        Self::collect_from_paths(
            Path::new("/sys/class/power_supply"),
            Path::new("/proc/meminfo"),
            Path::new("/sys/class/thermal"),
        )
    }

    pub fn collect_from_paths(
        power_supply_dir: &Path,
        meminfo_path: &Path,
        thermal_dir: &Path,
    ) -> Self {
        let battery = read_battery_from_dir(power_supply_dir);
        let (ram_used_pct, ram_used_mib, ram_total_mib) = match read_ram_usage(meminfo_path) {
            Some((pct, used, total)) => (Some(pct), Some(used), Some(total)),
            None => (None, None, None),
        };
        let cpu_temp_c = read_max_cpu_temp(thermal_dir);

        Self {
            battery,
            ram_used_pct,
            ram_used_mib,
            ram_total_mib,
            cpu_temp_c,
        }
    }

    pub fn format_lines(&self) -> Vec<String> {
        let bat = self
            .battery
            .as_ref()
            .map(BatteryInfo::display)
            .unwrap_or_else(|| "tidak tersedia (Desktop/AC)".to_string());
        let ram = match (self.ram_used_pct, self.ram_used_mib, self.ram_total_mib) {
            (Some(pct), Some(used), Some(total)) => {
                format!("{pct}% ({used} MiB / {total} MiB)")
            }
            (Some(pct), _, _) => format!("{pct}%"),
            _ => "tidak terbaca".to_string(),
        };
        let temp = self
            .cpu_temp_c
            .map(|t| format!("{t:.1} C"))
            .unwrap_or_else(|| "tidak terbaca".to_string());

        vec![
            format!("Baterai  : {bat}"),
            format!("RAM      : {ram}"),
            format!("Suhu CPU : {temp}"),
        ]
    }
}

pub fn read_ram_usage(meminfo_path: &Path) -> Option<(u8, u64, u64)> {
    let raw = fs::read_to_string(meminfo_path).ok()?;
    parse_proc_meminfo(&raw)
}

pub fn parse_proc_meminfo(raw: &str) -> Option<(u8, u64, u64)> {
    let mut mem_total_kb: Option<u64> = None;
    let mut mem_available_kb: Option<u64> = None;
    let mut mem_free_kb: Option<u64> = None;

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            mem_total_kb = rest.split_whitespace().next()?.parse::<u64>().ok();
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            mem_available_kb = rest.split_whitespace().next()?.parse::<u64>().ok();
        } else if let Some(rest) = line.strip_prefix("MemFree:") {
            mem_free_kb = rest.split_whitespace().next()?.parse::<u64>().ok();
        }
    }

    let total_kb = mem_total_kb.filter(|&t| t > 0)?;
    let avail_kb = mem_available_kb.or(mem_free_kb)?;
    let used_kb = total_kb.saturating_sub(avail_kb);
    let pct = ((used_kb as f64 / total_kb as f64) * 100.0)
        .round()
        .clamp(0.0, 100.0) as u8;

    Some((pct, used_kb / 1024, total_kb / 1024))
}

pub fn read_max_cpu_temp(thermal_dir: &Path) -> Option<f32> {
    let entries = fs::read_dir(thermal_dir).ok()?;
    let mut max_temp: Option<f32> = None;

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let temp_file = entry.path().join("temp");
        let Ok(raw) = fs::read_to_string(temp_file) else {
            continue;
        };
        let Ok(milli_c) = raw.trim().parse::<i64>() else {
            continue;
        };
        if !(1..=150_000).contains(&milli_c) {
            continue;
        }
        let celsius = milli_c as f32 / 1000.0;
        max_temp = Some(match max_temp {
            Some(prev) => prev.max(celsius),
            None => celsius,
        });
    }

    max_temp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_meminfo_percentage_accurately() {
        let sample = "\
MemTotal:       16384000 kB
MemFree:         2000000 kB
MemAvailable:    1638400 kB
Buffers:          500000 kB
";
        let (pct, used_mib, total_mib) = parse_proc_meminfo(sample).unwrap();
        assert_eq!(pct, 90);
        assert_eq!(total_mib, 16000);
        assert_eq!(used_mib, 14400);
    }

    #[test]
    fn collects_mock_battery_ram_and_thermal_zones() {
        let root = std::env::temp_dir().join(format!("waish-tele-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let power = root.join("power_supply");
        let bat0 = power.join("BAT0");
        fs::create_dir_all(&bat0).unwrap();
        fs::write(bat0.join("capacity"), "17\n").unwrap();
        fs::write(bat0.join("status"), "Discharging\n").unwrap();

        let meminfo = root.join("meminfo");
        fs::write(&meminfo, "MemTotal: 10000000 kB\nMemAvailable: 500000 kB\n").unwrap();

        let thermal = root.join("thermal");
        let z0 = thermal.join("thermal_zone0");
        let z1 = thermal.join("thermal_zone1");
        fs::create_dir_all(&z0).unwrap();
        fs::create_dir_all(&z1).unwrap();
        fs::write(z0.join("temp"), "62000\n").unwrap();
        fs::write(z1.join("temp"), "88500\n").unwrap();

        let snap = HardwareSnapshot::collect_from_paths(&power, &meminfo, &thermal);
        assert_eq!(snap.battery.as_ref().unwrap().capacity, 17);
        assert_eq!(snap.battery.as_ref().unwrap().status, "Discharging");
        assert_eq!(snap.ram_used_pct, Some(95));
        assert!((snap.cpu_temp_c.unwrap() - 88.5).abs() < 0.01);

        let _ = fs::remove_dir_all(&root);
    }
}
