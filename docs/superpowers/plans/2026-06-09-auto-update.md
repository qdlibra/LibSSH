# 版本更新检测 + macOS 自动下载安装 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 LibSSH 增加启动时/手动的 GitHub 版本检测，弹出更新说明对话框，并在 macOS 上半自动下载、校验、安装新版本。

**Architecture:** 新增单一职责模块 `src/updater.rs`，核心逻辑（版本比较、asset 选择、checksums 解析、host 白名单、macOS 安装编排）拆成可独立单测的纯函数，外层是薄薄的异步网络/系统调用壳。UI 复用 About 对话框的居中+遮罩样式新建更新对话框，启动检查沿用 `app.rs` 既有的 `runtime.spawn + slint::invoke_from_event_loop` 模式。CI 发布时生成 `checksums.txt` 供下载校验。

**Tech Stack:** Rust + Slint，`reqwest`(rustls) 异步 HTTP，`semver` 版本比较，`sha2` 完整性校验，`futures`(已有) 流式下载，tokio(已有) 运行时，gettext `.po` 国际化。

**设计依据：** [docs/superpowers/specs/2026-06-09-auto-update-design.md](../specs/2026-06-09-auto-update-design.md)

---

## 文件结构

| 文件 | 职责 | 操作 |
|---|---|---|
| `src/updater.rs` | 检测/下载/校验/macOS 安装编排 + 纯函数 | 新增 |
| `src/main.rs` | 注册 `mod updater` | 修改（1 行） |
| `Cargo.toml` | 新增 reqwest/semver/sha2 依赖；修正 repository | 修改 |
| `src/config.rs` | 新增 3 个配置字段及 getter/setter | 修改 |
| `ui/app.slint` | 更新对话框覆盖层 + About 加版本号与"检查更新"按钮 | 修改 |
| `src/app.rs` | 启动检查接线、回调、进度回写、设 `app-version` | 修改 |
| `lang/zh|en/LC_MESSAGES/LibSSH.po` | 更新相关 UI 文案中英翻译 | 修改 |
| `.github/workflows/release.yml` | publish job 生成并上传 `checksums.txt` | 修改 |
| `README.md` | 补"自动更新"说明 | 修改（可选） |

**模块内部接口（贯穿各任务，类型必须一致）：**

```rust
// 对外
pub struct ReleaseInfo { version: semver::Version, tag, notes, asset_url, asset_name, asset_size, checksums_url: Option<String> }
pub enum InstallOutcome { ReadyToRestart { helper_script: PathBuf }, GuidedManual }
pub async fn check_for_update(current: &str, skipped: Option<String>, manual: bool) -> Result<Option<ReleaseInfo>>
pub async fn download_and_verify(rel: &ReleaseInfo, dest_dir: &Path, on_progress: impl Fn(u64, u64)) -> Result<PathBuf>
pub fn install(dmg_path: &Path) -> Result<InstallOutcome>            // 仅 macOS 真实实现
#[cfg(target_os="macos")] pub fn run_helper_and_exit(helper_script: &Path) -> !
// 纯函数（可单测）
fn is_newer(current, candidate_tag) -> Result<bool>
fn arch_tag(arch) -> Option<&'static str>;  fn target_arch_tag() -> Option<&'static str>
fn pick_asset<'a>(assets, arch_tag) -> Option<&'a GhAsset>;  fn checksums_asset_url(assets) -> Option<String>
fn parse_checksums(text, asset_name) -> Option<String>;  fn sha256_hex(bytes) -> String
fn is_allowed_host(url) -> bool
fn parse_release(json) -> Result<GhRelease>;  fn select_release_info(rel, arch_tag) -> Option<ReleaseInfo>
fn should_offer(current, tag, skipped, manual) -> Result<bool>
fn find_app_bundle(exe) -> Option<PathBuf>;  fn build_helper_script(pid, mount_app, target_app, mount_point, script_path) -> String
```

**线程模型要点：** 在 `app.rs` 里跨 `runtime.spawn`(async) / `slint::invoke_from_event_loop` 共享的状态必须是 `Send`——用 `Arc<Mutex<…>>`，**不能**用 `Rc<RefCell<…>>`。只在 UI 线程回调里访问、从不进入 async 的状态（如 `store`）保持 `Rc<RefCell<…>>` 即可。

---

## Task 1: 脚手架 — 依赖、模块、类型骨架

**Files:**
- Modify: `Cargo.toml:8`（repository）、`Cargo.toml:12-43`（dependencies）
- Modify: `src/main.rs:3-11`（mod 声明）
- Create: `src/updater.rs`

- [ ] **Step 1: 加依赖并修正 repository**

`Cargo.toml`：把第 8 行
```toml
repository = "https://github.com/your/LibSSH"
```
改为
```toml
repository = "https://github.com/qdlibra/LibSSH"
```
在 `[dependencies]` 段末尾（`base64 = "0.22"` 之后）追加：
```toml
# Auto-update: GitHub API 检测 + 流式下载 + 完整性校验。
# rustls-tls 避免 OpenSSL 系统依赖（与 russh 的 RustCrypto 栈一致）。
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json", "stream"] }
semver = "1"
sha2 = "0.10"
```

> 注：以上 feature 名对应 reqwest 0.12。若 cargo 解析到 0.13+ 且 Step 3 的 `cargo build` 报 `feature rustls-tls does not exist`，把 `"rustls-tls"` 改为 `"rustls"` 重试。

- [ ] **Step 2: 注册模块**

`src/main.rs`，在 `mod ssh_config;`（第 10 行）后、`mod system;` 前按字母序插入一行：
```rust
mod updater;
```

- [ ] **Step 3: 创建 updater.rs 类型骨架**

Create `src/updater.rs`：
```rust
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
```

- [ ] **Step 4: 验证编译**

Run: `cargo build`
Expected: 编译成功（下载新依赖，首次较慢）。若出现 reqwest feature 报错，按 Step 1 的注调整。

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs src/updater.rs
git commit -m "build(updater): 引入 reqwest/semver/sha2 并搭建 updater 模块骨架"
```

---

## Task 2: 版本比较 `is_newer`（TDD）

**Files:** Modify `src/updater.rs`

- [ ] **Step 1: 写失败测试**

在 `src/updater.rs` 末尾追加：
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_compares_semver_ignoring_v_prefix() {
        assert!(is_newer("0.2.3", "v0.2.4").unwrap());
        assert!(is_newer("v0.2.3", "0.3.0").unwrap());
        assert!(!is_newer("0.2.3", "0.2.3").unwrap());
        assert!(!is_newer("0.2.3", "v0.2.2").unwrap());
        // 预发布版本比正式版小，但比更低的正式版大。
        assert!(is_newer("0.2.3", "v0.2.4-beta.1").unwrap());
        assert!(!is_newer("0.2.4", "v0.2.4-beta.1").unwrap());
        // 非法 tag 报错而不是 panic。
        assert!(is_newer("0.2.3", "vbogus").is_err());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test is_newer`
Expected: FAIL，`cannot find function is_newer in this scope`。

- [ ] **Step 3: 实现**

在 `src/updater.rs` 的类型定义之后、`#[cfg(test)]` 之前插入：
```rust
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test is_newer`
Expected: PASS，`test result: ok. 1 passed`。

- [ ] **Step 5: Commit**

```bash
git add src/updater.rs
git commit -m "feat(updater): 语义化版本比较 is_newer"
```

---

## Task 3: 架构标签与 asset 选择（TDD）

**Files:** Modify `src/updater.rs`

- [ ] **Step 1: 写失败测试**

在 `mod tests` 内追加：
```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test updater::tests::`
Expected: FAIL，找不到 `arch_tag` / `pick_asset` / `checksums_asset_url`。

- [ ] **Step 3: 实现**

在 `is_newer` 之后插入：
```rust
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test updater::tests::`
Expected: PASS，4 passed（含 Task 2）。

- [ ] **Step 5: Commit**

```bash
git add src/updater.rs
git commit -m "feat(updater): 架构映射与 macOS dmg / checksums 选择"
```

---

## Task 4: checksums 解析与 SHA256 计算（TDD）

**Files:** Modify `src/updater.rs`

- [ ] **Step 1: 写失败测试**

在 `mod tests` 内追加：
```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test updater::tests::`
Expected: FAIL，找不到 `parse_checksums` / `sha256_hex`。

- [ ] **Step 3: 实现**

在 `checksums_asset_url` 之后插入：
```rust
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test updater::tests::`
Expected: PASS，7 passed。

- [ ] **Step 5: Commit**

```bash
git add src/updater.rs
git commit -m "feat(updater): checksums.txt 解析与 SHA256 计算"
```

---

## Task 5: API 解析与更新决策（TDD）

**Files:** Modify `src/updater.rs`

- [ ] **Step 1: 写失败测试**

在 `mod tests` 内追加：
```rust
    const SAMPLE_JSON: &str = r#"{
      "tag_name": "v0.2.4",
      "body": "## 修复\n- 自动更新",
      "assets": [
        { "name": "LibSSH-macos-arm64.dmg", "browser_download_url": "https://objects.githubusercontent.com/a.dmg", "size": 123 },
        { "name": "checksums.txt", "browser_download_url": "https://objects.githubusercontent.com/checksums.txt", "size": 64 }
      ]
    }"#;

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
        // 没有当前架构的 asset → None。
        assert!(select_release_info(&rel, "riscv").is_none());
    }

    #[test]
    fn should_offer_respects_version_and_skip() {
        assert!(should_offer("0.2.3", "v0.2.4", None, false).unwrap());
        assert!(!should_offer("0.2.3", "0.2.3", None, false).unwrap());
        assert!(!should_offer("0.2.3", "v0.2.2", None, false).unwrap());
        // 自动检查时跳过被忽略的版本。
        assert!(!should_offer("0.2.3", "v0.2.4", Some("v0.2.4"), false).unwrap());
        // 手动检查无视 skip。
        assert!(should_offer("0.2.3", "v0.2.4", Some("v0.2.4"), true).unwrap());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test updater::tests::`
Expected: FAIL，找不到 `parse_release` / `select_release_info` / `should_offer`。

- [ ] **Step 3: 实现**

在 `sha256_hex` 之后插入：
```rust
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test updater::tests::`
Expected: PASS，9 passed。

- [ ] **Step 5: Commit**

```bash
git add src/updater.rs
git commit -m "feat(updater): release JSON 解析与更新决策 should_offer"
```

---

## Task 6: `check_for_update` 异步检测（薄壳，cargo build 验证）

**Files:** Modify `src/updater.rs`

> 网络 IO 不做单测；逻辑全部在已测纯函数里。本任务只确保编译通过，运行时验证留到 Task 13 接线后。

- [ ] **Step 1: 实现**

在 `should_offer` 之后插入：
```rust
/// 查询 GitHub 最新 release 并决定是否有可供本机安装的更新。
/// 返回 Ok(None) 表示已是最新 / 被跳过 / 本架构无产物。
pub async fn check_for_update(
    current: &str,
    skipped: Option<String>,
    manual: bool,
) -> Result<Option<ReleaseInfo>> {
    let arch = target_arch_tag().ok_or_else(|| anyhow!("unsupported architecture for auto-update"))?;
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let json = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("failed to reach GitHub releases API")?
        .error_for_status()
        .context("GitHub releases API returned an error")?
        .text()
        .await?;

    let rel = parse_release(&json)?;
    if !should_offer(current, &rel.tag_name, skipped.as_deref(), manual)? {
        return Ok(None);
    }
    Ok(select_release_info(&rel, arch))
}
```

- [ ] **Step 2: 验证编译**

Run: `cargo build`
Expected: 编译成功。

- [ ] **Step 3: Commit**

```bash
git add src/updater.rs
git commit -m "feat(updater): GitHub 最新版本异步检测 check_for_update"
```

---

## Task 7: host 白名单 + `download_and_verify`（TDD + 薄壳）

**Files:** Modify `src/updater.rs`

- [ ] **Step 1: 写失败测试（host 白名单）**

在 `mod tests` 内追加：
```rust
    #[test]
    fn is_allowed_host_requires_https_and_whitelist() {
        assert!(is_allowed_host("https://api.github.com/repos/x/releases/latest"));
        assert!(is_allowed_host("https://objects.githubusercontent.com/a.dmg"));
        assert!(is_allowed_host("https://release-assets.githubusercontent.com/a.dmg")); // 子域
        assert!(!is_allowed_host("http://api.github.com/x"));          // 非 https
        assert!(!is_allowed_host("https://evil.com/a.dmg"));
        assert!(!is_allowed_host("https://api.github.com.evil.com/x")); // 伪装域名
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test is_allowed_host`
Expected: FAIL，找不到 `is_allowed_host`。

- [ ] **Step 3: 实现 host 白名单**

在 `check_for_update` 之后插入：
```rust
/// 仅允许 https + GitHub 官方下载域名（含 *.githubusercontent.com 子域）。
fn is_allowed_host(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', ':']).next().unwrap_or("");
    const EXACT: &[&str] = &[
        "api.github.com",
        "github.com",
        "objects.githubusercontent.com",
        "codeload.github.com",
    ];
    EXACT.contains(&host) || host.ends_with(".githubusercontent.com")
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test is_allowed_host`
Expected: PASS。

- [ ] **Step 5: 实现 `download_and_verify`（薄壳，不单测）**

在 `is_allowed_host` 之后插入。注意 client 设了 `connect_timeout` 防止连接阶段无限挂起（下载本体不设整体 timeout，避免大文件被截断）：
```rust
/// 下载 dmg 到 dest_dir，校验 SHA256（缺 checksums 判失败）。
/// on_progress(已下载字节, 总字节)；总字节未知时回传 asset_size。
/// 第一版下载不可中断——连接卡住由 connect_timeout 兜底。
pub async fn download_and_verify(
    rel: &ReleaseInfo,
    dest_dir: &Path,
    on_progress: impl Fn(u64, u64),
) -> Result<PathBuf> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    if !is_allowed_host(&rel.asset_url) {
        bail!("refusing to download from untrusted host: {}", rel.asset_url);
    }
    let checksums_url = rel
        .checksums_url
        .as_deref()
        .ok_or_else(|| anyhow!("release has no checksums.txt; refusing to auto-update"))?;
    if !is_allowed_host(checksums_url) {
        bail!("refusing to fetch checksums from untrusted host");
    }

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()?;

    // 1) 期望哈希。
    let checksums = client
        .get(checksums_url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let expected = parse_checksums(&checksums, &rel.asset_name)
        .ok_or_else(|| anyhow!("checksums.txt has no entry for {}", rel.asset_name))?;

    // 2) 流式下载到文件 + 进度。
    tokio::fs::create_dir_all(dest_dir).await?;
    let dest = dest_dir.join(&rel.asset_name);
    let resp = client.get(&rel.asset_url).send().await?.error_for_status()?;
    let total = resp.content_length().unwrap_or(rel.asset_size);
    let mut file = tokio::fs::File::create(&dest).await?;
    let mut downloaded: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        on_progress(downloaded, total);
    }
    file.flush().await?;
    drop(file);

    // 3) 校验（复用 sha256_hex）。
    let bytes = tokio::fs::read(&dest).await?;
    if sha256_hex(&bytes).to_lowercase() != expected.to_lowercase() {
        let _ = tokio::fs::remove_file(&dest).await;
        bail!("checksum mismatch for {}", rel.asset_name);
    }
    Ok(dest)
}
```

- [ ] **Step 6: 验证编译并跑全部 updater 测试**

Run: `cargo test updater::tests::`
Expected: PASS，11 passed。

- [ ] **Step 7: Commit**

```bash
git add src/updater.rs
git commit -m "feat(updater): 带进度与 SHA256 校验的流式下载 download_and_verify"
```

---

## Task 8: macOS 安装纯函数（TDD）

**Files:** Modify `src/updater.rs`

> `find_app_bundle` / `sh_squote` / `build_helper_script` 是纯逻辑，不加 `#[cfg]`，所有平台都编译并参与测试（覆盖率更高）。

- [ ] **Step 1: 写失败测试**

在 `mod tests` 内追加：
```rust
    #[test]
    fn find_app_bundle_walks_up_to_dot_app() {
        let exe = Path::new("/Applications/LibSSH.app/Contents/MacOS/LibSSH");
        assert_eq!(find_app_bundle(exe), Some(PathBuf::from("/Applications/LibSSH.app")));
        // 开发环境（target/release）下没有 .app。
        assert_eq!(find_app_bundle(Path::new("/home/q/proj/target/release/LibSSH")), None);
        // bundle 自身。
        assert_eq!(
            find_app_bundle(Path::new("/Applications/LibSSH.app")),
            Some(PathBuf::from("/Applications/LibSSH.app"))
        );
    }

    #[test]
    fn build_helper_script_quotes_paths_and_has_steps() {
        let s = build_helper_script(
            4242,
            Path::new("/Volumes/LibSSH 1.0/LibSSH.app"), // 含空格，必须被引用
            Path::new("/Applications/LibSSH.app"),
            Path::new("/tmp/mnt"),
            Path::new("/tmp/upd.sh"),
        );
        assert!(s.contains("kill -0 4242"));
        assert!(s.contains("ditto '/Volumes/LibSSH 1.0/LibSSH.app' '/Applications/LibSSH.app'"));
        assert!(s.contains("xattr -dr com.apple.quarantine '/Applications/LibSSH.app'"));
        assert!(s.contains("hdiutil detach '/tmp/mnt'"));
        assert!(s.contains("open '/Applications/LibSSH.app'"));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test updater::tests::`
Expected: FAIL，找不到 `find_app_bundle` / `build_helper_script`。

- [ ] **Step 3: 实现**

在 `download_and_verify` 之后插入：
```rust
/// 从可执行文件路径向上找到 .app bundle 目录。
fn find_app_bundle(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|a| a.extension().and_then(|e| e.to_str()) == Some("app"))
        .map(|a| a.to_path_buf())
}

/// 用单引号包裹路径供 /bin/sh 使用，转义内部单引号。
fn sh_squote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

/// 生成"等旧进程退出 → 覆盖 .app → 解隔离 → 卸载 → 启动新版 → 自删"的脚本。
fn build_helper_script(
    pid: u32,
    mount_app: &Path,
    target_app: &Path,
    mount_point: &Path,
    script_path: &Path,
) -> String {
    format!(
        "#!/bin/sh\n\
         # LibSSH 自动更新辅助脚本（旧进程退出后接管覆盖与重启）。\n\
         while kill -0 {pid} 2>/dev/null; do sleep 0.2; done\n\
         /usr/bin/ditto {src} {dst}\n\
         /usr/bin/xattr -dr com.apple.quarantine {dst}\n\
         /usr/bin/hdiutil detach {mnt} >/dev/null 2>&1 || true\n\
         /usr/bin/open {dst}\n\
         /bin/rm -f {selfp}\n",
        pid = pid,
        src = sh_squote(mount_app),
        dst = sh_squote(target_app),
        mnt = sh_squote(mount_point),
        selfp = sh_squote(script_path),
    )
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test updater::tests::`
Expected: PASS，13 passed。

- [ ] **Step 5: Commit**

```bash
git add src/updater.rs
git commit -m "feat(updater): macOS .app 定位与自更新脚本生成（纯函数）"
```

---

## Task 9: macOS `install` 编排 + 平台占位（薄壳）

**Files:** Modify `src/updater.rs`

- [ ] **Step 1: 实现 macOS 安装与重启**

在 `build_helper_script` 之后插入：
```rust
/// macOS：挂载 dmg → 生成覆盖脚本 → 返回 ReadyToRestart；
/// 当前 app 不可写 / 非 bundle 运行 / 挂载失败时降级为引导式（打开 dmg）。
#[cfg(target_os = "macos")]
pub fn install(dmg_path: &Path) -> Result<InstallOutcome> {
    use std::process::Command;

    let open_dmg = |p: &Path| {
        let _ = Command::new("/usr/bin/open").arg(p).status();
    };

    let exe = std::env::current_exe().context("cannot locate current executable")?;
    let Some(bundle) = find_app_bundle(&exe) else {
        open_dmg(dmg_path);
        return Ok(InstallOutcome::GuidedManual);
    };
    let parent = bundle.parent().unwrap_or_else(|| Path::new("/"));
    if !dir_is_writable(parent) {
        open_dmg(dmg_path);
        return Ok(InstallOutcome::GuidedManual);
    }

    let mount_point = std::env::temp_dir().join(format!("LibSSH-update-mnt-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&mount_point);
    let attached = Command::new("/usr/bin/hdiutil")
        .args(["attach", "-nobrowse", "-noverify", "-mountpoint"])
        .arg(&mount_point)
        .arg(dmg_path)
        .status()
        .context("hdiutil attach failed")?;
    if !attached.success() {
        open_dmg(dmg_path);
        return Ok(InstallOutcome::GuidedManual);
    }

    let mount_app = mount_point.join("LibSSH.app");
    if !mount_app.exists() {
        let _ = Command::new("/usr/bin/hdiutil").arg("detach").arg(&mount_point).status();
        open_dmg(dmg_path);
        return Ok(InstallOutcome::GuidedManual);
    }

    let script_path = std::env::temp_dir().join(format!("LibSSH-update-{}.sh", std::process::id()));
    let script = build_helper_script(std::process::id(), &mount_app, &bundle, &mount_point, &script_path);
    std::fs::write(&script_path, script).context("failed to write helper script")?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700));

    Ok(InstallOutcome::ReadyToRestart { helper_script: script_path })
}

/// 探测目录是否可写（用临时探针文件）。
#[cfg(target_os = "macos")]
fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".LibSSH-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 分离地启动辅助脚本，然后退出本进程（脚本等本进程退出后接管）。
#[cfg(target_os = "macos")]
pub fn run_helper_and_exit(helper_script: &Path) -> ! {
    use std::process::{Command, Stdio};
    let _ = Command::new("/bin/sh")
        .arg(helper_script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    std::process::exit(0);
}

/// 其它平台：第一版不支持自动安装。
#[cfg(not(target_os = "macos"))]
pub fn install(_dmg_path: &Path) -> Result<InstallOutcome> {
    bail!("auto-install is not supported on this platform yet")
}
```

- [ ] **Step 2: 验证编译 + 全量测试**

Run: `cargo build && cargo test updater::tests::`
Expected: 编译成功；13 passed（macOS 上）。

> 在非 macOS（如 CI 的 ubuntu）上 `cargo build` 也必须通过——`install` 走 `#[cfg(not(target_os="macos"))]` 分支。

- [ ] **Step 3: Commit**

```bash
git add src/updater.rs
git commit -m "feat(updater): macOS 半自动安装编排与重启（其它平台占位）"
```

---

## Task 10: 配置项（TDD）

**Files:** Modify `src/config.rs`

- [ ] **Step 1: 写失败测试**

在 `src/config.rs` 的 `mod tests` 内追加：
```rust
    #[test]
    fn update_settings_round_trip_and_default() {
        let path = test_path("update-settings");
        let mut store = ConfigStore::load_at(path.clone()).unwrap();
        // 默认值。
        assert!(store.auto_check_update());
        assert_eq!(store.last_update_check(), None);
        assert_eq!(store.skipped_version(), None);

        store.set_auto_check_update(false);
        store.set_last_update_check(Some(1_700_000_000));
        store.set_skipped_version(Some("v0.2.4".to_string()));
        store.save().unwrap();

        let loaded = ConfigStore::load_at(path).unwrap();
        assert!(!loaded.auto_check_update());
        assert_eq!(loaded.last_update_check(), Some(1_700_000_000));
        assert_eq!(loaded.skipped_version(), Some("v0.2.4"));
    }

    #[test]
    fn legacy_config_without_update_fields_defaults_to_auto_check_on() {
        let path = test_path("legacy-no-update-fields");
        fs::write(&path, r#"{ "sessions": [] }"#).unwrap();
        let loaded = ConfigStore::load_at(path).unwrap();
        assert!(loaded.auto_check_update()); // 缺字段默认开
        assert_eq!(loaded.skipped_version(), None);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test update_settings_round_trip`
Expected: FAIL，找不到 `auto_check_update` 等方法。

- [ ] **Step 3: 在 ConfigFile 加字段与默认值**

`src/config.rs`：在 `ConfigFile` 定义之前（约 `:247`，`/// On-disk layout...` 注释之上）加默认函数：
```rust
fn default_true() -> bool {
    true
}
```
把 `ConfigFile` 的派生（约 `:248`）
```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
```
改为（去掉 `Default`，下面手写以保证 `auto_check_update` 默认 `true`）：
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
```
在 `ConfigFile` 的 `ai_skill` 字段后追加三个字段：
```rust
    /// 启动时是否自动检查更新（默认开）。
    #[serde(default = "default_true")]
    pub auto_check_update: bool,
    /// 上次检查更新的 unix 时间戳（秒），用于 24h 节流。
    #[serde(default)]
    pub last_update_check: Option<i64>,
    /// 用户"跳过此版本"记录的 tag，如 "v0.2.4"。
    #[serde(default)]
    pub skipped_version: Option<String>,
```
紧接 `ConfigFile` 结构体之后新增手写 `Default`：
```rust
impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            sessions: Vec::new(),
            download_dir: String::new(),
            language: String::new(),
            ai_skill: AiSkillConfig::default(),
            auto_check_update: true,
            last_update_check: None,
            skipped_version: None,
        }
    }
}
```

- [ ] **Step 4: 加 getter/setter**

在 `impl ConfigStore` 内（`set_language` 之后，约 `:350`）追加：
```rust
    pub fn auto_check_update(&self) -> bool {
        self.cache.auto_check_update
    }

    pub fn set_auto_check_update(&mut self, on: bool) {
        self.cache.auto_check_update = on;
    }

    pub fn last_update_check(&self) -> Option<i64> {
        self.cache.last_update_check
    }

    pub fn set_last_update_check(&mut self, ts: Option<i64>) {
        self.cache.last_update_check = ts;
    }

    pub fn skipped_version(&self) -> Option<&str> {
        self.cache.skipped_version.as_deref()
    }

    pub fn set_skipped_version(&mut self, tag: Option<String>) {
        self.cache.skipped_version = tag;
    }
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test config::tests::`
Expected: PASS（原有 + 2 个新测试全绿）。

- [ ] **Step 6: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): 新增自动更新相关设置（auto_check_update / last_update_check / skipped_version）"
```

---

## Task 11: 更新对话框 UI + About 改造

**Files:** Modify `ui/app.slint`

> Slint 无法单测，验证靠 `cargo build`（含 slint 编译）。颜色名（`Theme.bg-elevated` / `Theme.accent` / `Theme.danger` / `Theme.border-subtle`）若有缺失，打开 `ui/theme.slint` 选用最接近的已有名替换。`#ffffff` 是颜色字面量。

- [ ] **Step 1: 新增属性与回调**

`ui/app.slint`，在 About 相关属性区（约 `:121` `about-libs` 之后）追加：
```slint
    // --- App version & update dialog --------------------------------------
    in property <string> app-version;            // e.g. "0.2.3"
    in-out property <bool> update-open: false;
    in property <string> update-version;         // 新版本号 "0.2.4"
    in property <string> update-current;         // 当前版本 "0.2.3"
    in property <string> update-notes;           // release notes
    in property <string> update-phase: "prompt"; // prompt|downloading|verifying|ready|error
    in property <float> update-progress;         // 0.0 - 1.0
    in property <string> update-error;
    in property <bool> update-guided;            // true=引导式(方案A)，重启按钮文案不同
    callback update-confirm();                   // 立即更新→开始下载
    callback update-later();                     // 稍后
    callback update-skip();                      // 跳过此版本
    callback update-restart();                   // 就绪后重启 / 完成
    callback update-retry();                     // 失败后重试
    callback update-open-release();              // 去发布页
    callback check-update-manual();              // 关于页"检查更新"
```

- [ ] **Step 2: About 对话框加版本号与"检查更新"按钮**

在 About 对话框里，把"Open source · MIT / Apache-2.0"那段（约 `:749-753`）之后、分隔线（`:755`）之前插入版本号与按钮：
```slint
                Text {
                    text: "v" + root.app-version;
                    color: Theme.text-secondary;
                    font-size: Theme.fs-sm;
                }
                Rectangle {
                    height: 30px;
                    width: 112px;
                    border-radius: Theme.radius-sm;
                    background: check-upd-ta.has-hover ? Theme.bg-hover : Theme.bg-panel;
                    border-width: 1px;
                    border-color: Theme.border-strong;
                    check-upd-ta := TouchArea {
                        mouse-cursor: pointer;
                        clicked => {
                            root.about-open = false;
                            root.check-update-manual();
                        }
                    }
                    Text {
                        text: @tr("Check for updates");
                        color: Theme.text-primary;
                        font-size: Theme.fs-sm;
                        horizontal-alignment: center;
                        vertical-alignment: center;
                    }
                }
```

- [ ] **Step 3: 新增更新对话框覆盖层**

在 About 对话框那个 `Rectangle { ... }` 块（结束于 `:781`）之后插入一个新的居中对话框（模仿 About 的尺寸与遮罩）。下载阶段只显示进度、无取消按钮（见"已知偏差"）：
```slint
    // --- Update dialog (centered, dim backdrop) ---------------------------
    Rectangle {
        width: parent.width;
        height: parent.height;
        visible: root.update-open;
        background: #00000080;
        TouchArea {} // 吞背景点击：更新对话框只能按按钮关闭

        Rectangle {
            x: (parent.width - self.width) / 2;
            y: (parent.height - self.height) / 2;
            width: 420px;
            height: 460px;
            background: Theme.bg-panel;
            border-radius: Theme.radius-md;
            border-width: 1px;
            border-color: Theme.border-strong;
            drop-shadow-blur: 24px;
            drop-shadow-color: #000000a0;

            VerticalLayout {
                padding: 18px;
                spacing: 10px;

                Text {
                    text: @tr("New version available");
                    color: Theme.text-primary;
                    font-size: Theme.fs-lg;
                    font-weight: 700;
                }
                Text {
                    text: "v" + root.update-current + "  →  v" + root.update-version;
                    color: Theme.text-secondary;
                    font-size: Theme.fs-sm;
                }

                Rectangle { height: 1px; background: Theme.border-subtle; }

                // 更新说明（可滚动）
                Flickable {
                    vertical-stretch: 1;
                    viewport-width: self.width;
                    viewport-height: max(self.height, notes-text.preferred-height);
                    notes-text := Text {
                        text: root.update-notes;
                        color: Theme.text-primary;
                        font-size: Theme.fs-sm;
                        wrap: word-wrap;
                    }
                }

                // 进度 / 错误区
                if root.update-phase == "downloading" || root.update-phase == "verifying" : VerticalLayout {
                    spacing: 4px;
                    Text {
                        text: root.update-phase == "verifying"
                            ? @tr("Verifying…")
                            : @tr("Downloading…") + " \{Math.round(root.update-progress * 100)}%";
                        color: Theme.text-secondary;
                        font-size: Theme.fs-sm;
                    }
                    Rectangle {
                        height: 6px;
                        border-radius: 3px;
                        background: Theme.bg-panel;
                        border-width: 1px;
                        border-color: Theme.border-subtle;
                        Rectangle {
                            x: 0;
                            width: parent.width * root.update-progress;
                            height: parent.height;
                            border-radius: 3px;
                            background: Theme.accent;
                        }
                    }
                }
                if root.update-phase == "error" : Text {
                    text: root.update-error;
                    color: Theme.danger;
                    font-size: Theme.fs-sm;
                    wrap: word-wrap;
                }

                // 按钮区
                HorizontalLayout {
                    alignment: end;
                    spacing: 8px;

                    // prompt：跳过 / 稍后 / 立即更新
                    if root.update-phase == "prompt" : HorizontalLayout {
                        spacing: 8px;
                        Rectangle {
                            width: 100px; height: 32px; border-radius: Theme.radius-sm;
                            background: skip-ta.has-hover ? Theme.bg-hover : transparent;
                            border-width: 1px; border-color: Theme.border-strong;
                            skip-ta := TouchArea { mouse-cursor: pointer; clicked => { root.update-skip(); } }
                            Text { text: @tr("Skip this version"); color: Theme.text-secondary;
                                   font-size: Theme.fs-sm; horizontal-alignment: center; vertical-alignment: center; }
                        }
                        Rectangle {
                            width: 64px; height: 32px; border-radius: Theme.radius-sm;
                            background: later-ta.has-hover ? Theme.bg-hover : transparent;
                            border-width: 1px; border-color: Theme.border-strong;
                            later-ta := TouchArea { mouse-cursor: pointer; clicked => { root.update-later(); } }
                            Text { text: @tr("Later"); color: Theme.text-secondary;
                                   font-size: Theme.fs-sm; horizontal-alignment: center; vertical-alignment: center; }
                        }
                        Rectangle {
                            width: 96px; height: 32px; border-radius: Theme.radius-sm;
                            background: Theme.accent;
                            confirm-ta := TouchArea { mouse-cursor: pointer; clicked => { root.update-confirm(); } }
                            Text { text: @tr("Update now"); color: #ffffff;
                                   font-size: Theme.fs-sm; horizontal-alignment: center; vertical-alignment: center; }
                        }
                    }

                    // ready：重启 / 完成
                    if root.update-phase == "ready" : Rectangle {
                        width: 120px; height: 32px; border-radius: Theme.radius-sm;
                        background: Theme.accent;
                        restart-ta := TouchArea { mouse-cursor: pointer; clicked => { root.update-restart(); } }
                        Text { text: root.update-guided ? @tr("Done") : @tr("Restart now"); color: #ffffff;
                               font-size: Theme.fs-sm; horizontal-alignment: center; vertical-alignment: center; }
                    }

                    // error：重试 / 去发布页
                    if root.update-phase == "error" : HorizontalLayout {
                        spacing: 8px;
                        Rectangle {
                            width: 100px; height: 32px; border-radius: Theme.radius-sm;
                            background: rel-ta.has-hover ? Theme.bg-hover : transparent;
                            border-width: 1px; border-color: Theme.border-strong;
                            rel-ta := TouchArea { mouse-cursor: pointer; clicked => { root.update-open-release(); } }
                            Text { text: @tr("Release page"); color: Theme.text-secondary;
                                   font-size: Theme.fs-sm; horizontal-alignment: center; vertical-alignment: center; }
                        }
                        Rectangle {
                            width: 72px; height: 32px; border-radius: Theme.radius-sm;
                            background: Theme.accent;
                            retry-ta := TouchArea { mouse-cursor: pointer; clicked => { root.update-retry(); } }
                            Text { text: @tr("Retry"); color: #ffffff;
                                   font-size: Theme.fs-sm; horizontal-alignment: center; vertical-alignment: center; }
                        }
                    }
                }
            }
        }
    }
```

- [ ] **Step 4: 验证编译**

Run: `cargo build`
Expected: slint 编译通过、整体编译成功。若报某 `Theme.xxx` 不存在，按提示替换为 `ui/theme.slint` 里的可用名。

- [ ] **Step 5: Commit**

```bash
git add ui/app.slint
git commit -m "feat(ui): 更新对话框覆盖层 + 关于页版本号与检查更新按钮"
```

---

## Task 12: 国际化文案（.po）

**Files:** Modify `lang/zh/LC_MESSAGES/LibSSH.po`、`lang/en/LC_MESSAGES/LibSSH.po`

- [ ] **Step 1: 加中文翻译**

在 `lang/zh/LC_MESSAGES/LibSSH.po` 末尾追加：
```po
msgid "Check for updates"
msgstr "检查更新"

msgid "New version available"
msgstr "发现新版本"

msgid "Skip this version"
msgstr "跳过此版本"

msgid "Later"
msgstr "稍后"

msgid "Update now"
msgstr "立即更新"

msgid "Downloading…"
msgstr "下载中…"

msgid "Verifying…"
msgstr "校验中…"

msgid "Restart now"
msgstr "立即重启"

msgid "Done"
msgstr "完成"

msgid "Retry"
msgstr "重试"

msgid "Release page"
msgstr "去发布页"
```

- [ ] **Step 2: 加英文翻译**

在 `lang/en/LC_MESSAGES/LibSSH.po` 末尾追加（msgstr 即英文原文）：
```po
msgid "Check for updates"
msgstr "Check for updates"

msgid "New version available"
msgstr "New version available"

msgid "Skip this version"
msgstr "Skip this version"

msgid "Later"
msgstr "Later"

msgid "Update now"
msgstr "Update now"

msgid "Downloading…"
msgstr "Downloading…"

msgid "Verifying…"
msgstr "Verifying…"

msgid "Restart now"
msgstr "Restart now"

msgid "Done"
msgstr "Done"

msgid "Retry"
msgstr "Retry"

msgid "Release page"
msgstr "Release page"
```

> 下面这些是 Rust 侧用 `i18n::t(zh, en)` 直接给出的动态文案，**不**走 `.po`：「已是最新版本。」「检查更新失败。」「请将 LibSSH 拖到「应用程序」文件夹以完成更新。」「安装失败。」「下载或校验失败。」

- [ ] **Step 3: 验证编译（slint 会嵌入翻译）**

Run: `cargo build`
Expected: 编译成功。

- [ ] **Step 4: Commit**

```bash
git add lang/zh/LC_MESSAGES/LibSSH.po lang/en/LC_MESSAGES/LibSSH.po
git commit -m "i18n: 补充自动更新相关 UI 文案的中英翻译"
```

---

## Task 13: app.rs 接线（启动检查 + 回调 + 进度回写）

**Files:** Modify `src/app.rs`

> 这是集成点。无单测；`cargo build` + `cargo test`（确保不破坏既有测试）+ 手动验证。
> Rust 动态文案用 `crate::i18n::t(zh, en)`；下载目录用 `directories` 的 cache dir。
> `app.rs:6` 已 `use std::sync::{Arc, Mutex};`，可直接使用。

- [ ] **Step 1: 设置 app-version**

在 `run()` 里 `let window = AppWindow::new()?;`（`:71`）之后、`wire_callbacks(...)` 调用之前加一行：
```rust
    window.set_app_version(env!("CARGO_PKG_VERSION").into());
```

- [ ] **Step 2: 在 wire_callbacks 末尾接线更新逻辑**

在 `wire_callbacks(...)` 函数体的最后（结尾 `}` 之前）追加以下整块。关键：跨线程共享的 `pending_release` / `pending_helper` 用 `Arc<Mutex<…>>`（不是 `Rc<RefCell<…>>`），因为它们会被移入 `runtime.spawn` 的 async 与 `invoke_from_event_loop`（要求 `Send`）；`store` 保持 `Rc<RefCell<…>>`，只在 UI 线程回调里 `borrow`。

```rust
    // ===== 自动更新接线 =====
    // 当前弹出的新版本信息（点"立即更新"时用）。Arc<Mutex>：要跨 async/invoke。
    let pending_release: Arc<Mutex<Option<crate::updater::ReleaseInfo>>> = Arc::new(Mutex::new(None));
    // 下载安装完成后待执行的辅助脚本（点"重启"时用）。
    let pending_helper: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::new(Mutex::new(None));

    // 把一个 ReleaseInfo 显示到对话框（在 UI 线程调用）。内部 fn，不捕获环境。
    fn show_release(w: &AppWindow, rel: &crate::updater::ReleaseInfo) {
        w.set_update_current(env!("CARGO_PKG_VERSION").into());
        w.set_update_version(rel.version.to_string().into());
        w.set_update_notes(rel.notes.clone().into());
        w.set_update_phase("prompt".into());
        w.set_update_progress(0.0);
        w.set_update_guided(false);
        w.set_update_error("".into());
        w.set_update_open(true);
    }

    // 下载目录：~/Library/Caches/<app>/updates/
    fn updates_dir() -> std::path::PathBuf {
        directories::ProjectDirs::from("dev", "LibSSH", "LibSSH")
            .map(|d| d.cache_dir().join("updates"))
            .unwrap_or_else(|| std::env::temp_dir().join("LibSSH-updates"))
    }

    // --- 启动自动检查（节流 24h）---
    {
        let do_check = {
            let s = store.borrow();
            s.auto_check_update()
                && match s.last_update_check() {
                    Some(last) => chrono::Utc::now().timestamp() - last >= 24 * 3600,
                    None => true,
                }
        };
        if do_check {
            {
                let mut s = store.borrow_mut();
                s.set_last_update_check(Some(chrono::Utc::now().timestamp()));
                let _ = s.save();
            }
            let skipped = store.borrow().skipped_version().map(|s| s.to_string());
            let weak = window.as_weak();
            let pending = pending_release.clone();
            runtime.spawn(async move {
                match crate::updater::check_for_update(env!("CARGO_PKG_VERSION"), skipped, false).await {
                    Ok(Some(rel)) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                *pending.lock().unwrap() = Some(rel.clone());
                                show_release(&w, &rel);
                            }
                        });
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("auto update check failed: {e:#}"),
                }
            });
        }
    }

    // --- 手动检查（关于页按钮）---
    {
        let weak = window.as_weak();
        let store = store.clone();
        let runtime = runtime.clone();
        let pending = pending_release.clone();
        window.on_check_update_manual(move || {
            let skipped = store.borrow().skipped_version().map(|s| s.to_string());
            let weak = weak.clone();
            let pending = pending.clone();
            runtime.spawn(async move {
                let res = crate::updater::check_for_update(env!("CARGO_PKG_VERSION"), skipped, true).await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        match res {
                            Ok(Some(rel)) => {
                                *pending.lock().unwrap() = Some(rel.clone());
                                show_release(&w, &rel);
                            }
                            Ok(None) => {
                                w.set_alert_title(crate::i18n::t("检查更新", "Check for updates").into());
                                w.set_alert_message(crate::i18n::t("已是最新版本。", "You are on the latest version.").into());
                                w.set_alert_open(true);
                            }
                            Err(_) => {
                                w.set_alert_title(crate::i18n::t("检查更新", "Check for updates").into());
                                w.set_alert_message(crate::i18n::t("检查更新失败。", "Update check failed.").into());
                                w.set_alert_open(true);
                            }
                        }
                    }
                });
            });
        });
    }

    // --- 立即更新：下载 → 校验 → 安装 ---
    {
        let weak = window.as_weak();
        let runtime = runtime.clone();
        let pending = pending_release.clone();
        let helper = pending_helper.clone();
        window.on_update_confirm(move || {
            let Some(rel) = pending.lock().unwrap().clone() else { return; };
            if let Some(w) = weak.upgrade() {
                w.set_update_phase("downloading".into());
                w.set_update_progress(0.0);
            }
            let weak = weak.clone();
            let helper = helper.clone();
            runtime.spawn(async move {
                // 进度回调：节流到每 1% 刷新一次。Weak / Arc<Atomic> 都是 Send。
                let prog_weak = weak.clone();
                let last_pct = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
                let on_progress = move |done: u64, total: u64| {
                    let pct = if total > 0 { done * 100 / total } else { 0 };
                    if last_pct.swap(pct, std::sync::atomic::Ordering::Relaxed) != pct {
                        let prog_weak = prog_weak.clone();
                        let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = prog_weak.upgrade() {
                                w.set_update_progress(frac);
                            }
                        });
                    }
                };

                let dl = crate::updater::download_and_verify(&rel, &updates_dir(), on_progress).await;

                match dl {
                    Ok(dmg) => {
                        let vweak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = vweak.upgrade() { w.set_update_phase("verifying".into()); }
                        });
                        // 安装是系统调用，放 blocking 线程。
                        let install = tokio::task::spawn_blocking(move || crate::updater::install(&dmg)).await;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                match install {
                                    Ok(Ok(crate::updater::InstallOutcome::ReadyToRestart { helper_script })) => {
                                        *helper.lock().unwrap() = Some(helper_script);
                                        w.set_update_guided(false);
                                        w.set_update_phase("ready".into());
                                    }
                                    Ok(Ok(crate::updater::InstallOutcome::GuidedManual)) => {
                                        *helper.lock().unwrap() = None;
                                        w.set_update_guided(true);
                                        w.set_update_notes(crate::i18n::t(
                                            "请将 LibSSH 拖到「应用程序」文件夹以完成更新。",
                                            "Drag LibSSH into the Applications folder to finish updating.",
                                        ).into());
                                        w.set_update_phase("ready".into());
                                    }
                                    _ => {
                                        w.set_update_error(crate::i18n::t("安装失败。", "Install failed.").into());
                                        w.set_update_phase("error".into());
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("download failed: {e:#}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_update_error(crate::i18n::t("下载或校验失败。", "Download or verification failed.").into());
                                w.set_update_phase("error".into());
                            }
                        });
                    }
                }
            });
        });
    }

    // --- 稍后 ---
    {
        let weak = window.as_weak();
        window.on_update_later(move || {
            if let Some(w) = weak.upgrade() { w.set_update_open(false); }
        });
    }

    // --- 跳过此版本 ---
    {
        let weak = window.as_weak();
        let store = store.clone();
        let pending = pending_release.clone();
        window.on_update_skip(move || {
            if let Some(rel) = pending.lock().unwrap().clone() {
                let mut s = store.borrow_mut();
                s.set_skipped_version(Some(rel.tag.clone()));
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() { w.set_update_open(false); }
        });
    }

    // --- 重启 / 完成 ---
    {
        let weak = window.as_weak();
        let helper = pending_helper.clone();
        window.on_update_restart(move || {
            if let Some(_script) = helper.lock().unwrap().clone() {
                #[cfg(target_os = "macos")]
                crate::updater::run_helper_and_exit(&_script); // 不返回，进程被替换
            }
            // 引导式（无脚本）或非 macOS：仅关闭对话框。
            if let Some(w) = weak.upgrade() { w.set_update_open(false); }
        });
    }

    // --- 重试 ---
    {
        let weak = window.as_weak();
        window.on_update_retry(move || {
            if let Some(w) = weak.upgrade() {
                w.set_update_error("".into());
                w.set_update_phase("prompt".into()); // 回到 prompt，用户可重新点"立即更新"
            }
        });
    }

    // --- 去发布页 ---
    {
        window.on_update_open_release(move || {
            let url = "https://github.com/qdlibra/LibSSH/releases/latest";
            #[cfg(target_os = "macos")]
            let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
            #[cfg(target_os = "windows")]
            let _ = std::process::Command::new("cmd").args(["/C", "start", url]).spawn();
            #[cfg(all(unix, not(target_os = "macos")))]
            let _ = std::process::Command::new("xdg-open").arg(url).spawn();
        });
    }
```

- [ ] **Step 3: 验证编译**

Run: `cargo build`
Expected: 编译成功。排错提示：
- slint setter 名 = 属性名连字符转下划线：`update-phase`→`set_update_phase`、`app-version`→`set_app_version`。
- 若报 `pending`/`helper` 不是 `Send`：确认它们是 `Arc<Mutex<…>>` 而非 `Rc<RefCell<…>>`。
- 若报 `store` 相关 `Send` 错误：确认 `store` 没有被移入任何 `runtime.spawn(async move …)`（它只应在 UI 线程的回调体里 `borrow`）。

- [ ] **Step 4: 跑全部测试确认未破坏既有功能**

Run: `cargo test`
Expected: 全绿（updater 13 + config 既有及新增 + 其它既有测试）。

- [ ] **Step 5: 手动验证清单（macOS）**

把 `Cargo.toml` 版本临时改成一个**比线上最新 release 低**的值（如线上 `v0.2.3` → 本地改 `0.2.2`），`cargo run`，启动几秒后应弹出更新对话框：
- [ ] 显示新版本号、当前版本号、release notes。
- [ ] 点[稍后]关闭；24h 内重开 app 不再自动弹（节流生效）。
- [ ] 关于对话框显示 `v0.2.2` 和"检查更新"按钮；点按钮触发检查。
- [ ] 点[立即更新]→ 进度条推进 → 校验 →（已安装到 /Applications 时）显示[立即重启]。
- [ ] 点[跳过此版本]后，重开 app 自动检查不再弹该版本；关于页手动检查仍弹。
- [ ] 断网或把 `REPO` 改错，手动检查弹"检查更新失败"。
完成后把 `Cargo.toml` 版本改回真实值（**勿提交临时改动**）。

- [ ] **Step 6: Commit**

```bash
git add src/app.rs
git commit -m "feat(app): 接线启动/手动更新检查、下载安装与对话框回调"
```

---

## Task 14: CI 发布生成 checksums.txt

**Files:** Modify `.github/workflows/release.yml`

- [ ] **Step 1: 在 publish job 生成并上传 checksums.txt**

`.github/workflows/release.yml` 的 `publish` job（`:210-222`），把"Publish release"步骤的 `run: |` 块替换为：
```yaml
        run: |
          set -euo pipefail
          mapfile -t files < <(find artifacts -type f)
          # 生成 checksums.txt（sha256sum，文件名只保留 basename，供 App 自动更新校验）。
          : > checksums.txt
          for f in "${files[@]}"; do
            sha256sum "$f" | awk -v name="$(basename "$f")" '{print $1 "  " name}' >> checksums.txt
          done
          files+=("checksums.txt")
          if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
            gh release upload "$TAG" "${files[@]}" --repo "$REPO" --clobber
          else
            gh release create "$TAG" "${files[@]}" --repo "$REPO" --generate-notes --title "$TAG"
          fi
```

- [ ] **Step 2: 校验 YAML 语法**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))" && echo OK`
Expected: `OK`。

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: 发布时生成并上传 checksums.txt 供自动更新校验"
```

> 运行时验证：下次推 main 触发 release 后，确认 release 资产里出现 `checksums.txt` 且含各 dmg 的 sha256 行。

---

## Task 15: README 文档（可选收尾）

**Files:** Modify `README.md`

- [ ] **Step 1: 加"自动更新"说明**

在 `README.md` 的"## Platform Packaging"段之后插入：
```markdown
## 自动更新

LibSSH 启动时会（每 24 小时一次）从 GitHub Releases 检查新版本；也可在「关于」对话框点「检查更新」手动触发。检测到新版本会弹出更新说明与 `跳过此版本 / 稍后 / 立即更新`。

macOS 上点「立即更新」会下载对应架构的 dmg、用 `checksums.txt` 校验 SHA256，然后自动挂载、覆盖 `/Applications/LibSSH.app`、清除隔离标记，提示重启完成更新。当 app 不在可写位置（如开发环境）时降级为打开 dmg 引导手动安装。Windows / Linux 的自动安装尚未实现。

关闭自动检查：编辑配置文件把 `auto_check_update` 设为 `false`。
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: 补充自动更新说明"
```

---

## 已知偏差与 YAGNI 取舍

- **下载不可中断**：第一版下载阶段（`downloading`/`verifying`）不提供取消按钮，仅显示进度。dmg 约 7MB、通常数秒完成；连接卡住由 `connect_timeout(15s)` 兜底，下载中途断流由 reqwest 报错——两者都落到 `error` 阶段（有"重试 / 去发布页"逃生）。spec 错误矩阵里的"下载中取消"留待后续（需引入 `CancellationToken` 改 `download_and_verify` 签名）。
- **Windows / Linux 安装**：`install` 走 `#[cfg(not(target_os="macos"))]` 直接 `bail`；检测与 UI 逻辑已跨平台就绪，留待后续按"混合模式"补全平台分支。
- **更新说明渲染**：第一版纯文本展示 release body，不渲染 markdown。
- **更新源**：硬编码 GitHub（`REPO` 常量），"官网兜底"留待后续抽象 provider。

---

## Self-Review（计划完成后核对）

**Spec 覆盖：**
- 启动后台检测 + 三按钮弹窗 → Task 6/11/13 ✓
- 立即更新→下载（进度）→校验→macOS 半自动安装→重启 → Task 7/9/13 ✓
- 关于页版本号 + 手动检查 → Task 11/13 ✓
- 方案 B 半自动（hdiutil/ditto/xattr/辅助脚本）+ 回退 A → Task 8/9 ✓
- SHA256 校验（缺失判失败）+ host 白名单 → Task 4/7 ✓
- 跳过此版本 / 24h 节流 / auto_check_update → Task 10/13 ✓
- CI 生成 checksums.txt → Task 14 ✓
- 仅 macOS、其它平台占位、更新源可配置常量 → Task 9（`#[cfg]`）、Task 1（`REPO` 常量）✓
- 修正 repository 占位符 → Task 1 ✓

**类型一致性：** `ReleaseInfo`/`InstallOutcome`/`check_for_update`/`download_and_verify`/`install`/`run_helper_and_exit` 在定义（Task 1/6/7/9）与调用（Task 13）处签名一致；slint 属性 `update-phase`/`app-version` 与 setter `set_update_phase`/`set_app_version` 对应；跨线程状态统一用 `Arc<Mutex>`（Task 13）。

**无占位符：** 所有步骤含可执行代码与命令；UI/网络/系统调用类用 `cargo build` + 手动验证清单替代单测，已显式说明。
