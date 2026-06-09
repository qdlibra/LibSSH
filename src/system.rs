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

/// 全局 CLI 符号链接管理（仅 Unix：macOS / Linux）。
#[cfg(unix)]
pub use cli_link::{
    cli_link_status, disable_cli_link, enable_cli_link, local_bin_in_path, CliLinkOutcome,
    CliLinkStatus,
};

#[cfg(unix)]
mod cli_link {
    use anyhow::{anyhow, Context, Result};
    use std::path::{Path, PathBuf};

    /// 链接当前状态。判别值与 ui/app.slint 的 `cli-link-state` 约定一致：
    /// 0=未链接 1=已链接 2=失效/被占用。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CliLinkStatus {
        NotLinked = 0,
        Linked = 1,
        Stale = 2,
    }

    /// 建链结果，用于 UI 反馈。
    pub struct CliLinkOutcome {
        pub link_path: PathBuf,
        pub in_path: bool,
    }

    fn home() -> Result<PathBuf> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME 环境变量未设置"))
    }
    fn local_bin() -> Result<PathBuf> {
        Ok(home()?.join(".local/bin"))
    }
    fn link_path() -> Result<PathBuf> {
        Ok(local_bin()?.join("LibSSH"))
    }

    /// 检测 `~/.local/bin/LibSSH` 当前状态。任何解析失败都保守地按未链接处理。
    pub fn cli_link_status() -> CliLinkStatus {
        let (Ok(lp), Ok(exe)) = (link_path(), std::env::current_exe()) else {
            return CliLinkStatus::NotLinked;
        };
        link_status_in(&lp, &exe)
    }

    /// 建立/重建指向当前二进制的符号链接。
    pub fn enable_cli_link() -> Result<CliLinkOutcome> {
        let dir = local_bin()?;
        let lp = link_path()?;
        let exe = std::env::current_exe().context("无法定位当前可执行文件")?;
        std::fs::create_dir_all(&dir).with_context(|| format!("无法创建 {}", dir.display()))?;
        enable_link_at(&lp, &exe)?;
        Ok(CliLinkOutcome {
            link_path: lp,
            in_path: local_bin_in_path(),
        })
    }

    /// 移除我们建立的符号链接。
    pub fn disable_cli_link() -> Result<()> {
        let lp = link_path()?;
        disable_link_at(&lp)
    }

    /// `~/.local/bin` 是否在 PATH（仅用于提示；GUI 继承的 PATH 可能不全）。
    pub fn local_bin_in_path() -> bool {
        let Ok(dir) = local_bin() else {
            return false;
        };
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|e| e == dir))
    }

    // ---- 纯函数（注入路径，便于单测）----

    fn link_status_in(link_path: &Path, current_exe: &Path) -> CliLinkStatus {
        let Ok(meta) = std::fs::symlink_metadata(link_path) else {
            return CliLinkStatus::NotLinked;
        };
        if !meta.file_type().is_symlink() {
            // 同名普通文件占位：视作需要用户处理。
            return CliLinkStatus::Stale;
        }
        match std::fs::read_link(link_path) {
            Ok(target) => {
                let a = std::fs::canonicalize(&target).unwrap_or(target);
                let b = std::fs::canonicalize(current_exe)
                    .unwrap_or_else(|_| current_exe.to_path_buf());
                if a == b {
                    CliLinkStatus::Linked
                } else {
                    CliLinkStatus::Stale
                }
            }
            Err(_) => CliLinkStatus::Stale,
        }
    }

    fn enable_link_at(link_path: &Path, current_exe: &Path) -> Result<()> {
        let parent = link_path
            .parent()
            .ok_or_else(|| anyhow!("链接路径没有父目录"))?;
        // 原子替换：先建临时链接再 rename 覆盖，避免半成品。
        let tmp = parent.join(format!(".LibSSH.tmp-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(current_exe, &tmp)
            .with_context(|| format!("无法创建符号链接 {}", tmp.display()))?;
        std::fs::rename(&tmp, link_path)
            .with_context(|| format!("无法替换 {}", link_path.display()))?;
        Ok(())
    }

    fn disable_link_at(link_path: &Path) -> Result<()> {
        let Ok(meta) = std::fs::symlink_metadata(link_path) else {
            return Ok(()); // 本就不存在，视作已移除。
        };
        if !meta.file_type().is_symlink() {
            return Err(anyhow!(
                "{} 不是符号链接，已跳过删除以防误删",
                link_path.display()
            ));
        }
        if link_path.file_name().and_then(|s| s.to_str()) != Some("LibSSH") {
            return Err(anyhow!("拒绝删除非 LibSSH 链接：{}", link_path.display()));
        }
        std::fs::remove_file(link_path).with_context(|| format!("无法删除 {}", link_path.display()))
    }

    #[cfg(test)]
    mod tests {
        use super::{disable_link_at, enable_link_at, link_status_in, CliLinkStatus};
        use std::path::{Path, PathBuf};

        // 每个测试用独立临时目录，避免并行冲突；用例名作为 tag。
        fn temp_dir(tag: &str) -> PathBuf {
            let d = std::env::temp_dir()
                .join(format!("libssh-clilink-{}-{}", tag, std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        // 造一个"真实存在"的假二进制，canonicalize 才能解析。
        fn fake_exe(dir: &Path, name: &str) -> PathBuf {
            let p = dir.join(name);
            std::fs::write(&p, b"#!/bin/sh\n").unwrap();
            p
        }

        #[test]
        fn empty_dir_is_not_linked() {
            let d = temp_dir("empty");
            let exe = fake_exe(&d, "LibSSH-bin");
            let link = d.join("LibSSH");
            assert_eq!(link_status_in(&link, &exe), CliLinkStatus::NotLinked);
        }

        #[test]
        fn enable_then_status_is_linked() {
            let d = temp_dir("enable");
            let exe = fake_exe(&d, "LibSSH-bin");
            let link = d.join("LibSSH");
            enable_link_at(&link, &exe).unwrap();
            assert_eq!(link_status_in(&link, &exe), CliLinkStatus::Linked);
        }

        #[test]
        fn link_to_other_target_is_stale_then_relinkable() {
            let d = temp_dir("stale");
            let exe = fake_exe(&d, "LibSSH-bin");
            let other = fake_exe(&d, "other-bin");
            let link = d.join("LibSSH");
            // 先指向 other -> 相对当前 exe 应为 Stale
            std::os::unix::fs::symlink(&other, &link).unwrap();
            assert_eq!(link_status_in(&link, &exe), CliLinkStatus::Stale);
            // 重链覆盖 -> Linked
            enable_link_at(&link, &exe).unwrap();
            assert_eq!(link_status_in(&link, &exe), CliLinkStatus::Linked);
        }

        #[test]
        fn disable_removes_our_link() {
            let d = temp_dir("disable");
            let exe = fake_exe(&d, "LibSSH-bin");
            let link = d.join("LibSSH");
            enable_link_at(&link, &exe).unwrap();
            disable_link_at(&link).unwrap();
            assert_eq!(link_status_in(&link, &exe), CliLinkStatus::NotLinked);
        }

        #[test]
        fn disable_refuses_plain_file() {
            let d = temp_dir("plainfile");
            let link = d.join("LibSSH");
            std::fs::write(&link, b"not a symlink").unwrap(); // 普通文件占位
            let err = disable_link_at(&link).unwrap_err();
            assert!(err.to_string().contains("不是符号链接"));
            assert!(link.exists(), "普通文件不能被删除");
        }
    }
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
