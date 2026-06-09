# SFTP 原生编辑器 + 标签/图标修复 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用原生 Slint 纯文本覆盖式编辑器替换坏掉的"下载+系统程序打开"，并修复关闭标签误弹"连接失败"框、文件夹图标 Windows 黄化。

**Architecture:** SFTP 后台线程新增 `ReadFile`/`WriteFile` 命令（用 russh-sftp 2.3 高层 `read()/write()`），读到内容经 UTF-8/大小守卫后通过新事件 `SftpFileContent` 推给 UI；AppWindow 新增满窗编辑覆盖层（仿 About 对话框），保存回调直接经 SFTP 写回远端。标签关闭用一个共享 `HashSet` 标记"用户主动关闭"以抑制误弹。

**Tech Stack:** Rust, Slint 1.8, russh-sftp 2.3, tokio。测试用 `cargo test`（纯函数）+ `cargo build` + 连真实 SSH 手动验证。

参考设计：[docs/superpowers/specs/2026-06-09-sftp-editor-and-ui-fixes-design.md](../specs/2026-06-09-sftp-editor-and-ui-fixes-design.md)

---

## 文件结构

| 文件 | 职责 | 改动 |
|---|---|---|
| `src/app.rs` | 窗口回调与事件分发 | 关闭抑制集合；`SftpFileContent` 分支；`on_sftp_edit` 改走 ReadFile；`on_editor_save/close`；移除 `on_sftp_view` |
| `src/sftp.rs` | SFTP 后台 worker | `check_editable` 纯函数+测试；`ReadFile`/`WriteFile` 命令与处理；移除 `OpenTemp`/`open_temp`/`open_with_os`/`spawn_edit_watcher` |
| `src/ssh.rs` | 会话事件枚举 | 新增 `SessionEvent::SftpFileContent` |
| `ui/app.slint` | 主窗口 | 编辑器属性/回调/满窗覆盖层；移除 `sftp-view` 转发 |
| `ui/sftp_panel.slint` | 文件管理器 | 菜单去"查看"、改弹窗高度；文件夹/文件图标拆分着色 |
| `ui/terminal_view.slint` | 会话视图 | 移除 `sftp-view` 回调与 `view` 转发 |
| `ui/theme.slint` | 设计令牌 | 新增 `folder-icon` 色 |

---

## Task 1: 关闭标签抑制"连接失败"弹窗（需求 1）

**Files:**
- Modify: `src/app.rs`（`apply_session_event_to_window` 签名与 `Closed` 分支 [src/app.rs:1672](../../../src/app.rs)、`on_tab_closed` [src/app.rs:964](../../../src/app.rs)、调用点）
- Test: `src/app.rs` 内 `#[cfg(test)]` 模块（已存在，含 `should_show_connection_failed_alert` 测试）

- [ ] **Step 1: 写失败测试** — 在 app.rs 测试模块新增纯函数 `should_alert_on_close` 的测试：

```rust
#[test]
fn user_close_suppresses_failure_alert() {
    // 用户主动关闭：永不弹窗，即使从未连上
    assert!(!should_alert_on_close(true, None));
    assert!(!should_alert_on_close(true, Some(0)));
    // 非用户关闭：沿用原逻辑
    assert!(should_alert_on_close(false, None));
    assert!(should_alert_on_close(false, Some(0)));
    assert!(!should_alert_on_close(false, Some(1)));
    assert!(!should_alert_on_close(false, Some(2)));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test should_alert_on_close`
Expected: 编译失败 `cannot find function should_alert_on_close`

- [ ] **Step 3: 实现纯函数** — 在 `should_show_connection_failed_alert`（[src/app.rs:312](../../../src/app.rs)）旁新增：

```rust
/// 是否应在断开时弹"连接失败"框。
/// `was_user_close` 为 true 表示用户主动关闭该标签，永不弹。
fn should_alert_on_close(was_user_close: bool, previous_state: Option<u8>) -> bool {
    !was_user_close && should_show_connection_failed_alert(previous_state)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test should_alert_on_close`
Expected: PASS

- [ ] **Step 5: 接入共享集合** — 在 `run()`/构建窗口处（`on_tab_closed` 定义之前，约 [src/app.rs:954](../../../src/app.rs)）创建集合，并克隆给 `on_tab_closed` 与事件分发：

```rust
use std::collections::HashSet;
let user_closing_tabs: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
```

在 `on_tab_closed` 闭包内、`if id == "welcome" { return; }` 之后、`handle.close()` 之前插入：

```rust
user_closing_tabs.lock().unwrap().insert(id.clone());
```

（注意：`on_tab_closed` 已捕获多个 `close_*` 克隆，照此模式 `let close_user_closing = user_closing_tabs.clone();` 再 move 进闭包。）

- [ ] **Step 6: 给事件分发函数加参数并在 Closed 分支使用** — 修改 `apply_session_event_to_window` 签名（[src/app.rs:1672](../../../src/app.rs)）增加末参：

```rust
fn apply_session_event_to_window(
    win: &AppWindow,
    tab_id: &str,
    event: SessionEvent,
    bufs: &TermBuffers,
    statuses: &TabStatuses,
    local: &LocalSnap,
    local_net_hist: &NetHist,
    user_closing: &Arc<Mutex<HashSet<String>>>,
) {
```

`Closed` 分支（[src/app.rs:1772](../../../src/app.rs)）改为：

```rust
let show_failure_alert = {
    let was_user_close = user_closing.lock().unwrap().remove(tab_id);
    let mut statuses = statuses.lock().unwrap();
    let previous_state = statuses.get(tab_id).map(|st| st.state);
    if let Some(st) = statuses.get_mut(tab_id) {
        st.state = 2;
    }
    should_alert_on_close(was_user_close, previous_state)
};
```

- [ ] **Step 7: 更新调用点** — `grep -n "apply_session_event_to_window(" src/app.rs` 找到调用处，把 `user_closing_tabs`（按需 `.clone()` 后引用）作为末参传入。该调用通常在事件循环闭包内，需 `let ev_user_closing = user_closing_tabs.clone();` 并在闭包中以 `&ev_user_closing` 传入。

- [ ] **Step 8: 编译并跑全部测试**

Run: `cargo build && cargo test`
Expected: 编译通过，测试全绿。

- [ ] **Step 9: Commit**

```bash
git add src/app.rs
git commit -m "fix(tabs): 用户主动关闭标签时不再误弹连接失败框"
```

---

## Task 2: 可编辑性守卫纯函数（需求 3/4 前置，TDD）

**Files:**
- Modify: `src/sftp.rs`（新增纯函数 + `#[cfg(test)]` 测试）

- [ ] **Step 1: 写失败测试** — 在 `src/sftp.rs` 末尾新增：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_editable_accepts_small_utf8() {
        assert_eq!(check_editable(b"hello", 1024).unwrap(), "hello");
        assert_eq!(check_editable(b"", 1024).unwrap(), "");
    }

    #[test]
    fn check_editable_rejects_too_large() {
        assert!(matches!(check_editable(b"abcd", 2), Err(EditableError::TooLarge)));
    }

    #[test]
    fn check_editable_rejects_non_utf8() {
        // 0xFF 不是合法 UTF-8 起始字节
        assert!(matches!(check_editable(&[0xff, 0xfe, 0x00], 1024), Err(EditableError::NotUtf8)));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test check_editable`
Expected: 编译失败 `cannot find ... EditableError / check_editable`

- [ ] **Step 3: 实现** — 在 `src/sftp.rs` 顶部 `SftpCommand` 枚举附近新增：

```rust
#[derive(Debug, PartialEq)]
pub enum EditableError {
    TooLarge,
    NotUtf8,
}

/// 远端文件内容是否适合在纯文本编辑器中打开。
/// 超过 `max_bytes` 或非 UTF-8 一律拒绝，避免保存时损坏文件。
#[allow(dead_code)] // Task 5 接线后移除
pub fn check_editable(bytes: &[u8], max_bytes: usize) -> Result<String, EditableError> {
    if bytes.len() > max_bytes {
        return Err(EditableError::TooLarge);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.to_string()),
        Err(_) => Err(EditableError::NotUtf8),
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test check_editable`
Expected: PASS（3 个测试）

- [ ] **Step 5: Commit**

```bash
git add src/sftp.rs
git commit -m "feat(sftp): 加可编辑性守卫纯函数 check_editable"
```

---

## Task 3: 文件夹图标 Windows 风格黄色（需求 2）

**Files:**
- Modify: `ui/theme.slint`（新增 `folder-icon`，[ui/theme.slint:38](../../../ui/theme.slint) 之后）
- Modify: `ui/sftp_panel.slint`（文件列表行图标，[ui/sftp_panel.slint:351-358](../../../ui/sftp_panel.slint)）

- [ ] **Step 1: theme 增加文件夹色** — 在 `danger` 行（[ui/theme.slint:38](../../../ui/theme.slint)）之后插入：

```slint
    // Windows-style manila folder yellow (亮暗主题同值)。
    out property <brush> folder-icon: #f7c948;
```

- [ ] **Step 2: 拆分文件列表图标为可着色字形** — 把 [ui/sftp_panel.slint:351-358](../../../ui/sftp_panel.slint) 这段：

```slint
                                Text {
                                    text: entry.is-dir ? "📁 " + entry.name : "📄 " + entry.name;
                                    color: entry.is-dir ? Theme.accent : Theme.text-primary;
                                    font-size: Theme.fs-sm;
                                    horizontal-stretch: 1;
                                    overflow: elide;
                                    vertical-alignment: center;
                                }
```

替换为「图标 Text + 文件名 Text」：

```slint
                                Text {
                                    text: entry.is-dir ? "\u{e2c7}" : "\u{e873}";
                                    font-family: "Material Icons";
                                    color: entry.is-dir ? Theme.folder-icon : Theme.text-muted;
                                    font-size: Theme.fs-md;
                                    vertical-alignment: center;
                                }
                                Text {
                                    text: entry.name;
                                    color: Theme.text-primary;
                                    font-size: Theme.fs-sm;
                                    horizontal-stretch: 1;
                                    overflow: elide;
                                    vertical-alignment: center;
                                }
```

- [ ] **Step 3: 编译**

Run: `cargo build`
Expected: 通过（slint 编译无误）。

- [ ] **Step 4: 手动验证** — 运行 app，连一台服务器打开 SFTP：文件夹显示黄色实心文件夹字形、文件显示灰色文档字形，文件名颜色不变；亮/暗主题都正常。

- [ ] **Step 5: Commit**

```bash
git add ui/theme.slint ui/sftp_panel.slint
git commit -m "feat(sftp): 文件夹图标改 Windows 风格黄色字形"
```

---

## Task 4: 编辑器满窗覆盖层 UI（需求 4）

**Files:**
- Modify: `ui/app.slint`（属性区 [ui/app.slint:92-94](../../../ui/app.slint)、回调区、覆盖层插入到 About 对话框之后 [ui/app.slint:759](../../../ui/app.slint)、`std-widgets` import 确认 `TextEdit`）

- [ ] **Step 1: 确认/补充 TextEdit import** — 查看 app.slint 顶部 `import ... from "std-widgets.slint"`，确保含 `TextEdit`；没有则加入。

- [ ] **Step 2: 新增编辑器属性与回调** — 在 `alert-message`（[ui/app.slint:94](../../../ui/app.slint)）之后插入：

```slint
    // --- In-app text editor (SFTP 远端文件) ------------------------------
    in-out property <bool> editor-open: false;
    in-out property <string> editor-tab-id;   // 保存时定位 SFTP handle
    in-out property <string> editor-filename;
    in-out property <string> editor-path;     // 远端绝对路径
    in-out property <string> editor-content;
    in-out property <bool> editor-dirty: false;
    in-out property <string> editor-status;
    callback editor-save(string /* tab-id */, string /* remote-path */, string /* content */);
    callback editor-close();
```

- [ ] **Step 3: 新增满窗覆盖层** — 在 About 对话框块结束（[ui/app.slint:759](../../../ui/app.slint) 的 `}` 之后、root 组件闭合 `}` 之前）插入：

```slint
    // --- In-app text editor overlay (满窗) -------------------------------
    Rectangle {
        width: parent.width;
        height: parent.height;
        visible: root.editor-open;
        background: #00000080;
        // 吞掉背景点击，避免误关（编辑器只能按"关闭"退出）
        TouchArea {}

        property <bool> confirm-discard: false;

        Rectangle {
            x: 24px; y: 24px;
            width: parent.width - 48px;
            height: parent.height - 48px;
            background: Theme.bg-panel;
            border-radius: Theme.radius-md;
            border-width: 1px;
            border-color: Theme.border-strong;
            drop-shadow-blur: 24px;
            drop-shadow-color: #000000a0;

            VerticalLayout {
                padding: 12px;
                spacing: 8px;

                // 标题栏：文件名 + 路径 + 脏标记 + 保存/关闭
                HorizontalLayout {
                    spacing: 8px;
                    Text {
                        text: (root.editor-dirty ? "● " : "") + root.editor-filename;
                        color: Theme.text-primary;
                        font-size: Theme.fs-md;
                        font-weight: 700;
                        vertical-alignment: center;
                    }
                    Text {
                        text: root.editor-path;
                        color: Theme.text-muted;
                        font-size: Theme.fs-xs;
                        horizontal-stretch: 1;
                        overflow: elide;
                        vertical-alignment: center;
                    }
                    // 保存
                    Rectangle {
                        width: 64px; height: 26px;
                        border-radius: Theme.radius-sm;
                        background: save-ta.has-hover ? Theme.accent-hover : Theme.accent;
                        save-ta := TouchArea {
                            mouse-cursor: pointer;
                            clicked => {
                                root.editor-save(root.editor-tab-id, root.editor-path, root.editor-content);
                            }
                        }
                        Text { text: @tr("Save"); color: #ffffff; font-size: Theme.fs-sm;
                               horizontal-alignment: center; vertical-alignment: center; }
                    }
                    // 关闭（脏时二次点击才放弃）
                    Rectangle {
                        width: parent.confirm-discard ? 150px : 64px; height: 26px;
                        border-radius: Theme.radius-sm;
                        background: close-ta.has-hover ? Theme.bg-hover : Theme.bg-panel-alt;
                        border-width: 1px;
                        border-color: parent.confirm-discard ? Theme.danger : Theme.border-subtle;
                        close-ta := TouchArea {
                            mouse-cursor: pointer;
                            clicked => {
                                if (root.editor-dirty && !parent.parent.confirm-discard) {
                                    parent.parent.confirm-discard = true;
                                } else {
                                    parent.parent.confirm-discard = false;
                                    root.editor-close();
                                }
                            }
                        }
                        Text {
                            text: parent.parent.confirm-discard ? @tr("Unsaved · click to discard") : @tr("Close");
                            color: parent.parent.confirm-discard ? Theme.danger : Theme.text-primary;
                            font-size: Theme.fs-sm;
                            horizontal-alignment: center; vertical-alignment: center;
                        }
                    }
                }

                // 编辑区
                TextEdit {
                    vertical-stretch: 1;
                    text <=> root.editor-content;
                    font-family: Theme.font-mono;
                    font-size: Theme.fs-sm;
                    wrap: no-wrap;
                    edited => {
                        root.editor-dirty = true;
                        parent.confirm-discard = false;
                    }
                }

                // 状态行
                Text {
                    text: root.editor-status;
                    color: Theme.text-muted;
                    font-size: Theme.fs-xs;
                    height: 14px;
                    overflow: elide;
                }
            }
        }
    }
```

> 说明：`confirm-discard` 定义在外层覆盖 Rectangle 上，按钮内用 `parent.parent.confirm-discard` 访问（层级：TouchArea→Rectangle(按钮)→HorizontalLayout? ）。**执行时按实际嵌套层级核对 `parent` 链深度**，必要时把 `confirm-discard` 提升为 root 的 `in-out property` 以简化访问。Cmd/Ctrl+S 快捷键本期不做（以"保存"按钮为准），留作后续。

- [ ] **Step 4: 编译**

Run: `cargo build`
Expected: 通过。若报 `parent.parent` 层级/属性访问错误，按上面说明把 `confirm-discard` 改为 root 级 `in-out property <bool> editor-confirm-discard` 并相应改引用。

- [ ] **Step 5: 手动冒烟**（此时回调未接 Rust，覆盖层默认不显示）— `cargo run`，确认 app 正常启动、无覆盖层、原功能不受影响。

- [ ] **Step 6: Commit**

```bash
git add ui/app.slint
git commit -m "feat(ui): 加 SFTP 远端文件满窗编辑覆盖层（暂未接线）"
```

---

## Task 5: SFTP 读/写命令 + 事件 + 打开/保存接线（需求 3/4 核心）

**Files:**
- Modify: `src/ssh.rs`（`SessionEvent` 枚举 [src/ssh.rs:143](../../../src/ssh.rs)）
- Modify: `src/sftp.rs`（`SftpCommand` [src/sftp.rs:27](../../../src/sftp.rs)、`SftpHandle` 方法 [src/sftp.rs:43](../../../src/sftp.rs)、命令处理 [src/sftp.rs:215+](../../../src/sftp.rs)）
- Modify: `src/app.rs`（`SftpFileContent` 分支、`on_sftp_edit` [src/app.rs:1528](../../../src/app.rs)、新增 `on_editor_save`/`on_editor_close`）

- [ ] **Step 1: 加事件变体** — `src/ssh.rs` `SessionEvent`（[src/ssh.rs:162](../../../src/ssh.rs) `SftpStatus` 旁）新增：

```rust
    SftpFileContent {
        remote: String,
        filename: String,
        content: String,
    },
```

- [ ] **Step 2: 加 SFTP 命令与 handle 方法** — `src/sftp.rs` `SftpCommand`（[src/sftp.rs:27](../../../src/sftp.rs)）增加：

```rust
    ReadFile { remote: String },
    WriteFile { remote: String, content: String },
```

`impl SftpHandle`（[src/sftp.rs:43](../../../src/sftp.rs)）增加：

```rust
    pub fn read_file(&self, remote: String) {
        let _ = self.commands.send(SftpCommand::ReadFile { remote });
    }

    pub fn write_file(&self, remote: String, content: String) {
        let _ = self.commands.send(SftpCommand::WriteFile { remote, content });
    }
```

- [ ] **Step 3: 处理 ReadFile/WriteFile** — 在命令 `match` 中（与 `OpenTemp` 同级，[src/sftp.rs:343](../../../src/sftp.rs) 附近）新增两个分支。常量定义在文件顶部：`const MAX_EDIT_BYTES: usize = 5 * 1024 * 1024;`

```rust
            SftpCommand::ReadFile { remote } => {
                let filename = base_name(&remote);
                // 先看大小，避免把超大文件读进内存
                let too_big = sftp
                    .metadata(&remote)
                    .await
                    .ok()
                    .and_then(|m| m.size)
                    .map(|sz| sz as usize > MAX_EDIT_BYTES)
                    .unwrap_or(false);
                if too_big {
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{}: {}", t("文件过大，无法编辑", "File too large to edit"), filename
                    )));
                } else {
                    match sftp.read(remote.clone()).await {
                        Ok(bytes) => match check_editable(&bytes, MAX_EDIT_BYTES) {
                            Ok(content) => {
                                let _ = events.send(SessionEvent::SftpFileContent {
                                    remote: remote.clone(),
                                    filename,
                                    content,
                                });
                            }
                            Err(EditableError::TooLarge) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}", t("文件过大，无法编辑", "File too large to edit"), filename
                                )));
                            }
                            Err(EditableError::NotUtf8) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}", t("二进制或非 UTF-8 文件，暂不支持编辑", "Binary / non-UTF-8 file, not editable"), filename
                                )));
                            }
                        },
                        Err(e) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}", t("打开失败", "Open failed")
                            )));
                        }
                    }
                }
            }
            SftpCommand::WriteFile { remote, content } => {
                let filename = base_name(&remote);
                match sftp.write(remote.clone(), content.as_bytes()).await {
                    Ok(()) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}", t("已保存", "Saved"), filename
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}", t("保存失败", "Save failed")
                        )));
                    }
                }
            }
```

移除 `check_editable` 上的 `#[allow(dead_code)]`（现已被使用）。

- [ ] **Step 4: 处理 SftpFileContent 事件（打开编辑器）** — `apply_session_event_to_window` 的 `match event`（[src/app.rs:1716](../../../src/app.rs)）新增分支：

```rust
        SessionEvent::SftpFileContent { remote, filename, content } => {
            win.set_editor_tab_id(tab_id.into());
            win.set_editor_path(remote.into());
            win.set_editor_filename(filename.into());
            win.set_editor_content(content.into());
            win.set_editor_dirty(false);
            win.set_editor_status("".into());
            win.set_editor_open(true);
        }
```

- [ ] **Step 5: `on_sftp_edit` 改走 ReadFile** — 把 [src/app.rs:1528-1535](../../../src/app.rs) 的 `handle.open_temp(path.to_string(), true)` 改为：

```rust
                handle.read_file(path.to_string());
```

- [ ] **Step 6: 接 `editor-save` / `editor-close`** — 在窗口回调注册处（`on_sftp_edit` 之后）新增：

```rust
    let save_sftp = sftp_handles.clone();
    window.on_editor_save(move |tab_id: SharedString, remote: SharedString, content: SharedString| {
        if let Ok(handles) = save_sftp.lock() {
            if let Some(handle) = handles.get(tab_id.as_str()) {
                handle.write_file(remote.to_string(), content.to_string());
            }
        }
    });

    let weak_close = window.as_weak();
    window.on_editor_close(move || {
        if let Some(w) = weak_close.upgrade() {
            w.set_editor_open(false);
            w.set_editor_content("".into());
            w.set_editor_dirty(false);
        }
    });
```

- [ ] **Step 7: 保存成功后清脏标记** — 在 `SftpStatus` 分支或 `SftpFileContent` 之外，保存成功只回 `SftpStatus`。为让"已保存"清掉脏标记，在 `apply_session_event_to_window` 的 `SftpStatus` 分支里，如内容以"已保存/Saved"开头则 `win.set_editor_dirty(false)`。改 `SftpStatus` 分支（[src/sftp.rs](../../../src/sftp.rs) 发送、app.rs 接收处）：

```rust
        SessionEvent::SftpStatus(status) => {
            if status.starts_with(&crate::i18n::t("已保存", "Saved").to_string()) && win.get_editor_open() {
                win.set_editor_dirty(false);
                win.set_editor_status(status.clone().into());
            }
            // ……保留原有把 status 写到 SFTP 状态栏的逻辑（按现状）……
        }
```

> 执行时先看 `SftpStatus` 分支现有实现，**在其基础上增量添加**清脏标记逻辑，勿删原有行为。

- [ ] **Step 8: 编译并测试**

Run: `cargo build && cargo test`
Expected: 通过；`check_editable` 测试仍绿；无 `dead_code` 告警（check_editable 已用）。

- [ ] **Step 9: 手动验证** — 连服务器：右键文本文件→"编辑"→满窗编辑器载入内容；改动出现脏标记 `●`；点"保存"状态显示"已保存"、脏标记消失；远端文件确被更新（重开确认）。对二进制文件（如某 .png）点编辑→状态栏提示"二进制/非 UTF-8"，不打开。

- [ ] **Step 10: Commit**

```bash
git add src/ssh.rs src/sftp.rs src/app.rs
git commit -m "feat(sftp): 远端文件经内嵌编辑器读取/保存（ReadFile/WriteFile + SftpFileContent）"
```

---

## Task 6: 菜单改 下载/编辑/删除 + 清理失效 View/OpenTemp（需求 3/4 收尾）

**Files:**
- Modify: `ui/sftp_panel.slint`（菜单项 [ui/sftp_panel.slint:327-346](../../../ui/sftp_panel.slint)、弹窗高度 [ui/sftp_panel.slint:316](../../../ui/sftp_panel.slint)、`view` 回调声明 [ui/sftp_panel.slint:65](../../../ui/sftp_panel.slint)）
- Modify: `ui/terminal_view.slint`（`sftp-view` 回调 [ui/terminal_view.slint:100](../../../ui/terminal_view.slint)、`view` 转发 [ui/terminal_view.slint:693](../../../ui/terminal_view.slint)）
- Modify: `ui/app.slint`（`sftp-view` 回调 [ui/app.slint:161](../../../ui/app.slint)、转发 [ui/app.slint:270](../../../ui/app.slint)）
- Modify: `src/app.rs`（删除 `on_sftp_view` [src/app.rs:1519-1526](../../../src/app.rs)）
- Modify: `src/sftp.rs`（删除 `OpenTemp` 命令与处理、`open_temp` 方法、`open_with_os`×2、`spawn_edit_watcher`）

- [ ] **Step 1: sftp_panel 去掉"查看"菜单项并改高度** — 删除 [ui/sftp_panel.slint:331-334](../../../ui/sftp_panel.slint) 的 `View` `SftpMenuItem` 块；删除 [ui/sftp_panel.slint:65](../../../ui/sftp_panel.slint) 的 `callback view(string);`；弹窗高度（[ui/sftp_panel.slint:316](../../../ui/sftp_panel.slint)）由 `(entry.is-dir ? 1 : 4)` 改为 `(entry.is-dir ? 1 : 3)`。

- [ ] **Step 2: terminal_view 去掉 sftp-view 转发** — 删除 [ui/terminal_view.slint:100](../../../ui/terminal_view.slint) 的 `callback sftp-view(string);` 与 [ui/terminal_view.slint:693](../../../ui/terminal_view.slint) 的 `view(path) => { root.sftp-view(path); }`。

- [ ] **Step 3: app.slint 去掉 sftp-view 链路** — 删除 [ui/app.slint:161](../../../ui/app.slint) 的 `callback sftp-view(...)` 与 [ui/app.slint:270](../../../ui/app.slint) 的 `sftp-view(path) => { root.sftp-view(term.id, path); }`。

- [ ] **Step 4: app.rs 删除 on_sftp_view** — 删除 [src/app.rs:1519-1526](../../../src/app.rs) 整段 `let view_sftp = ...; window.on_sftp_view(...)`。

- [ ] **Step 5: sftp.rs 删除死代码** — 删除：`SftpCommand::OpenTemp` 变体（[src/sftp.rs:33](../../../src/sftp.rs)）、`open_temp` 方法（[src/sftp.rs:68-70](../../../src/sftp.rs)）、`OpenTemp` 处理分支（[src/sftp.rs:343-382](../../../src/sftp.rs)）、两个 `open_with_os`（[src/sftp.rs:660-698](../../../src/sftp.rs)）、`spawn_edit_watcher`（[src/sftp.rs:718-749](../../../src/sftp.rs)）。删除后清理因之不再使用的 import（如 `OpenTemp` 用到而别处没用的项；按编译告警处理）。

- [ ] **Step 6: 编译并测试**

Run: `cargo build && cargo test`
Expected: 通过、无未使用告警、测试全绿。若有 `unused import`/`dead_code` 告警，按提示清理。

- [ ] **Step 7: 手动验证** — 右键文件菜单只剩 `下载 / 编辑 / 删除`；文件夹右键仍只有 `删除`；"编辑"走内嵌编辑器；macOS 下不再有"下载了打不开"的问题。

- [ ] **Step 8: Commit**

```bash
git add ui/sftp_panel.slint ui/terminal_view.slint ui/app.slint src/app.rs src/sftp.rs
git commit -m "refactor(sftp): 菜单改为下载/编辑/删除并移除失效的 View/OpenTemp 路径"
```

---

## 收尾

- [ ] **lang 文案核对** — 确认 `lang/*` 含 `Save`/`Close`/`Edit`/`Download`/`Delete`/`Unsaved · click to discard`/`File too large to edit`/`Binary / non-UTF-8 file, not editable` 的中英翻译（`@tr` + `t()`）；缺则补齐。
- [ ] **最终全量验证**：`cargo build && cargo test` 全绿；手动跑通 4 项需求。
- [ ] 更新 spec 状态为"已实现"（可选）。

## Self-Review 检查记录

- **Spec 覆盖**：需求 1→Task 1；需求 2→Task 3；需求 3+4→Task 2/4/5/6。无遗漏。
- **占位符**：无 TBD/TODO；UI 层级 `parent.parent` 处给了"提升为 root 属性"的明确兜底。
- **类型一致**：`check_editable`/`EditableError`（Task 2 定义→Task 5 使用）、`read_file`/`write_file`（Task 5 定义即用）、`SftpFileContent` 字段（ssh.rs 定义→sftp.rs 发送→app.rs 接收）三处字段名一致（remote/filename/content）。`should_alert_on_close` 签名 Task 1 内自洽。
