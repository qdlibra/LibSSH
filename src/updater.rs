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

/// 查询 GitHub 最新 release 并决定是否有可供本机安装的更新。
/// 返回 Ok(None) 表示已是最新 / 被跳过 / 本架构无产物。
pub async fn check_for_update(
    current: &str,
    skipped: Option<String>,
    manual: bool,
) -> Result<Option<ReleaseInfo>> {
    let arch =
        target_arch_tag().ok_or_else(|| anyhow!("unsupported architecture for auto-update"))?;
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

/// 仅允许 https + GitHub 官方下载域名（含 *.githubusercontent.com 子域）。
fn is_allowed_host(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    // authority = 第一个 '/' 之前的部分。白名单是裸主机名，因此带 userinfo('@')
    // 或端口(':')的 authority 一律拒绝——否则 `api.github.com:8080` 或
    // `api.github.com@evil.com` 这类伪装会绕过白名单。
    let authority = rest.split('/').next().unwrap_or("");
    if authority.contains('@') || authority.contains(':') {
        return false;
    }
    let host = authority;
    const EXACT: &[&str] = &[
        "api.github.com",
        "github.com",
        "objects.githubusercontent.com",
        "codeload.github.com",
    ];
    EXACT.contains(&host) || host.ends_with(".githubusercontent.com")
}

/// 下载 dmg 到 dest_dir，校验 SHA256（缺 checksums 判失败）。
/// on_progress(已下载字节, 总字节)；总字节未知时回传 asset_size。
/// 下载前预检磁盘空间；下载过程中轮询 `cancel`，置位即中止并清理半成品。
pub async fn download_and_verify(
    rel: &ReleaseInfo,
    dest_dir: &Path,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    on_progress: impl Fn(u64, u64),
) -> Result<PathBuf> {
    use futures::StreamExt;
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncWriteExt;

    if !is_allowed_host(&rel.asset_url) {
        bail!(
            "refusing to download from untrusted host: {}",
            rel.asset_url
        );
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

    // 2) 下载前磁盘空间预检（需 asset_size + 10% 余量）。查询失败则跳过预检。
    tokio::fs::create_dir_all(dest_dir).await?;
    if let Some(avail) = available_space(dest_dir) {
        let needed = rel.asset_size.saturating_add(rel.asset_size / 10);
        if avail < needed {
            bail!(
                "insufficient disk space for {}: need ~{needed} bytes, {avail} available",
                rel.asset_name
            );
        }
    }

    // 3) 流式下载到文件 + 进度；4) 读回校验 SHA256。
    // 任何失败（取消、网络中断、写盘、校验不符）都清理半成品文件，避免 cache 残留坏 dmg。
    let dest = dest_dir.join(&rel.asset_name);
    let result: Result<()> = async {
        let resp = client
            .get(&rel.asset_url)
            .send()
            .await?
            .error_for_status()?;
        let total = resp.content_length().unwrap_or(rel.asset_size);
        let mut file = tokio::fs::File::create(&dest).await?;
        let mut downloaded: u64 = 0;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            if cancel.load(Ordering::Relaxed) {
                bail!("download cancelled");
            }
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;
            on_progress(downloaded, total);
        }
        file.flush().await?;
        drop(file);

        let bytes = tokio::fs::read(&dest).await?;
        if sha256_hex(&bytes).to_lowercase() != expected.to_lowercase() {
            bail!("checksum mismatch for {}", rel.asset_name);
        }
        Ok(())
    }
    .await;

    match result {
        Ok(()) => Ok(dest),
        Err(e) => {
            let _ = tokio::fs::remove_file(&dest).await;
            Err(e)
        }
    }
}

/// dest_dir 所在文件系统的可用字节（查询失败返回 None，跳过预检）。
fn available_space(dir: &Path) -> Option<u64> {
    fs2::available_space(dir).ok()
}

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

    let mount_point =
        std::env::temp_dir().join(format!("LibSSH-update-mnt-{}", std::process::id()));
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
        let _ = Command::new("/usr/bin/hdiutil")
            .arg("detach")
            .arg(&mount_point)
            .status();
        open_dmg(dmg_path);
        return Ok(InstallOutcome::GuidedManual);
    }

    let script_path = std::env::temp_dir().join(format!("LibSSH-update-{}.sh", std::process::id()));
    let script = build_helper_script(
        std::process::id(),
        &mount_app,
        &bundle,
        &mount_point,
        &script_path,
    );
    std::fs::write(&script_path, script).context("failed to write helper script")?;
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700)) {
        tracing::warn!("failed to chmod 700 the update helper script: {e}");
    }

    Ok(InstallOutcome::ReadyToRestart {
        helper_script: script_path,
    })
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
        GhAsset {
            name: name.into(),
            browser_download_url: format!("https://x/{name}"),
            size: 1,
        }
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
        assert_eq!(
            pick_asset(&assets, "arm64").unwrap().name,
            "LibSSH-macos-arm64.dmg"
        );
        assert_eq!(
            pick_asset(&assets, "x86_64").unwrap().name,
            "LibSSH-macos-x86_64.dmg"
        );
        assert!(pick_asset(&assets, "riscv").is_none());
    }

    #[test]
    fn checksums_asset_url_finds_checksums_txt() {
        let assets = vec![asset("LibSSH-macos-arm64.dmg"), asset("checksums.txt")];
        assert_eq!(
            checksums_asset_url(&assets).as_deref(),
            Some("https://x/checksums.txt")
        );
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
        assert_eq!(
            parse_checksums(text, "LibSSH-macos-arm64.dmg").as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn sha256_hex_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
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
        assert_eq!(
            info.asset_url,
            "https://objects.githubusercontent.com/a.dmg"
        );
        assert_eq!(info.asset_size, 123);
        assert_eq!(
            info.checksums_url.as_deref(),
            Some("https://objects.githubusercontent.com/checksums.txt")
        );
        assert!(info.notes.contains("自动更新"));
        assert!(select_release_info(&rel, "riscv").is_none());
    }

    #[test]
    fn find_app_bundle_walks_up_to_dot_app() {
        let exe = Path::new("/Applications/LibSSH.app/Contents/MacOS/LibSSH");
        assert_eq!(
            find_app_bundle(exe),
            Some(PathBuf::from("/Applications/LibSSH.app"))
        );
        assert_eq!(
            find_app_bundle(Path::new("/home/q/proj/target/release/LibSSH")),
            None
        );
        assert_eq!(
            find_app_bundle(Path::new("/Applications/LibSSH.app")),
            Some(PathBuf::from("/Applications/LibSSH.app"))
        );
    }

    #[test]
    fn build_helper_script_quotes_paths_and_has_steps() {
        let s = build_helper_script(
            4242,
            Path::new("/Volumes/LibSSH 1.0/LibSSH.app"),
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

    #[test]
    fn is_allowed_host_requires_https_and_whitelist() {
        assert!(is_allowed_host(
            "https://api.github.com/repos/x/releases/latest"
        ));
        assert!(is_allowed_host(
            "https://objects.githubusercontent.com/a.dmg"
        ));
        assert!(is_allowed_host(
            "https://release-assets.githubusercontent.com/a.dmg"
        ));
        assert!(!is_allowed_host("http://api.github.com/x"));
        assert!(!is_allowed_host("https://evil.com/a.dmg"));
        assert!(!is_allowed_host("https://api.github.com.evil.com/x"));
        // 回归保护：端口 / userinfo / 裸 apex 不得绕过白名单。
        assert!(!is_allowed_host("https://api.github.com:8080/x"));
        assert!(!is_allowed_host("https://api.github.com@evil.com/x"));
        assert!(!is_allowed_host("https://githubusercontent.com/x"));
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
