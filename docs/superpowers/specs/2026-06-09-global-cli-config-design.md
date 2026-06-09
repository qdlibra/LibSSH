# 设计：设置菜单「全局 CLI」开关

- 日期：2026-06-09
- 状态：待评审
- 主题：在 GUI 设置中新增「全局 CLI 配置」，一键把 LibSSH 二进制软链接到 PATH，使 `LibSSH skill …` 可全局调用

## 背景与动机

LibSSH 的 CLI 与 GUI 是**同一个二进制**：`main()` 在 [src/main.rs](../../../src/main.rs) 中检查首个参数是否为 `skill`，是则走 `cli::run_args` 并退出，否则启动 GUI。CLI 主要供本机 AI 编码工具（Claude Code、Codex 等）调用——它们以非交互子进程方式运行，**读不到 shell alias**，因此必须让 `LibSSH` 命令出现在 PATH 中才能稳定调用。

目前用户得手动 `ln -sf target/release/LibSSH ~/.local/bin/LibSSH`。本设计把这一步收进 GUI 设置面板，做成一个可逆的状态开关。

## 目标

- 设置齿轮菜单新增一项，一键在 `~/.local/bin` 建立/移除指向当前二进制的符号链接。
- 状态自检：菜单文案反映当前是「未链接 / 已链接 / 链接失效」。
- 零权限摩擦：全程不需要 sudo / 管理员授权。
- 操作有明确反馈；若目标目录不在 PATH，提示用户如何加入。

## 非目标（YAGNI，本次明确不做）

- Windows 支持：本项在 Windows 上**整行隐藏**（不显示、不实现）。
- `/usr/local/bin` 等需要授权的系统级目录。
- 让用户在 UI 里自定义安装目录（固定 `~/.local/bin`）。
- 自动改写用户的 `.zshrc/.bashrc` 把 `~/.local/bin` 写进 PATH（只提示，不代写）。

## 详细设计

### 1. 用户体验

设置菜单（右上角齿轮弹出，见 [ui/app.slint](../../../ui/app.slint) 中 `settings-open` 控制的 popup）新增一行，**位置在 Language 与 About 之间**。沿用现有菜单项的 `Rectangle + TouchArea 整行点击` 模式，不引入新的 Switch 控件。行内图标用一个终端/链接类 Material Icons 字形（如 `terminal` 或 `link`），实现时确认 [ui/fonts](../../../ui/fonts) 的图标字体包含该字形。

文案与行为按状态切换：

| 状态 | 菜单文案 | 点击行为 |
|---|---|---|
| `NotLinked` | 启用全局 CLI | 建立链接 |
| `Linked` | 全局 CLI 已启用 · 点击移除 | 移除链接 |
| `Stale` | 重新链接全局 CLI | 重建链接（覆盖旧链接） |

`Stale` = 链接存在但指向的不是当前正在运行的二进制（例如二进制被移动、或换了 debug/release 路径）。

**操作反馈**：点击后在菜单项下方用一行 `Text`（`cli-link-feedback` property）显示结果——成功含链接路径，失败含原因。不弹独立对话框，保持在设置菜单内。

**PATH 提示**：建链成功后，若检测到 `~/.local/bin` 不在 PATH，追加一行可复制提示：
```
~/.local/bin 不在 PATH。请将这行加入 ~/.zshrc：
export PATH="$HOME/.local/bin:$PATH"
```
注意：GUI 进程（尤其 macOS 下双击 `.app` 由 launchd 启动）继承的 PATH 通常比终端精简，**此检测仅用于提示，不阻断建链操作**。

### 2. 后端 API（集中在 [src/system.rs](../../../src/system.rs)）

跟现有 `detect_dark_mode()` 一样用 `#[cfg(...)]` 做平台分支。核心逻辑写成**可注入目录与二进制路径的纯函数**，便于单测；对外再包一层读取真实 `current_exe()` / `~/.local/bin` 的薄封装。

```rust
/// 全局 CLI 链接的当前状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliLinkStatus {
    NotLinked,
    Linked,
    Stale,        // 链接存在但指向别的二进制
}

/// 建链/移除后的结果，带给 UI 反馈用。
pub struct CliLinkOutcome {
    pub link_path: PathBuf,   // ~/.local/bin/LibSSH
    pub in_path: bool,        // ~/.local/bin 是否在 PATH（仅提示用）
}

// —— 对外封装（解析真实路径后委托给纯函数）——
#[cfg(unix)] pub fn cli_link_status() -> CliLinkStatus;
#[cfg(unix)] pub fn enable_cli_link() -> anyhow::Result<CliLinkOutcome>;
#[cfg(unix)] pub fn disable_cli_link() -> anyhow::Result<()>;

// —— 纯函数（单测目标，dir/exe 注入）——
#[cfg(unix)] fn link_status_in(link_path: &Path, current_exe: &Path) -> CliLinkStatus;
#[cfg(unix)] fn enable_link_at(link_path: &Path, current_exe: &Path) -> anyhow::Result<()>;
#[cfg(unix)] fn disable_link_at(link_path: &Path) -> anyhow::Result<()>;

/// ~/.local/bin 是否在 PATH（split ':'，仅提示用）。
pub fn local_bin_in_path() -> bool;
```

行为细节：

- **目标路径**：`~/.local/bin/LibSSH`。HOME 用 `directories`（已是依赖）或 `std::env::var("HOME")` 解析。
- **链接源**：`std::env::current_exe()`——当前运行的二进制。打包成 `.app` 时即 `…/LibSSH.app/Contents/MacOS/LibSSH`，链接到它仍能跑 CLI（同一二进制带 `skill` 参数走 CLI 分支）。
- **enable**：`create_dir_all(~/.local/bin)` → 用 `std::os::unix::fs::symlink` **原子替换**：先建到临时名（如 `LibSSH.tmp-<pid>`）再 `fs::rename` 覆盖目标，避免出现半成品链接。
- **disable**：删除前**校验**目标确为「指向 LibSSH 二进制的 symlink」（`symlink_metadata` 确认是 symlink + `read_link` 的文件名为 `LibSSH`）。**绝不删除普通同名文件**——若不是我们的链接，返回错误并提示用户手动处理。
- **status**：`symlink_metadata` 判断是否 symlink；`read_link` 比对 target 是否等于 `current_exe()`（做路径 canonicalize 后比较，容忍符号链接/相对路径差异）。

### 3. UI ↔ Rust 绑定

**[ui/app.slint](../../../ui/app.slint)**：

```slint
// 顶层属性
in-out property <int>  cli-link-state: 0;      // 0=NotLinked 1=Linked 2=Stale
in-out property <bool> cli-link-supported: true; // Windows 下置 false -> 整行隐藏
in-out property <bool> cli-in-path: true;
in-out property <string> cli-link-feedback;    // 操作结果反馈文字
callback toggle-cli-link();
```

- 新菜单项 `Rectangle` 设 `visible: root.cli-link-supported;`，并让 popup 高度随可见项数变化（可见时 +28px；Windows 隐藏时保持原高度）。
- 项内 `Text` 文案按 `cli-link-state` 三态切换（用 Slint 条件表达式或在 Rust 侧算好塞 property）。
- 反馈行 `Text { text: root.cli-link-feedback; visible: root.cli-link-feedback != ""; }`。

**[src/app.rs](../../../src/app.rs)**（沿用 `on_set_language` 的回调模式，约 1374–1390 行附近）：

- 启动初始化：
  ```rust
  #[cfg(unix)] {
      w.set_cli_link_supported(true);
      w.set_cli_link_state(system::cli_link_status() as i32);
      w.set_cli_in_path(system::local_bin_in_path());
  }
  #[cfg(not(unix))] w.set_cli_link_supported(false);
  ```
- `window.on_toggle_cli_link(move || { … })`：依当前状态调用 `enable_cli_link()` / `disable_cli_link()`，成功后刷新 `cli_link_state` / `cli_in_path`，并写 `cli_link_feedback`（成功含路径，必要时含 PATH 提示；失败含原因）。文案走 `i18n::t(zh, en)`。

> 说明：状态真相源是**文件系统里的 symlink**，不在 [src/config.rs](../../../src/config.rs) 另存开关位，避免配置与实际链接状态不一致。本功能因此**不改 `ConfigFile`**。

### 4. i18n（[lang/zh|en/LC_MESSAGES/LibSSH.po](../../../lang)）

静态文案用 `@tr()`；三态/反馈等动态文案在 Rust 侧用 `i18n::t(zh, en)`。需要新增的中文/英文串（示意，最终以实现为准）：

- 启用全局 CLI / Enable global CLI
- 全局 CLI 已启用 · 点击移除 / Global CLI enabled · click to remove
- 重新链接全局 CLI / Re-link global CLI
- 已链接到 {path} / Linked at {path}
- 已移除全局 CLI 链接 / Global CLI link removed
- ~/.local/bin 不在 PATH，请加入… / ~/.local/bin is not on PATH, add…
- 失败：{reason} / Failed: {reason}

### 5. 平台处理

- `#[cfg(unix)]`（macOS + Linux）：完整实现。
- `#[cfg(not(unix))]`（Windows）：`cli_link_supported = false` → 菜单整行隐藏；`system.rs` 中 enable/disable/status 等函数全部带 `#[cfg(unix)]`，Windows 下不编译，UI 也永不触发它们。

## 安全与边界

- **误删防护**：disable 仅删「指向 LibSSH 的 symlink」，普通文件一律不动。
- **原子性**：enable 用临时名 + rename，杜绝半成品链接。
- **current_exe 失败**：返回错误并在反馈区提示，不静默吞掉。
- **目录缺失**：`~/.local/bin` 不存在则 `create_dir_all`。
- **PATH 检测局限**：GUI 继承 PATH 可能不全，检测结果只用于「提示」，从不阻断建链；即使显示"在 PATH"也以用户终端实际为准。

## 测试计划

`#[cfg(test)]` 于 [src/system.rs](../../../src/system.rs)，对纯函数用 `tempdir`：

1. `enable_link_at` 在空目录建链 → `link_status_in` 返回 `Linked`。
2. 目标已是指向**其他**路径的 symlink → `link_status_in` 返回 `Stale`；`enable_link_at` 能覆盖重建为 `Linked`。
3. `disable_link_at` 删除自建链接后 → `NotLinked`。
4. 目标是**普通文件**（非 symlink）→ `disable_link_at` 返回错误且文件仍在。
5. 无链接时 `link_status_in` 返回 `NotLinked`。

（symlink 相关测试用 `#[cfg(unix)]` 守卫。）

## 涉及文件改动清单

| 文件 | 改动 |
|---|---|
| [src/system.rs](../../../src/system.rs) | 新增 `CliLinkStatus`/`CliLinkOutcome`、enable/disable/status/纯函数 + 单测 |
| [ui/app.slint](../../../ui/app.slint) | 新增 4 个 property + 1 个 callback；菜单加一行（`visible` 绑定 + 高度自适应） |
| [src/app.rs](../../../src/app.rs) | 启动初始化 cli 链接状态；绑定 `on_toggle_cli_link` |
| [lang/zh/LC_MESSAGES/LibSSH.po](../../../lang) | 新增中文翻译串 |
| [lang/en/LC_MESSAGES/LibSSH.po](../../../lang) | 新增英文翻译串 |

不改 `config.rs`、不改 `cli.rs`。
