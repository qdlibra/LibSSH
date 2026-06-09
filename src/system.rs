//! Lightweight poller for local machine stats (CPU / memory / network).

use std::time::Duration;

use sysinfo::{Disks, Networks, System};

/// Snapshot passed to the UI each tick.
#[derive(Debug, Clone, Default)]
pub struct SystemSnapshot {
    pub cpu_percent: f32,
    pub mem_percent: f32,
    pub swap_percent: f32,
    pub mem_used_mib: u64,
    pub mem_total_mib: u64,
    pub swap_used_mib: u64,
    pub swap_total_mib: u64,
    pub net_bytes_per_sec: u64,
    pub net_rx_per_sec: u64,
    pub net_tx_per_sec: u64,
    /// Per-filesystem (mount, available_bytes, total_bytes).
    pub disks: Vec<(String, u64, u64)>,
}

/// Stateful sampler. Construct once per process and poll via [`Self::sample`].
pub struct SystemSampler {
    sys: System,
    nets: Networks,
    disks: Disks,
    last_rx_total: u64,
    last_tx_total: u64,
    last_instant: std::time::Instant,
}

impl SystemSampler {
    pub fn new() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();
        let nets = Networks::new_with_refreshed_list();
        let last_rx_total = nets.iter().map(|(_, d)| d.total_received()).sum();
        let last_tx_total = nets.iter().map(|(_, d)| d.total_transmitted()).sum();
        let disks = Disks::new_with_refreshed_list();
        Self {
            sys,
            nets,
            disks,
            last_rx_total,
            last_tx_total,
            last_instant: std::time::Instant::now(),
        }
    }

    /// Recommended poll interval for a UI sidebar.
    pub fn recommended_interval() -> Duration {
        Duration::from_millis(1000)
    }

    pub fn sample(&mut self) -> SystemSnapshot {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.nets.refresh(true);

        let cpu_percent = self.sys.global_cpu_usage() / 100.0;

        let mem_total = self.sys.total_memory();
        let mem_used = self.sys.used_memory();
        let mem_percent = if mem_total > 0 {
            mem_used as f32 / mem_total as f32
        } else {
            0.0
        };

        let swap_total = self.sys.total_swap();
        let swap_used = self.sys.used_swap();
        let swap_percent = if swap_total > 0 {
            swap_used as f32 / swap_total as f32
        } else {
            0.0
        };

        let rx_total: u64 = self.nets.iter().map(|(_, d)| d.total_received()).sum();
        let tx_total: u64 = self.nets.iter().map(|(_, d)| d.total_transmitted()).sum();
        let now = std::time::Instant::now();
        let elapsed = now
            .duration_since(self.last_instant)
            .as_secs_f64()
            .max(0.001);
        let rx_delta = rx_total.saturating_sub(self.last_rx_total);
        let tx_delta = tx_total.saturating_sub(self.last_tx_total);
        self.last_rx_total = rx_total;
        self.last_tx_total = tx_total;
        self.last_instant = now;
        let net_rx_per_sec = (rx_delta as f64 / elapsed) as u64;
        let net_tx_per_sec = (tx_delta as f64 / elapsed) as u64;

        self.disks.refresh(true);
        let disks: Vec<(String, u64, u64)> = self
            .disks
            .iter()
            .map(|d| {
                (
                    d.mount_point().to_string_lossy().to_string(),
                    d.available_space(),
                    d.total_space(),
                )
            })
            .filter(|(_, _, total)| *total > 0)
            .collect();

        SystemSnapshot {
            cpu_percent,
            mem_percent,
            swap_percent,
            mem_used_mib: mem_used / 1024 / 1024,
            mem_total_mib: mem_total / 1024 / 1024,
            swap_used_mib: swap_used / 1024 / 1024,
            swap_total_mib: swap_total / 1024 / 1024,
            net_bytes_per_sec: net_rx_per_sec + net_tx_per_sec,
            net_rx_per_sec,
            net_tx_per_sec,
            disks,
        }
    }
}

/// Human-readable network throughput (e.g. `"1.2 MB/s"`).
pub fn format_bytes_per_sec(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B/s", "KB/s", "MB/s", "GB/s"];
    let mut value = bytes as f64;
    let mut idx = 0;
    while value >= 1024.0 && idx < UNITS.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{} {}", bytes, UNITS[idx])
    } else {
        format!("{:.1} {}", value, UNITS[idx])
    }
}

/// Detect whether the OS is currently using a dark appearance.
///
/// Returns `None` when the platform preference can't be determined, so the
/// caller can leave the current theme choice untouched rather than guessing.
pub fn detect_dark_mode() -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        // `defaults read -g AppleInterfaceStyle` prints "Dark" in dark mode and
        // exits non-zero (key absent) in light mode.
        let out = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .output()
            .ok()?;
        Some(
            out.status.success()
                && String::from_utf8_lossy(&out.stdout)
                    .to_lowercase()
                    .contains("dark"),
        )
    }
    #[cfg(target_os = "windows")]
    {
        // Registry: AppsUseLightTheme — 0x0 = dark, 0x1 = light.
        let out = std::process::Command::new("reg")
            .args([
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
                "/v",
                "AppsUseLightTheme",
            ])
            .output()
            .ok()?;
        parse_windows_apps_use_light_theme(&String::from_utf8_lossy(&out.stdout))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Freedesktop / GNOME: `gsettings get org.gnome.desktop.interface color-scheme`
        // → 'prefer-dark' | 'default' | 'prefer-light'. Fall back to the GTK theme
        // name when the color-scheme key is unavailable (older desktops).
        if let Ok(out) = std::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "color-scheme"])
            .output()
        {
            if out.status.success() {
                if let Some(dark) = parse_gnome_color_scheme(&String::from_utf8_lossy(&out.stdout)) {
                    return Some(dark);
                }
            }
        }
        if let Ok(out) = std::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "gtk-theme"])
            .output()
        {
            if out.status.success() {
                return Some(
                    String::from_utf8_lossy(&out.stdout)
                        .to_lowercase()
                        .contains("dark"),
                );
            }
        }
        None
    }
}

/// Parse `org.gnome.desktop.interface color-scheme`: `Some(true)` = dark,
/// `Some(false)` = light/default, `None` = unrecognised.
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn parse_gnome_color_scheme(value: &str) -> Option<bool> {
    let v = value.trim().trim_matches('\'').to_lowercase();
    if v.contains("dark") {
        Some(true)
    } else if v.contains("light") || v.contains("default") {
        Some(false)
    } else {
        None
    }
}

/// Parse `reg query … AppsUseLightTheme` output: `0x0` = dark, `0x1` = light.
#[cfg(any(test, target_os = "windows"))]
fn parse_windows_apps_use_light_theme(out: &str) -> Option<bool> {
    let idx = out.find("0x")?;
    let digit = out[idx + 2..].trim_start().chars().next()?;
    Some(digit == '0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_byte_rates() {
        assert_eq!(format_bytes_per_sec(512), "512 B/s");
        assert_eq!(format_bytes_per_sec(1536), "1.5 KB/s");
        assert_eq!(format_bytes_per_sec(1024 * 1024), "1.0 MB/s");
    }

    #[test]
    fn parses_gnome_color_scheme_values() {
        assert_eq!(parse_gnome_color_scheme("'prefer-dark'\n"), Some(true));
        assert_eq!(parse_gnome_color_scheme("'prefer-light'"), Some(false));
        assert_eq!(parse_gnome_color_scheme("'default'\n"), Some(false));
        assert_eq!(parse_gnome_color_scheme("'mystery'"), None);
    }

    #[test]
    fn parses_windows_theme_flag() {
        assert_eq!(
            parse_windows_apps_use_light_theme("    AppsUseLightTheme    REG_DWORD    0x0\r\n"),
            Some(true)
        );
        assert_eq!(
            parse_windows_apps_use_light_theme("    AppsUseLightTheme    REG_DWORD    0x1\r\n"),
            Some(false)
        );
        assert_eq!(parse_windows_apps_use_light_theme("nothing"), None);
    }
}
