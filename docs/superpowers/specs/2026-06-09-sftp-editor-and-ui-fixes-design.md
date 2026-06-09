# 设计方案：SFTP 原生编辑器 + 标签关闭/文件夹图标修复

- 日期：2026-06-09
- 状态：已确认，待写实现计划
- 涉及模块：SFTP 文件管理器、标签交互、主题

## 背景与目标

四项改动集中在 SFTP 文件管理器与标签交互上，作为一批提交。核心是用**原生 Slint 纯文本覆盖式编辑器**替换当前坏掉的"下载 + 系统程序打开"逻辑，并顺带修两个 bug、改一处图标。

经与用户确认的关键决策：

1. Monaco 集成方式 → **不用 Monaco，走原生 Slint 编辑器**。
2. 编辑器呈现 → **全屏覆盖编辑面板**（非独立标签页）。
3. 语法高亮 → **不做高亮，纯文本编辑**（已用最新 Slint 文档确认：可编辑控件 `TextEdit` 的 `text` 是单色纯字符串，无按 token 着色能力；`StyledText` 能渲染富文本但只读，不可编辑）。

## 需求清单

1. 点击标签关闭 SSH 标签时无需弹出警告框。
2. 文件管理器中，文件夹图标颜色使用 Windows 风格。
3. 文件管理器中，右键"查看"文本文件时文件失去焦点、无法查看内容（bug）。
4. 增加编辑器，将右键的"查看"改为"编辑"，在项目内打开编辑文件内容。

---

## 任务 1 — 关闭标签不再弹"连接失败"框

### 根因

关闭一个尚未连上（或仍在连接中）的标签时：`on_tab_closed`（[src/app.rs:964](../../../src/app.rs)）调用 `handle.close()` → SSH 会话异步发出 `SessionEvent::Closed(reason)` → 事件处理（[src/app.rs:1767](../../../src/app.rs)）调用 `should_show_connection_failed_alert(previous_state)`。该函数对"从未成功连接过"的状态（`None` / `Some(0)`）返回 `true`，于是弹出"连接失败"框（`show_connection_failed_alert`，[src/app.rs:316](../../../src/app.rs)）。

注意：`on_tab_closed` 会同步移除该标签的 status（[src/app.rs:977](../../../src/app.rs)），等 `Closed` 事件到达时 `statuses.get(tab_id)` 已是 `None`，因此无法靠"状态是否存在"来区分"用户主动关闭"与"真实连接失败"。

### 方案

新增一个共享集合记录"用户主动关闭中的标签"：

- 类型：`Arc<Mutex<HashSet<String>>>`（命名如 `user_closing_tabs`），在构建期创建并克隆给相关闭包。
- `on_tab_closed`：在调用 `handle.close()` **之前**，把 `id` 插入该集合。
- `Closed` 事件分支：若 `tab_id` 在该集合中，则**跳过** `show_connection_failed_alert`，并把该 id 从集合移除（一次性）。
- 真正的连接失败（非用户关闭）不在集合中，弹窗行为不变。

### 涉及改动

- `src/app.rs`：新增集合；`on_tab_closed` 闭包插入 id；`SessionEvent::Closed` 处理处（约 [src/app.rs:1772](../../../src/app.rs)）增加抑制判断。需把集合传入处理 `SessionEvent` 的函数（`match event` 在 [src/app.rs:1716](../../../src/app.rs)）。
- 既有测试 `should_show_connection_failed_alert` 保持不变、保持绿。

---

## 任务 2 — 文件夹图标 Windows 风格黄色

### 现状

文件列表每行用 emoji 渲染图标（[ui/sftp_panel.slint:351-353](../../../ui/sftp_panel.slint)）：

```slint
text: entry.is-dir ? "📁 " + entry.name : "📄 " + entry.name;
color: entry.is-dir ? Theme.accent : Theme.text-primary;
```

emoji 是彩色字形，`color` 无法控制其颜色，所以"文件夹颜色"实际不可调，且各平台观感不一致。

### 方案

把"图标"与"文件名"拆成两个 `Text`，图标改用**可着色的 Material Icons 字形**（项目已内置完整 `MaterialIcons-Regular.ttf`，356KB 非子集，含 folder/file 字形）：

- 文件夹：字形 `\u{e2c7}`（folder），颜色 = Windows 经典暖黄。
- 文件：字形 `\u{e873}`（description），颜色 = 中性灰（`Theme.text-secondary` / `text-muted`）。
- 文件名 `Text`：仍用 `Theme.text-primary`。

新增主题色：`ui/theme.slint` 增加 `out property <brush> folder-icon: #f7c948;`（亮/暗主题可用同一值，必要时暗色略提亮）。

### 涉及改动

- `ui/theme.slint`：新增 `folder-icon`。
- `ui/sftp_panel.slint`：文件列表行（约 [ui/sftp_panel.slint:348-358](../../../ui/sftp_panel.slint)）的 `HorizontalLayout` 内，把单个 `Text` 拆为「图标 `Text`（Material Icons 字体）+ 文件名 `Text`」。
- 范围：仅右侧文件列表。左侧目录树本就没有文件夹图标（只有 ▶/▼ 箭头 + 名称），不在本次范围。

---

## 任务 3 + 4 — 原生纯文本全屏覆盖编辑器（替换 View/Edit）

### 根因（任务 3）

当前"查看/编辑"走 `open_temp`（[src/sftp.rs:343](../../../src/sftp.rs)）：下载到临时目录后用 `open_with_os` 打开。而 macOS 命中 `#[cfg(not(windows))]` 分支用了 **Linux 专用的 `xdg-open`**（[src/sftp.rs:696](../../../src/sftp.rs)），macOS 上根本不存在该命令 → 文件下载了却打不开 → "无法查看内容"。用内嵌编辑器替换这条路径后，该 bug 自然消失（不再依赖系统打开）。

### 右键菜单

菜单项改为 `下载 / 编辑 / 删除`（去掉只读"查看"）：

- 删除 `View` 菜单项与 `view` 回调（[ui/sftp_panel.slint:331-334](../../../ui/sftp_panel.slint)）。
- 保留 `Edit`（[ui/sftp_panel.slint:335-338](../../../ui/sftp_panel.slint)），它触发 `root.edit(...)` → `sftp-edit`。
- 弹窗高度从 4 项改 3 项：`height: (entry.is-dir ? 1 : 3) * 27px + 8px`（[ui/sftp_panel.slint:316](../../../ui/sftp_panel.slint)）。

### 打开流程（点"编辑"）

1. `sftp-edit(tab-id, path)` → Rust `on_sftp_edit`（[src/app.rs:1529](../../../src/app.rs)）改为发新命令而非 `open_temp`。
2. 新增 `SftpCommand::ReadFile { remote }`：照 `download_impl`（[src/sftp.rs:497](../../../src/sftp.rs)）的 `sftp.open(remote).read()` 模式把内容读入内存 `Vec<u8>`。
3. **守卫**：
   - 大小上限（默认 5 MB）——超限不打开，状态栏提示。
   - UTF-8 校验——非 UTF-8/二进制不打开（避免保存时损坏文件），状态栏提示"二进制或非 UTF-8 文件，暂不支持编辑"。
4. 通过后发 `SessionEvent::SftpFileContent { remote, filename, content }` → 事件处理设置编辑器属性并打开覆盖层。

### 保存流程（点"保存" / Cmd·Ctrl+S）

1. `editor-save(remote-path, content)` → Rust（用记忆的当前编辑器所属 tab-id 找到对应 SFTP handle）。
2. 新增 `SftpCommand::WriteFile { remote, content }`：照 `upload_impl`（[src/sftp.rs:560+](../../../src/sftp.rs)）用写入+创建+截断标志打开远端文件，写入 `content` 的 UTF-8 字节，覆盖远端文件（不落临时文件）。
3. 状态回显：成功"已保存: <文件名>" / 失败"保存失败: <err>"；清除脏标记。

### 编辑器 UI（app.slint 新增覆盖层）

仿现有 alert 覆盖层（[ui/app.slint:605-657](../../../ui/app.slint)）新增一个 `editor-open` 覆盖层，占满标签栏下方主内容区：

- 顶部标题栏：文件名 + 远端路径；右侧 `保存` / `关闭` 按钮；脏标记 `●`。
- 主体：`TextEdit`（std-widgets），`font-family: "Cascadia Mono"`（已内置）、等宽、多行、`wrap: no-wrap`、可滚动，`text <=> root.editor-content`。
- 快捷键：Cmd/Ctrl+S 保存。
- 新增 AppWindow 属性（in-out）：`editor-open: bool`、`editor-filename: string`、`editor-path: string`、`editor-content: string`、`editor-dirty: bool`、`editor-status: string`。
- 新增回调：`editor-save(string /* remote-path */, string /* content */)`、`editor-close()`。
- 脏标记：`TextEdit` 文本变化时置 `editor-dirty = true`；保存成功后由 Rust 置回 false。

Rust 侧用 `Rc<RefCell<Option<String>>>` 记住"当前打开编辑器所属 tab-id"，开时设置、存时使用、关时清空（同一时刻只有一个覆盖式编辑器）。

### 清理（移除失效代码）

确认 `open_temp` 仅被 `on_sftp_view` 与 `on_sftp_edit` 调用，替换后以下变为死代码，一并移除：

- `sftp-view` 回调链路（slint 回调、`on_sftp_view`）。
- `SftpCommand::OpenTemp` 及其处理分支（[src/sftp.rs:343](../../../src/sftp.rs)）。
- `open_temp`（[src/sftp.rs:68](../../../src/sftp.rs)）、`open_with_os`（windows/非 windows 两个，[src/sftp.rs:660,695](../../../src/sftp.rs)）、`spawn_edit_watcher`（[src/sftp.rs:718](../../../src/sftp.rs)）。
- 保留 `Download`（菜单"下载"）原样不动。

### 边界情况

- 空文件：打开为空编辑区，可编辑保存。
- 超大/二进制/非 UTF-8：守卫拦截，状态栏提示，不打开。
- 保存失败（权限/磁盘）：状态栏报错，保留脏标记，不关闭编辑器。
- 关闭时有未保存改动：`关闭`按钮**二次点击才丢弃**（按钮临时变为"未保存·再次点击关闭"，约 2s 复原）——行内确认、不弹模态框，契合需求 1"少弹框"的取向。
- 外部并发改动 / 编码转换 / 冲突检测：不在范围内。

---

## 测试策略

- Rust 单元测试（`cargo test`）：抽出可测纯函数——
  - "是否可编辑"判定（大小上限 + UTF-8 有效性）。
  - 关闭抑制决策（给定 tab 是否在 `user_closing_tabs` 中）。
  - `should_show_connection_failed_alert` 既有测试保持绿。
- `cargo build` 必须通过（含 slint 编译）。
- SFTP 实际读写、覆盖层 UI、快捷键、文件夹图标观感：连真实 SSH 服务器**手动验证**（由用户执行）。

## 不在范围内（YAGNI）

- 语法高亮 / LSP / 代码智能（原生纯文本方案不做）。
- 多文件同时编辑、文档式多标签。
- 外部改动冲突检测、非 UTF-8 编码识别与转换。
- 左侧目录树加文件夹图标。

## 默认决策（可调）

1. 文件夹黄：`#f7c948`。
2. 编辑大小上限：5 MB。
3. 关闭未保存：二次点击确认（非模态）。
4. 编辑器字体：内置 Cascadia Mono。

## 涉及文件清单

- `src/app.rs`：tab 关闭抑制集合；`on_sftp_edit` 改走 ReadFile；新增 `SftpFileContent` 事件处理与编辑器属性/回调绑定；移除 `on_sftp_view`。
- `src/sftp.rs`：新增 `ReadFile` / `WriteFile` 命令与处理；移除 `OpenTemp` / `open_temp` / `open_with_os` / `spawn_edit_watcher`。
- `src/ssh.rs`（[src/ssh.rs:143](../../../src/ssh.rs)，`SessionEvent` 枚举）：新增 `SessionEvent::SftpFileContent`。
- `ui/app.slint`：编辑器覆盖层 + 属性/回调；移除 `sftp-view` 回调（:161）与转发（:270）。
- `ui/sftp_panel.slint`：菜单去掉"查看" `view` 回调与菜单项、改弹窗高度；文件夹/文件图标拆分着色。
- `ui/terminal_view.slint`：移除 `sftp-view` 回调（:100）与 `view(path)` 转发（:693）。
- `ui/theme.slint`：新增 `folder-icon`。
- `lang/*`：确保"编辑/下载/删除/保存/关闭"等文案的中英翻译齐全。
