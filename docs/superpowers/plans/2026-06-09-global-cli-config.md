# 全局 CLI 配置 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在设置齿轮菜单新增一行可逆开关，一键把当前 LibSSH 二进制软链接到 `~/.local/bin`，让 `LibSSH skill …` 可全局调用。

**Architecture:** 链接逻辑集中在 `src/system.rs` 的 `#[cfg(unix)]` 子模块，核心写成可注入路径的纯函数便于单测；UI 在 `ui/app.slint` 设置菜单加一项 + 反馈行；`src/app.rs` 负责启动时初始化状态、绑定点击回调。状态真相源是文件系统里的 symlink，不写入 config。Windows 整行隐藏、后端不编译。

**Tech Stack:** Rust（`std::os::unix::fs::symlink`、`std::fs`、`anyhow`）、Slint、现有 `i18n` 模块。

**Commit 约定：** 本仓库 commit 信息用中文 conventional-commits 风格（见 `git log`）。每条 commit 末尾追加：
```
Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
```

---

## File Structure

| 文件 | 职责 | 改动 |
|---|---|---|
| `src/system.rs` | 新增 `cli_link` 子模块：管理全局 CLI 符号链接（状态/建链/移除/PATH 检测）+ 单测 | 末尾追加，唯一职责清晰 |
| `ui/app.slint` | 设置菜单加一项 + 反馈行；新增 4 个 property + 1 个 callback；popup 高度自适应 | 局部插入 |
| `src/app.rs` | 启动初始化链接状态；绑定 `on_toggle_cli_link` | 两处插入 |

**不改动**：`src/config.rs`（不存开关位）、`src/cli.rs`（不碰 policy）、`lang/*.po`（菜单文案用 `lang-en` 内联三元，与现有 Language 项一致）。

**两个易错点（已在本计划规避）：**
- 测试模块放在 `cli_link` **内部**（`mod tests`），否则兄弟模块无法访问其私有纯函数（Rust 可见性）。
- 菜单项用 `if cond : Element {}` **条件实例化**，不用 `visible:`——Slint 的 `visible:false` 在 `VerticalLayout` 里仍占空间，会让高度算错。

---

## Task 1: `system.rs` 链接核心逻辑（纯函数 + 单测，TDD）

**Files:**
- Modify: `src/system.rs`（在文件末尾、现有 `#[cfg(test)] mod tests` 之后追加新模块）

对外封装（`cli_link_status`/`enable_cli_link`/`disable_cli_link`/`local_bin_in_path`）解析真实 `~/.local/bin` 与 `current_exe()` 后委托模块私有纯函数（`link_status_in`/`enable_link_at`/`disable_link_at`）。测试置于 `cli_link` 模块**内部** `mod tests`，用 `std::env::temp_dir()` 临时目录，零新增依赖。

- [ ] **Step 1: 写模块骨架（三个纯函数体先 `todo!()`）+ 完整测试**

在 `src/system.rs` **末尾**追加：

```rust
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

    // ---- 纯函数：注入路径，便于单测（本步先 todo!()，Step 3 填实现）----

    fn link_status_in(_link_path: &Path, _current_exe: &Path) -> CliLinkStatus {
        todo!()
    }

    fn enable_link_at(_link_path: &Path, _current_exe: &Path) -> Result<()> {
        todo!()
    }

    fn disable_link_at(_link_path: &Path) -> Result<()> {
        todo!()
    }

    #[cfg(test)]
    mod tests {
        use super::{disable_link_at, enable_link_at, link_status_in, CliLinkStatus};
        use std::path::PathBuf;

        // 每个测试用独立临时目录，避免并行冲突；用例名作为 tag。
        fn temp_dir(tag: &str) -> PathBuf {
            let d = std::env::temp_dir()
                .join(format!("libssh-clilink-{}-{}", tag, std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        // 造一个“真实存在”的假二进制，canonicalize 才能解析。
        fn fake_exe(dir: &PathBuf, name: &str) -> PathBuf {
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
```

- [ ] **Step 2: 跑测试，确认红（`todo!()` 触发）**

Run: `cargo test cli_link 2>&1 | tail -20`
Expected: 测试**编译通过但失败**，panic 信息含 `not yet implemented`。（若是编译错误而非 panic，说明签名/可见性写错，回查 Step 1。）

- [ ] **Step 3: 用真实实现替换三个纯函数的 `todo!()`**

把 Step 1 中三个 `todo!()` 函数体替换为：

```rust
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
```

- [ ] **Step 4: 跑测试，确认绿**

Run: `cargo test cli_link 2>&1 | tail -20`
Expected: `test result: ok. 5 passed`（5 个 `cli_link::tests::*`）。

- [ ] **Step 5: 整体编译**

Run: `cargo build 2>&1 | tail -5`
Expected: 编译成功，无阻断 warning。

- [ ] **Step 6: Commit**

```bash
git add src/system.rs
git commit -m "feat(cli): 全局 CLI 软链接核心逻辑与单测

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `app.slint` 设置菜单项与反馈行

**Files:**
- Modify: `ui/app.slint`（property 区 ~127 行；设置菜单 popup 543–625 行）

无单测，靠 `cargo build`（slint 宏编译）+ Task 4 手动验证。

- [ ] **Step 1: 新增 property 与 callback**

在 `ui/app.slint` 第 127 行 `callback set-language(string);` 之后插入：

```slint
    // --- Global CLI symlink (设置菜单「全局 CLI」) -------------------------
    in-out property <int>  cli-link-state: 0;        // 0=未链接 1=已链接 2=失效
    in-out property <bool> cli-link-supported: true; // Windows 下置 false → 整行隐藏
    in-out property <bool> cli-in-path: true;        // ~/.local/bin 是否在 PATH（仅提示）
    in-out property <string> cli-link-feedback;      // 操作结果反馈（多行）
    callback toggle-cli-link();
```

- [ ] **Step 2: 设置菜单 popup 高度改为自适应**

在 `ui/app.slint` 找到设置菜单 popup（约 543–547 行）：

```slint
    Rectangle {
        x: parent.width - 196px;
        y: 40px;
        width: 198px;
        height: 104px;
        visible: root.settings-open;
```

把固定 `height: 104px;` 改为：

```slint
        height: root.cli-link-supported
            ? (root.cli-link-feedback != "" ? 196px : 133px)
            : 104px;
```

（Windows 隐藏 CLI 行 → 104px=3 项；显示 CLI 行 → 133px=4 项；带反馈 → 196px 容纳多行 PATH 提示。高度与下方 `if` 实例化的项数严格对应。）

- [ ] **Step 3: 插入「全局 CLI」菜单项与反馈行（用 `if` 条件实例化）**

在 `ui/app.slint` 的 Language 项 `Rectangle { … }`（583–603 行）与 About 项 `Rectangle { … }`（604–623 行）**之间**插入。注意用 `if … :` 而非 `visible:`，否则隐藏时仍占布局空间：

```slint
            // Global CLI symlink toggle（点击后不关闭菜单，以便显示反馈）。
            if root.cli-link-supported : Rectangle {
                height: 28px;
                border-radius: Theme.radius-sm;
                background: cli-ta.has-hover ? Theme.bg-hover : transparent;
                cli-ta := TouchArea {
                    mouse-cursor: pointer;
                    clicked => { root.toggle-cli-link(); }
                }
                HorizontalLayout {
                    padding-left: 8px; spacing: 8px;
                    Text { text: "\u{E157}"; font-family: "Material Icons"; // link
                           color: Theme.text-secondary; font-size: Theme.fs-sm;
                           vertical-alignment: center; }
                    Text {
                        text: root.cli-link-state == 1
                            ? (root.lang-en ? "Global CLI enabled · click to remove" : "全局 CLI 已启用 · 点击移除")
                            : root.cli-link-state == 2
                            ? (root.lang-en ? "Re-link global CLI" : "重新链接全局 CLI")
                            : (root.lang-en ? "Enable global CLI" : "启用全局 CLI");
                        color: Theme.text-primary; font-size: Theme.fs-md;
                        vertical-alignment: center; horizontal-stretch: 1;
                    }
                }
            }
            // CLI 操作结果反馈（仅在有内容时存在）。
            if root.cli-link-feedback != "" : Text {
                text: root.cli-link-feedback;
                color: Theme.text-secondary;
                font-size: Theme.fs-sm;
                wrap: word-wrap;
                horizontal-alignment: left;
            }
```

- [ ] **Step 4: 确认 slint 编译通过**

Run: `cargo build 2>&1 | tail -8`
Expected: 编译成功；无 slint 语法错误（未声明的 property/callback 会在此报错）。

- [ ] **Step 5: Commit**

```bash
git add ui/app.slint
git commit -m "feat(cli): 设置菜单新增全局 CLI 开关与反馈行

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `app.rs` 启动初始化与回调绑定

**Files:**
- Modify: `src/app.rs`（启动初始化 ~73 行；回调绑定 ~1390 行 `on_set_language` 之后）

- [ ] **Step 1: 启动时初始化链接状态**

在 `src/app.rs` 第 73 行 `window.set_lang_en(crate::i18n::is_en());` 之后插入：

```rust
    // 初始化「全局 CLI」开关状态（仅 Unix；Windows 整行隐藏）。
    #[cfg(unix)]
    {
        window.set_cli_link_supported(true);
        window.set_cli_link_state(crate::system::cli_link_status() as i32);
        window.set_cli_in_path(crate::system::local_bin_in_path());
    }
    #[cfg(not(unix))]
    window.set_cli_link_supported(false);
```

- [ ] **Step 2: 绑定 `on_toggle_cli_link` 回调**

在 `src/app.rs` 的 `wire_callbacks` 内、`on_set_language` 块结束（约第 1390 行的 `});`）之后插入：

```rust
    // 「全局 CLI」开关：建链/重链/移除 ~/.local/bin/LibSSH（仅 Unix）。
    #[cfg(unix)]
    {
        let weak = window.as_weak();
        window.on_toggle_cli_link(move || {
            let Some(w) = weak.upgrade() else {
                return;
            };
            let linked = w.get_cli_link_state() == 1; // 1=Linked → 移除；否则建/重链
            let feedback = if linked {
                match crate::system::disable_cli_link() {
                    Ok(()) => {
                        crate::i18n::t("已移除全局 CLI 链接", "Global CLI link removed").to_string()
                    }
                    Err(err) => {
                        if crate::i18n::is_en() {
                            format!("Failed: {err}")
                        } else {
                            format!("失败：{err}")
                        }
                    }
                }
            } else {
                match crate::system::enable_cli_link() {
                    Ok(outcome) => {
                        w.set_cli_in_path(outcome.in_path);
                        let path = outcome.link_path.display();
                        if outcome.in_path {
                            if crate::i18n::is_en() {
                                format!("Linked at {path}")
                            } else {
                                format!("已链接到 {path}")
                            }
                        } else if crate::i18n::is_en() {
                            format!(
                                "Linked at {path}\n~/.local/bin is not on PATH. Add to ~/.zshrc:\nexport PATH=\"$HOME/.local/bin:$PATH\""
                            )
                        } else {
                            format!(
                                "已链接到 {path}\n~/.local/bin 不在 PATH，请加入 ~/.zshrc：\nexport PATH=\"$HOME/.local/bin:$PATH\""
                            )
                        }
                    }
                    Err(err) => {
                        if crate::i18n::is_en() {
                            format!("Failed: {err}")
                        } else {
                            format!("失败：{err}")
                        }
                    }
                }
            };
            // 重新读取文件系统状态作为真相源，并写回反馈。
            w.set_cli_link_state(crate::system::cli_link_status() as i32);
            w.set_cli_link_feedback(feedback.into());
        });
    }
```

- [ ] **Step 3: 编译确认**

Run: `cargo build 2>&1 | tail -8`
Expected: 编译成功。若报 `get_cli_link_state`/`set_cli_link_feedback`/`set_cli_in_path` 未找到，说明与 Task 2 的 property 名不一致——回查（slint 把 `cli-link-state` 生成 `get/set_cli_link_state`）。

- [ ] **Step 4: 全量测试，确认无回归**

Run: `cargo test 2>&1 | tail -15`
Expected: 所有测试通过（含 Task 1 的 5 个 `cli_link::tests` 与既有 `system`/`i18n` 测试）。

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(cli): 绑定全局 CLI 开关的初始化与点击回调

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: 手动验证（真机交互）

无代码改动；逐条执行并确认。

> ⚠️ 前置说明：此前可能已手动建过 `~/.local/bin/LibSSH`（指向 `target/release/LibSSH`）。用 `cargo run`（debug 二进制）打开时 `current_exe` 是 `target/debug/LibSSH`，与现有 release 链接不一致 → 菜单应显示 **「重新链接全局 CLI」**（Stale）。若想从干净状态测全流程，先 `rm -f ~/.local/bin/LibSSH`。

- [ ] **Step 1: 从干净状态启动**

Run:
```bash
rm -f ~/.local/bin/LibSSH
cargo run
```
点右上角齿轮 → 菜单出现「**启用全局 CLI**」一行（state=0）。

- [ ] **Step 2: 点击启用，核对链接与反馈**

点击「启用全局 CLI」：菜单**保持打开**，文案变为「**全局 CLI 已启用 · 点击移除**」，下方反馈显示 `已链接到 …/target/debug/LibSSH`（若该 GUI 进程 PATH 不含 `~/.local/bin`，附 `export PATH=…` 提示）。

另开终端核对：
```bash
ls -l ~/.local/bin/LibSSH               # symlink → …/target/debug/LibSSH
~/.local/bin/LibSSH skill policy show   # 打印 JSON，证明 CLI 全局可用
```
Expected: symlink 指向当前运行的二进制；`skill policy show` 输出 `{"enabled": …}` JSON。

- [ ] **Step 3: 点击移除，核对链接消失**

再次点击该行（此时是「点击移除」）：文案回到「启用全局 CLI」，反馈显示「已移除全局 CLI 链接」。
```bash
ls -l ~/.local/bin/LibSSH      # No such file or directory
```

- [ ] **Step 4: Stale（占位文件）验证**

```bash
printf 'x' > ~/.local/bin/LibSSH   # 同名普通文件占位
cargo run
```
打开设置菜单：应显示「**重新链接全局 CLI**」（Stale）。点击它会 `enable`（原子 rename 覆盖为我们的 symlink）→ 变「已启用」。
（`disable` 对普通文件的防误删分支由 Task 1 单测 `disable_refuses_plain_file` 覆盖，手动无需重复。）
清理：`rm -f ~/.local/bin/LibSSH`。

- [ ] **Step 5: Windows 隐藏确认（如有 Windows 环境，可选）**

Windows 上 `cargo run`：设置菜单**不出现** CLI 行；`cargo build` 不因 `#[cfg(unix)]` 报错。无 Windows 环境则跳过（`#[cfg]` 保证不编译该路径）。

---

## 验收对照（spec → task）

- 一键建/移除 `~/.local/bin` 符号链接 → Task 1（逻辑）+ Task 3（触发）
- 三态自检（未链接/已链接/失效）→ Task 1 `link_status_in` + Task 2 文案 + Task 3 初始化
- 零授权（用户级目录）→ Task 1 `local_bin()`，无 sudo
- 操作反馈 + 不在 PATH 提示 → Task 3 `feedback` + Task 2 反馈行
- 双语文案 → Task 2 菜单内联三元 + Task 3 反馈 `is_en()` 分支（故不动 `lang/*.po`）
- 原子建链 / 删前校验防误删 → Task 1 `enable_link_at` / `disable_link_at` + 单测
- 状态以文件系统为真相、不改 config → 全程不碰 `config.rs`
- Windows 隐藏、后端不编译 → Task 2 `if` 条件实例化 + Task 3 `#[cfg]` + Task 1 `#[cfg(unix)]`
