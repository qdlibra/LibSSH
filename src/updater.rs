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

/// 从 `sha256sum` 格式的 checksums.txt 取某文件的期望哈希（按 basename 匹配）。
/// 行格式：`<hex>  <filename>`，filename 可能带 `*`（二进制模式）或路径前缀。
fn parse_checksums(text: &str, asset_name: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let hash = it.next()?;
        let name = it.next()?;
        let name = name.trim_start_matches('*');
        let base = name.rsplit('/').next().unwrap_or(name);
        if base == asset_name {
            return Some(hash.to_lowercase());
        }
    }
    None
}

/// 字节流的 SHA256，返回小写十六进制串。
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 反序列化 GitHub release JSON。
fn parse_release(json: &str) -> Result<GhRelease> {
    serde_json::from_str(json).context("failed to parse GitHub release JSON")
}

/// 从 release + 目标架构组装 ReleaseInfo（不做版本新旧判断）。
fn select_release_info(rel: &GhRelease, arch_tag: &str) -> Option<ReleaseInfo> {
    let asset = pick_asset(&rel.assets, arch_tag)?;
    let version = semver::Version::parse(normalize_tag(&rel.tag_name)).ok()?;
    Some(ReleaseInfo {
        version,
        tag: rel.tag_name.clone(),
        notes: rel.body.clone(),
        asset_url: asset.browser_download_url.clone(),
        asset_name: asset.name.clone(),
        asset_size: asset.size,
        checksums_url: checksums_asset_url(&rel.assets),
    })
}

/// 是否应向用户弹出更新：版本更新 且（手动 或 未被跳过）。
fn should_offer(current: &str, tag: &str, skipped: Option<&str>, manual: bool) -> Result<bool> {
    if !is_newer(current, tag)? {
        return Ok(false);
    }
    if !manual {
        if let Some(sk) = skipped {
            if sk == tag {
                return Ok(false);
            }
        }
    }
    Ok(true)
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

    #[test]
    fn parse_checksums_finds_entry_by_basename() {
        let text = "\
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  LibSSH-macos-x86_64.dmg
ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  LibSSH-macos-arm64.dmg
";
        assert_eq!(
            parse_checksums(text, "LibSSH-macos-arm64.dmg").as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert!(parse_checksums(text, "LibSSH-windows-x86_64.exe").is_none());
    }

    #[test]
    fn parse_checksums_tolerates_star_prefix_and_paths() {
        let text = "abc123  *dist/LibSSH-macos-arm64.dmg\n";
        assert_eq!(parse_checksums(text, "LibSSH-macos-arm64.dmg").as_deref(), Some("abc123"));
    }

    #[test]
    fn sha256_hex_matches_known_vectors() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    const SAMPLE_JSON: &str = r###"{
      "tag_name": "v0.2.4",
      "body": "## 修复\n- 自动更新",
      "assets": [
        { "name": "LibSSH-macos-arm64.dmg", "browser_download_url": "https://objects.githubusercontent.com/a.dmg", "size": 123 },
        { "name": "checksums.txt", "browser_download_url": "https://objects.githubusercontent.com/checksums.txt", "size": 64 }
      ]
    }"###;

    #[test]
    fn select_release_info_builds_from_json() {
        let rel = parse_release(SAMPLE_JSON).unwrap();
        let info = select_release_info(&rel, "arm64").unwrap();
        assert_eq!(info.tag, "v0.2.4");
        assert_eq!(info.version, semver::Version::parse("0.2.4").unwrap());
        assert_eq!(info.asset_name, "LibSSH-macos-arm64.dmg");
        assert_eq!(info.asset_url, "https://objects.githubusercontent.com/a.dmg");
        assert_eq!(info.asset_size, 123);
        assert_eq!(info.checksums_url.as_deref(), Some("https://objects.githubusercontent.com/checksums.txt"));
        assert!(info.notes.contains("自动更新"));
        assert!(select_release_info(&rel, "riscv").is_none());
    }

    #[test]
    fn should_offer_respects_version_and_skip() {
        assert!(should_offer("0.2.3", "v0.2.4", None, false).unwrap());
        assert!(!should_offer("0.2.3", "0.2.3", None, false).unwrap());
        assert!(!should_offer("0.2.3", "v0.2.2", None, false).unwrap());
        assert!(!should_offer("0.2.3", "v0.2.4", Some("v0.2.4"), false).unwrap());
        assert!(should_offer("0.2.3", "v0.2.4", Some("v0.2.4"), true).unwrap());
    }
}
