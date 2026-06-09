# 设计方案：版本更新检测 + macOS 自动下载安装

- 日期：2026-06-09
- 状态：已确认，待写实现计划
- 涉及模块：新增 `src/updater.rs`、`src/app.rs`（启动检查与回调）、`src/config.rs`（配置项）、`ui/app.slint`（更新对话框 + 关于页改造）、`lang/*`（i18n）、`.github/workflows/release.yml`（校验和）、`Cargo.toml`（依赖）

## 背景与目标

LibSSH 是 Rust + Slint 原生桌面应用（非 Tauri/Electron，无现成 updater 插件），通过 GitHub Releases（`github.com/qdlibra/LibSSH`）分发，发布流水线已高度自动化（改 `Cargo.toml` 版本号推 main → 自动打 tag → 建 Release，`--generate-notes` 自动生成更新说明）。

本功能给运行中的 App 增加两件事：

1. **版本更新检测**：检测到新版本后，弹出更新说明 + `[跳过此版本] / [稍后] / [立即更新]` 三个按钮。
2. **自动下载安装**：点"立即更新"后，从 GitHub Releases 下载对应架构的安装包并完成安装。

第一版**只实现 macOS**的下载安装链路；检测与提示逻辑天然跨平台，但安装的平台分支只填 macOS，其余平台留 `#[cfg]` 占位。

经与用户确认的关键决策：

1. 安装方式 → **混合模式**；落到第一版（仅 macOS）即 **macOS 半自动安装（方案 B）**：自动挂载 dmg、覆盖 `.app`、清除 quarantine，用户只需点"重启"。不可写位置自动降级为引导式（方案 A）。
2. 目标平台 → **仅 macOS**（Apple Silicon + Intel 两个架构按运行时自动选）。
3. 更新源 → **GitHub Releases**（`/repos/qdlibra/LibSSH/releases/latest`）。更新源在代码里留成可配置常量，便于将来加"官网兜底"。
4. 完整性校验 → **加 SHA256 校验**：改 `release.yml` 发布时生成 `checksums.txt`，App 下载后比对。
5. 更新弹窗按钮 → `[跳过此版本] / [稍后] / [立即更新]` 三个。

## 需求清单

1. 启动后台静默检测新版本；有新版本时弹出更新说明 + `[跳过此版本] / [稍后] / [立即更新]`。
2. 点"立即更新"后，自动下载最新安装包（带进度），校验后在 macOS 上半自动安装并提示重启。
3. 关于对话框显示当前版本号，并提供手动"检查更新"入口。

---

## 总体架构与端到端流程

```
启动 App ──(后台 tokio task，静默)──► GET api.github.com/repos/qdlibra/LibSSH/releases/latest
                                           │  （User-Agent + Accept: application/vnd.github+json）
                         解析 tag_name="v0.2.4" → semver 0.2.4
                         比较 vs env!(CARGO_PKG_VERSION)=0.2.3，且不等于 skipped_version
                                           │
                     ┌─────────────────────┴─────────────────────┐
                 已是最新 / 被跳过                              有新版本
                     │                                            │
            静默结束（手动检查时              invoke_from_event_loop → 主线程弹【更新对话框】
            回显"已是最新版本"）              ├ 新版本号 0.2.4（当前 0.2.3）
                                              ├ 更新说明（release body）
                                              └ [跳过此版本] [稍后] [立即更新]
                                                            │
                       ┌────────────────┬───────────────────┴────────────┐
                  [跳过此版本]        [稍后]                          [立即更新]
                  存 skipped_version  关闭，下次启动/手动              下载对应架构 dmg（进度条）
                  关闭，不再提示       再提示                                │
                                                            ┌─────────────┴──────────────┐
                                                        下载/校验失败                 成功 + SHA256 通过
                                                     update-phase=error                    │
                                                     [重试] [去发布页]            安装编排（macOS 方案 B）
                                                                                            │
                                                                            ┌───────────────┴───────────────┐
                                                                     可写位置（已安装）              不可写（开发/dmg 内运行）
                                                                     生成辅助脚本，update-phase=ready   降级方案 A：open dmg + 提示拖拽
                                                                     提示"更新就绪" [立即重启]
                                                                                │
                                                                  [立即重启] → spawn 分离辅助脚本 + 退出本进程
                                                                                │
                                                          辅助脚本：等待退出 → ditto 覆盖 .app → xattr 解隔离 → detach → open 新版
```

**线程模型**：检测与下载是异步 I/O，放后台 tokio task；Slint event loop 在主线程。沿用 `app.rs` 现有的「后台任务 + `Weak<AppWindow>` + `slint::invoke_from_event_loop` 回调」模式（参考现有 SSH 事件回写 UI 的写法），所有 `set_update_*` 必须在事件循环线程执行。

---

## 模块设计

### 1 — `src/updater.rs`（新增，单一职责）

对外暴露窄接口，核心逻辑（版本比较、asset 选择、checksums 解析）是可独立单测的纯函数：

```rust
/// 更新源（留成可配置，第一版只填 GitHub）。
const REPO: &str = "qdlibra/LibSSH";

pub struct ReleaseInfo {
    pub version: semver::Version,   // 解析后的新版本
    pub tag: String,                // "v0.2.4"
    pub notes: String,              // release body（markdown 原文）
    pub asset_url: String,          // 当前架构 dmg 的下载直链
    pub asset_name: String,         // "LibSSH-macos-arm64.dmg"
    pub asset_size: u64,
    pub checksums_url: Option<String>, // checksums.txt 的下载直链（若存在）
}

pub enum InstallOutcome {
    /// 方案 B 成功：辅助脚本已就绪，等用户点重启。
    ReadyToRestart,
    /// 降级方案 A：已打开 dmg，提示用户手动拖拽。
    GuidedManual,
}

/// 检测 + 比较；返回 None 表示已是最新或被跳过。
pub async fn check_for_update(current: &str, skipped: Option<&str>) -> Result<Option<ReleaseInfo>>;

/// 下载到 cache 目录，校验 SHA256；on_progress(已下载, 总量)。
pub async fn download_and_verify(rel: &ReleaseInfo, on_progress: impl Fn(u64, u64)) -> Result<PathBuf>;

/// 平台相关安装；第一版仅 #[cfg(target_os = "macos")] 有真实实现，其余返回 Unsupported。
pub fn install(dmg_path: &Path) -> Result<InstallOutcome>;

// —— 可单测的纯函数 ——
fn is_newer(current: &str, candidate_tag: &str) -> Result<bool>;       // semver 比较，容错前缀 'v'
fn pick_asset<'a>(assets: &'a [Asset], arch: &str) -> Option<&'a Asset>; // 按架构选 dmg
fn parse_checksums(text: &str, asset_name: &str) -> Option<String>;     // 从 checksums.txt 取该文件 sha256
fn target_arch_tag() -> &'static str;  // aarch64→"arm64"，x86_64→"x86_64"
```

不预先抽象 `Installer` trait——第一版只有 macOS，等真做第二个平台时才知道正确的抽象形状（YAGNI）。

### 2 — 版本检测与比较

- **API**：`GET https://api.github.com/repos/qdlibra/LibSSH/releases/latest`，必须带 `User-Agent`（GitHub 强制要求）与 `Accept: application/vnd.github+json`。
- **反序列化**（serde，仅取需要字段）：`tag_name: String`、`body: String`、`assets: [{ name, browser_download_url, size }]`。
- **版本比较**：`env!("CARGO_PKG_VERSION")`（当前 = `0.2.3`）；`tag_name` 去掉前缀 `v` 后用 `semver::Version` 解析比较，仅当 `latest > current` 视为有更新。
- **架构选择**：`std::env::consts::ARCH`（Apple Silicon = `aarch64`，Intel = `x86_64`）映射到产物命名 `arm64` / `x86_64`，匹配 asset `LibSSH-macos-<arch>.dmg`。
- **跳过版本**：若 `tag_name` 等于 `config.skipped_version`，视为无更新（手动检查时忽略此过滤，仍提示）。
- **节流**：记录 `last_update_check` 时间戳，距上次 < 24h 的**自动**检查直接跳过（手动检查不节流）。
- **未认证速率限制**（60 次/小时/IP）对低频更新检查无影响；命中时记日志、静默失败。

### 3 — 下载与校验

- HTTP 客户端：`reqwest`（`default-features = false`，启用 `rustls-tls` + `stream`），避免 OpenSSL 系统依赖、复用现有 tokio。
- 下载目录：`directories`（项目已依赖）的 cache dir，例如 `~/Library/Caches/<app>/updates/LibSSH-macos-arm64.dmg`；旧文件先清理。
- 进度：`response.bytes_stream()` 逐块写文件并累加，回调 `(downloaded, total)`，节流到 UI（约每 100ms 或每 1% 刷新一次）。
- **SHA256 校验**：先下 `checksums.txt`（小文件），`parse_checksums` 取本 asset 的期望值；下载完成后用 `sha2` 计算实际值比对。不一致 → 删除文件、报错、**不**进入安装。
- 安全：强制 HTTPS，校验下载 host 在白名单内（`api.github.com` / `objects.githubusercontent.com` / `github.com` / `*.githubusercontent.com`）。

### 4 — macOS 安装（方案 B 半自动，回退 A 引导）

```
install(dmg):
  1. 定位 .app bundle：
     exe = std::env::current_exe()              # …/LibSSH.app/Contents/MacOS/LibSSH
     bundle = 向上取第一个以 ".app" 结尾的祖先目录
     若不存在（cargo run 开发环境）→ 回退方案 A
  2. 可写性检查：
     若 bundle 不存在 或 其父目录不可写（系统保护位置）→ 回退方案 A
  3. 挂载：hdiutil attach -nobrowse -noverify -mountpoint <临时挂载点> <dmg>
     从挂载点找到 "LibSSH.app"
  4. 写辅助脚本（/bin/sh，写入私有临时目录，chmod 700），内容：
       a. 等待当前进程退出：while kill -0 <PID> 2>/dev/null; do sleep 0.2; done
       b. ditto "<挂载点>/LibSSH.app" "<bundle>"      # 覆盖安装，保留权限/资源分支
       c. xattr -dr com.apple.quarantine "<bundle>"   # 解除隔离 —— 真正规避 Gatekeeper
       d. hdiutil detach "<临时挂载点>"
       e. open "<bundle>"                             # 启动新版
  5. 返回 ReadyToRestart（脚本尚未执行；等用户点[立即重启]）

restart()（用户点[立即重启]）：
  - 用 Command 以分离方式（setsid/nohup 语义，stdin/out/err 重定向、不继承）spawn 步骤 4 的脚本
  - 然后本进程正常退出 → 脚本检测到退出 → 完成覆盖 + 启动新版

方案 A（回退，引导式）：
  - hdiutil attach（带 -browse 弹 Finder）或 `open <dmg>`
  - update-phase=ready，提示"请将 LibSSH 拖到「应用程序」覆盖旧版本"
```

**为何方案 B 反而更彻底解决未签名问题**：未签名/未公证应用，无论用户手动拖拽还是双击，下载来的 `.app` 都带 `com.apple.quarantine`，会触发 Gatekeeper 拦截（"来自身份不明的开发者"）。方案 B 在脚本里 `xattr -dr com.apple.quarantine` 主动解隔离，使新版可直接启动；纯引导（方案 A）做不到这点。

**自替换为何用辅助脚本**：进程不能可靠地覆盖正在运行的自身 bundle，最稳妥是「旧进程退出 → 外部脚本接管覆盖 + 重启」。脚本由旧进程在退出前分离 spawn，不随旧进程一起被收割。

### 5 — UI 设计

**更新对话框**（新增覆盖层，复用 About 对话框的居中 + 暗背景遮罩样式，参考 `ui/app.slint:693` 的 About 弹窗与 `:103` 的 `about-open` 模式）。

新增 `AppWindow` 属性（in-out）：

- `update-open: bool`
- `update-version: string`（"0.2.4"）、`update-current: string`（"0.2.3"）
- `update-notes: string`（release notes）
- `update-phase: string`：`prompt` / `downloading` / `verifying` / `ready` / `error`
- `update-progress: float`（0.0–1.0）
- `update-error: string`

新增回调：`update-confirm()`（立即更新）、`update-later()`、`update-skip()`、`update-restart()`、`update-retry()`、`update-open-release()`（去发布页）。

按 `update-phase` 切换底部按钮/内容：

| phase | 内容 | 按钮 |
|---|---|---|
| prompt | 版本号 + 可滚动更新说明 | `[跳过此版本] [稍后] [立即更新]` |
| downloading | 进度条 + "下载中 45%" | `[取消]` |
| verifying | "校验中…" | （无） |
| ready | "更新已就绪 / 拖拽提示" | `[立即重启]`（B）或 `[完成]`（A 引导） |
| error | 错误信息 | `[重试] [去发布页]` |

**关于对话框改造**（`ui/app.slint` About 弹窗内）：

- 显示当前版本号：新增 `AppWindow` 属性 `app-version: string`，启动时由 Rust 用 `env!("CARGO_PKG_VERSION")` 设入；在 About 弹窗标题下渲染 `v{app-version}`。
- 新增"检查更新"按钮 → 触发 `check-update-manual()` 回调（手动检查：不节流、不过滤已跳过版本，"已最新"也回显）。

**i18n**：UI 静态文案走 Slint `@tr("...")` 并补 `lang/zh|en/LC_MESSAGES/LibSSH.po`（发现新版本 / 跳过此版本 / 稍后 / 立即更新 / 下载中 / 校验中 / 更新就绪 / 立即重启 / 重试 / 去发布页 / 检查更新 / 已是最新版本 / 请拖拽到应用程序 等）；Rust 侧动态文案（含变量的状态串）走 `i18n::t(zh, en)`（`src/i18n.rs:31`）。

### 6 — 配置项（`src/config.rs`）

在现有配置结构中新增字段（serde，向后兼容默认值）：

- `auto_check_update: bool`（默认 `true`）—— 关掉后启动不自动检查，仍可手动。
- `last_update_check: Option<i64>`（unix 秒）—— 24h 自动检查节流。
- `skipped_version: Option<String>`（如 `"v0.2.4"`）—— "跳过此版本"写入。

### 7 — 启动接线（`src/app.rs`）

- 在 `app::run()` 构建窗口后、`window.run()` 前：设 `app-version`；若 `auto_check_update` 且未命中节流，spawn 后台 task 调 `updater::check_for_update`，有结果 → `invoke_from_event_loop` 打开更新对话框。
- 绑定上述 6 个更新回调 + `check-update-manual`，与现有回调注册集中在一处（参考 `initialise_models` 与各 `window.on_*` 注册段）。
- 下载/安装的进度与阶段切换，全部经 `Weak<AppWindow>` + `invoke_from_event_loop` 回写。

---

## CI / 发布流水线改动（`.github/workflows/release.yml`）

在 `publish` job（`:199–223`）下载完所有 artifacts、发布之前，生成 `checksums.txt` 并随 release 一起上传：

- 用 `sha256sum`（publish 跑在 `ubuntu-latest`）对所有产物算校验和，输出标准 `<sha256>  <文件名>` 格式到 `checksums.txt`（文件名只保留 basename）。
- 把 `checksums.txt` 加入 `gh release create` / `gh release upload` 的文件列表。
- 不改动各平台 build/package 步骤。

## 其他改动

- **`Cargo.toml`**：把占位的 `repository = "https://github.com/your/LibSSH"`（`:8`）改为真实 `https://github.com/qdlibra/LibSSH`；新增依赖 `reqwest`（rustls-tls + stream，禁用默认 features）、`semver`、`sha2`。
- **`README.md`**：补一节"自动更新"说明（可选，文档收尾时做）。

---

## 错误处理与回退矩阵

| 场景 | 行为 |
|---|---|
| 检查网络失败/超时 | 自动检查：静默记日志；手动检查：回显"检查失败，请稍后重试" |
| API 解析失败 / 无匹配架构 asset | 记日志；手动时提示"未找到适配的安装包" |
| `checksums.txt` 缺失 | 判更新失败（保守，见默认决策 4），提示"无法校验完整性，请去发布页手动下载" + `[去发布页]` |
| SHA256 不匹配 | 删除文件、`phase=error`、`[重试] [去发布页]` |
| 下载失败/中断 | `phase=error`、`[重试] [去发布页]` |
| 当前 app 在只读位置 / 开发环境 | 降级方案 A 引导式，提示手动拖拽 |
| hdiutil 挂载/ditto 失败 | `phase=error`，回退"打开 dmg 让用户手动安装" |
| 磁盘空间不足 | 下载前按 `asset_size` 预检，不足则提示 |
| 用户点[取消]（下载中） | 中止下载、删临时文件、回到 prompt 或关闭 |

## 安全考量（自动更新是高危攻击面）

- 全链路强制 HTTPS；下载 host 白名单校验。
- SHA256 完整性校验（缺校验和默认判失败）。
- 辅助脚本写入仅当前用户可读写的私有临时目录（`chmod 700`），内容由程序生成、不含任何来自网络的可执行片段（路径来自本地定位结果，做 shell 转义）。
- 不在更新流程中执行任何"由 release 数据指定的命令"——安装步骤是固定逻辑。
- 第一版不做代码签名/公证、GPG/minisign 签名校验（见 YAGNI），但 SHA256 + HTTPS 是基线。

## 测试策略

- **Rust 单元测试**（`cargo test`，纯函数，不打真实网络）：
  - `is_newer`：`0.2.3` vs `0.2.4`（真）、相等（假）、更低（假）、带/不带 `v` 前缀、非法 tag（错误）、pre-release 边界。
  - `pick_asset`：给定 assets 列表 + 架构，选对 `arm64` / `x86_64` dmg；无匹配返回 None。
  - `parse_checksums`：从样例 `checksums.txt` 文本取对应文件 sha256；文件不在表内返回 None。
  - `target_arch_tag`：架构映射。
  - API 反序列化：用固定 JSON 字符串测 serde 结构。
- **`cargo build`** 必须通过（含 slint 编译）。
- **手动验证**（由用户执行）：真实 GitHub release 的检测/下载/进度/校验；macOS 半自动安装在已安装到 `/Applications` 的情形下覆盖 + 解隔离 + 重启；不可写位置降级引导；更新对话框各 phase 与中英文案。

## 不在范围内（YAGNI）

- Windows / Linux 的下载安装（`install` 留 `#[cfg]` 占位，返回 Unsupported）。
- 官网兜底更新源（更新源已留成可配置常量，未来再加 provider 抽象）。
- 增量 / 差量更新、回滚、完全静默后台自动安装。
- 代码签名 / 公证、GPG / minisign 签名校验。
- 应用内渲染 release notes 的完整 markdown（第一版纯文本/简单换行即可）。

## 默认决策（可调）

1. HTTP 客户端：`reqwest` + `rustls-tls`（在意体积可换 `ureq`）。
2. 自动检查节流：24 小时。
3. 下载目录：`directories` cache dir 下的 `updates/`。
4. `checksums.txt` 缺失：判更新失败（保守）。
5. 更新对话框：复用 About 遮罩样式，单一覆盖层 + `phase` 状态机。
6. 更新说明：第一版纯文本展示 release body。

## 涉及文件清单

- `src/updater.rs`（新增）：检测 / 下载 / 校验 / macOS 安装编排 + 纯函数。
- `src/app.rs`：启动检查接线；设 `app-version`；6 个更新回调 + `check-update-manual`；进度/阶段经 `invoke_from_event_loop` 回写。
- `src/config.rs`：新增 `auto_check_update` / `last_update_check` / `skipped_version`。
- `src/i18n.rs`：Rust 侧动态状态文案（沿用 `t`）。
- `ui/app.slint`：更新对话框覆盖层 + 属性/回调；About 弹窗加版本号与"检查更新"按钮。
- `lang/zh|en/LC_MESSAGES/LibSSH.po`：更新相关 UI 文案中英翻译。
- `.github/workflows/release.yml`：`publish` job 生成并上传 `checksums.txt`。
- `Cargo.toml`：修正 `repository`；新增 `reqwest` / `semver` / `sha2`。
- `README.md`：补"自动更新"说明（收尾，可选）。
