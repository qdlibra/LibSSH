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
