//! 版本更新检测 + macOS 自动下载安装。
//! 纯逻辑（版本比较、asset 选择、校验、安装编排）拆成可单测的函数，
//! 外层是薄薄的异步网络 / 系统调用壳。第一版仅 macOS 实现安装。
#![allow(dead_code)] // 含平台分支：非 macOS 下部分 helper 天然未被调用。

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// 更新源仓库（留成常量，便于将来加"官网兜底"provider）。
const REPO: &str = "qdlibra/LibSSH";
/// GitHub 要求所有 API 请求带 User-Agent。
const USER_AGENT: &str = concat!("LibSSH/", env!("CARGO_PKG_VERSION"));

/// GitHub Releases API 响应（只取需要的字段）。
#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// 一个可供下载安装的新版本。
#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub version: semver::Version,
    pub tag: String,
    pub notes: String,
    pub asset_url: String,
    pub asset_name: String,
    pub asset_size: u64,
    pub checksums_url: Option<String>,
}

/// 安装结果。
#[derive(Debug, Clone)]
pub enum InstallOutcome {
    /// 方案 B：辅助脚本已就绪，等用户点重启再执行。
    ReadyToRestart { helper_script: PathBuf },
    /// 方案 A：已打开 dmg，提示用户手动拖拽。
    GuidedManual,
}

/// 去掉首部可选的 'v' 前缀和首尾空白。
fn normalize_tag(tag: &str) -> &str {
    tag.trim().trim_start_matches('v')
}

/// candidate_tag 是否比 current 新（语义化版本比较）。
fn is_newer(current: &str, candidate_tag: &str) -> Result<bool> {
    let cur = semver::Version::parse(normalize_tag(current))
        .with_context(|| format!("invalid current version: {current}"))?;
    let cand = semver::Version::parse(normalize_tag(candidate_tag))
        .with_context(|| format!("invalid release tag: {candidate_tag}"))?;
    Ok(cand > cur)
}

/// 运行时架构 → release 产物命名里的架构标签。
fn arch_tag(arch: &str) -> Option<&'static str> {
    match arch {
        "aarch64" => Some("arm64"),
        "x86_64" => Some("x86_64"),
        _ => None,
    }
}

/// 当前编译目标的架构标签。
fn target_arch_tag() -> Option<&'static str> {
    arch_tag(std::env::consts::ARCH)
}

/// 选出当前架构对应的 macOS dmg。
fn pick_asset<'a>(assets: &'a [GhAsset], arch_tag: &str) -> Option<&'a GhAsset> {
    let needle = format!("LibSSH-macos-{arch_tag}.dmg");
    assets.iter().find(|a| a.name == needle)
}

/// 找到 checksums.txt 的下载直链。
fn checksums_asset_url(assets: &[GhAsset]) -> Option<String> {
    assets
        .iter()
        .find(|a| a.name == "checksums.txt")
        .map(|a| a.browser_download_url.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_compares_semver_ignoring_v_prefix() {
        assert!(is_newer("0.2.3", "v0.2.4").unwrap());
        assert!(is_newer("v0.2.3", "0.3.0").unwrap());
        assert!(!is_newer("0.2.3", "0.2.3").unwrap());
        assert!(!is_newer("0.2.3", "v0.2.2").unwrap());
        assert!(is_newer("0.2.3", "v0.2.4-beta.1").unwrap());
        assert!(!is_newer("0.2.4", "v0.2.4-beta.1").unwrap());
        assert!(is_newer("0.2.3", "vbogus").is_err());
    }

    fn asset(name: &str) -> GhAsset {
        GhAsset { name: name.into(), browser_download_url: format!("https://x/{name}"), size: 1 }
    }

    #[test]
    fn arch_tag_maps_known_arches() {
        assert_eq!(arch_tag("aarch64"), Some("arm64"));
        assert_eq!(arch_tag("x86_64"), Some("x86_64"));
        assert_eq!(arch_tag("powerpc"), None);
    }

    #[test]
    fn pick_asset_matches_macos_dmg_for_arch() {
        let assets = vec![
            asset("LibSSH-macos-arm64.dmg"),
            asset("LibSSH-macos-x86_64.dmg"),
            asset("LibSSH-windows-x86_64.exe"),
            asset("checksums.txt"),
        ];
        assert_eq!(pick_asset(&assets, "arm64").unwrap().name, "LibSSH-macos-arm64.dmg");
        assert_eq!(pick_asset(&assets, "x86_64").unwrap().name, "LibSSH-macos-x86_64.dmg");
        assert!(pick_asset(&assets, "riscv").is_none());
    }

    #[test]
    fn checksums_asset_url_finds_checksums_txt() {
        let assets = vec![asset("LibSSH-macos-arm64.dmg"), asset("checksums.txt")];
        assert_eq!(checksums_asset_url(&assets).as_deref(), Some("https://x/checksums.txt"));
        assert!(checksums_asset_url(&[asset("LibSSH-macos-arm64.dmg")]).is_none());
    }
}
