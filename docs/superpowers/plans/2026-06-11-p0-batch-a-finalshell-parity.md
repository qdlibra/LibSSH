# P0 批次 A：FinalShell 对齐（重连 / 命令历史补全 / 快捷命令 / 密码加密）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 OB 清单《FinalShell功能对比补充清单》P0 中笔记建议优先的四项：断线自动重连、命令历史 + 智能补全（底部本地命令栏）、快捷命令面板、密码 keyring 加密存储。

**Architecture:** 重连复用现有 spawn_session 事件总线，把「spawn + 事件转发」抽成可重入的 `start_session_io()`，手动/自动重连统一走 `invoke_reconnect_tab`；命令历史与补全采用 FinalShell 的「底部本地输入框」模式（避免与远端 shell 键位冲突），终端内敲键用 dirty-flag 行跟踪尽力记录；密码存储采用「无标志位」方案——写优先 keyring、失败回退明文，读时密码为空才查 keyring，迁移幂等自愈。

**Tech Stack:** Rust + Slint 1.8（既有），新增 keyring v3（apple-native / windows-native / sync-secret-service）。

**剩余 P0（批次 B，另行写计划）：** 进程管理器、会话分组/搜索/排序。

---

## 既有代码事实（写代码前必读）

- `src/app.rs:66` `run()`；`src/app.rs:532` `wire_callbacks(...)` 注册全部回调；窗口类型 `AppWindow` 由 `slint::include_modules!()` 生成，slint struct（`TerminalState` 等）字段在 Rust 侧是 snake_case。
- 连接回调 `on_connect_session`（`src/app.rs` ~845-1019）：创建 TabInfo/TerminalState/TermBuffer → `spawn_session`（src/ssh.rs:335）→ handle 存入 `handles: Rc<RefCell<HashMap<String, SessionHandle>>>` → `spawn_sftp` → 两个 `std::thread::spawn` 事件转发线程（`rx.blocking_recv()` → `slint::invoke_from_event_loop` → `apply_session_event_to_window`）。
- `apply_session_event_to_window`（`src/app.rs:2175`）：`SessionEvent::Closed` 分支在 ~2276；`Connected` 在 ~2266；`user_closing` 一次性标记区分用户主动关闭。
- `TabStatus`（`src/app.rs:51`）：per-tab 监控/状态，`state: u8`（0 连接中/1 已连/2 断开）。
- 按键链：`terminal_view.slint` ime-input `key-pressed` → `root.send-key` → `app.slint:199` `callback send-key(tab-id, key, ctrl, alt, shift)` → `app.rs:1143` `on_send_key` → `key_to_pty_bytes`（app.rs:2676）→ `handle.send_raw`。
- 粘贴：`on_paste_from_clipboard`（app.rs，grep `paste_from_clipboard`）直接 `send_raw`。
- `ConfigFile`（src/config.rs:252）serde 全默认、可叠加；`ConfigStore::save()` 原子写 `sessions.json`。
- `set_terminal_row(win, tab_id, mutator)`（app.rs:2660）单行更新 TerminalState。
- Slint 键码：箭头等在 `\u{F700}`-`\u{F8FF}` PUA 区（见 key_to_pty_bytes 的映射表）。
- i18n：slint 用 `@tr("English msgid")`，翻译进 `lang/zh/LC_MESSAGES/LibSSH.po`（en.po 同 msgid 恒等）；Rust 用 `crate::i18n::t("中","en")` 无需 po。
- 测试：`cargo test`（纯 Rust 单测，无 UI 测试）。验收一律 `cargo test` + `cargo build`。

---

### Task 1: 抽取 `start_session_io()`（纯重构，行为不变）

**Files:**
- Modify: `src/app.rs`（on_connect_session 闭包 ~903-1018 后半段抽函数）

- [ ] **Step 1.1: 定义 IO 上下文 struct**

在 `struct AppModels`（app.rs:27）后加：

```rust
/// start_session_io 所需的共享容器集合。connect 与 reconnect 都从这里拿。
#[derive(Clone)]
struct SessionIoCtx {
    runtime: Arc<Runtime>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    sftp_handles: SftpHandles,
    sftp_manual_nav: SftpManualNav,
    bufs: TermBuffers,
    tab_statuses: TabStatuses,
    user_closing: Arc<Mutex<HashSet<String>>>,
    local_snap: LocalSnap,
    local_net_hist: NetHist,
}
```

- [ ] **Step 1.2: 抽函数**

把 on_connect_session 闭包中「`let (initial_cols, initial_rows) = ...` 到第二个事件转发线程结束」整体搬到新的自由函数（保持原语句不动，仅替换变量名前缀 `connect_*` → ctx 字段）：

```rust
/// 为 tab 建立 SSH+SFTP 连接并接好事件转发。connect 与 reconnect 共用。
/// 调用方负责：TerminalState/TermBuffer/TabStatus 行已存在。
fn start_session_io(
    weak: slint::Weak<AppWindow>,
    ctx: &SessionIoCtx,
    tab_id: String,
    session: Session,
    initial_cols: u32,
    initial_rows: u32,
) {
    let sftp_session = session.clone();
    let (handle, mut rx) = spawn_session(
        ctx.runtime.handle(), tab_id.clone(), session, initial_cols, initial_rows,
    );
    ctx.handles.borrow_mut().insert(tab_id.clone(), handle);

    let (sftp_tx, mut sftp_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
    let sftp_handle = spawn_sftp(ctx.runtime.handle(), sftp_session, sftp_tx);
    ctx.sftp_handles.lock().unwrap().insert(tab_id.clone(), sftp_handle);

    // —— 以下两个事件转发线程：从原闭包原样搬运，
    //    connect_xxx.clone() 改为 ctx.xxx.clone() ——
    /* shell 事件线程（含 CwdChanged 防抖）原样 */
    /* sftp 事件线程原样 */
}
```

- [ ] **Step 1.3: wire_callbacks 里构建 ctx 并改写 connect 闭包**

`wire_callbacks` 开头（sessions_model 克隆附近）构建 `let io_ctx = SessionIoCtx { runtime: runtime.clone(), handles: handles.clone(), sftp_handles: sftp_handles.clone(), sftp_manual_nav: sftp_manual_nav.clone(), bufs: bufs.clone(), tab_statuses: tab_statuses.clone(), user_closing: user_closing_tabs.clone(), local_snap: local_snap.clone(), local_net_hist: local_net_hist.clone() };`。connect 闭包捕获 `io_ctx.clone()` + `last_term_size`，后半段替换为：

```rust
let (initial_cols, initial_rows) = *connect_last_size.lock().unwrap();
start_session_io(weak.clone(), &connect_ctx, tab_id.clone(), session, initial_cols, initial_rows);
```

- [ ] **Step 1.4: 验证 + 提交**

Run: `cargo test && cargo build`，Expected: 75 passed、无新警告。

```bash
git add -A && git commit -m "refactor(app): 抽取 start_session_io，connect 与后续 reconnect 共用连接装配"
```

---

### Task 2: 手动重连（conn-lost 状态 + 状态条重连按钮）

**Files:**
- Modify: `ui/app.slint`（TerminalState 加字段、callback、实例化接线）
- Modify: `ui/terminal_view.slint`（状态条按钮）
- Modify: `src/app.rs`（tab_sessions 映射、on_reconnect_tab、Closed/Connected 置位）
- Modify: `lang/zh/LC_MESSAGES/LibSSH.po`、`lang/en/LC_MESSAGES/LibSSH.po`

- [ ] **Step 2.1: TerminalState 加 `conn-lost`**

`ui/app.slint` struct TerminalState（37-54 行）`is-alt-screen: bool,` 后加：

```slint
    conn-lost: bool,       // 连接已断开（显示重连按钮；重连中复位）
```

AppWindow callback 区（`callback send-key(...)` 上方）加：

```slint
    callback reconnect-tab(string /* tab-id */);
```

TerminalView 实例化（~305 起）加两行绑定：

```slint
    conn-lost: term.conn-lost;
    reconnect => { root.reconnect-tab(term.id); }
```

- [ ] **Step 2.2: 状态条重连按钮**

`ui/terminal_view.slint`：root 属性区加 `in property <bool> conn-lost: false;`、callback 区加 `callback reconnect();`。状态条（`if root.is-alt-screen : Text` 之前）加：

```slint
                if root.conn-lost : Rectangle {
                    width: 64px; height: 18px;
                    y: (parent.height - self.height) / 2;
                    border-radius: Theme.radius-sm;
                    background: reconnect-ta.has-hover ? Theme.accent-hover : Theme.accent;
                    reconnect-ta := TouchArea {
                        mouse-cursor: pointer;
                        clicked => { root.reconnect(); }
                    }
                    Text {
                        text: @tr("Reconnect");
                        color: white; font-size: Theme.fs-xs;
                        horizontal-alignment: center; vertical-alignment: center;
                    }
                }
```

- [ ] **Step 2.3: Rust 端：tab_sessions + on_reconnect_tab**

app.rs `run()`：`let tab_sessions: Rc<RefCell<HashMap<String, Session>>> = Rc::new(RefCell::new(HashMap::new()));`，传入 wire_callbacks（加参数）。

- connect 闭包：拿到 session 后 `reconnect_sessions.borrow_mut().insert(tab_id.clone(), session.clone());`
- `on_tab_closed`：`close_tab_sessions.borrow_mut().remove(&id);`
- TerminalState 创建处（connect_terminals.push）加 `conn_lost: false,`
- `apply_session_event_to_window`：`Closed` 分支 `update_terminal` 闭包里加 `t.conn_lost = true;`；`Connected` 分支加 `t.conn_lost = false;`
- 新回调（wire_callbacks 内、connect 回调之后）：

```rust
let rc_ctx = io_ctx.clone();
let rc_sessions = tab_sessions.clone();
let rc_last_size = last_term_size.clone();
let weak = window.as_weak();
window.on_reconnect_tab(move |tab_id: SharedString| {
    let tab_id = tab_id.to_string();
    // 已在连接中/已连接（state != 2）则忽略：防手动+自动双触发。
    {
        let st = rc_ctx.tab_statuses.lock().unwrap();
        if st.get(&tab_id).map(|s| s.state) != Some(2) {
            return;
        }
    }
    let Some(session) = rc_sessions.borrow().get(&tab_id).cloned() else { return; };
    rc_ctx.user_closing.lock().unwrap().remove(&tab_id);
    if let Some(h) = rc_ctx.handles.borrow_mut().remove(&tab_id) { h.close(); }
    if let Some(h) = rc_ctx.sftp_handles.lock().unwrap().remove(&tab_id) { h.close(); }
    if let Some(st) = rc_ctx.tab_statuses.lock().unwrap().get_mut(&tab_id) { st.state = 0; }
    if let Some(w) = weak.upgrade() {
        set_terminal_row(&w, &tab_id, |row| {
            row.conn_lost = false;
            row.status = crate::i18n::t("重连中...", "Reconnecting...").into();
        });
    }
    let (cols, rows) = *rc_last_size.lock().unwrap();
    start_session_io(weak.clone(), &rc_ctx, tab_id, session, cols, rows);
});
```

- [ ] **Step 2.4: .po 文案**

zh.po 加：`msgid "Reconnect"` → `msgstr "重连"`；en.po 加恒等条目。

- [ ] **Step 2.5: 验证 + 提交**

`cargo test && cargo build` 过后，手动验收点（写进 commit body）：连一台服务器→ 服务器端 `kill` 掉 sshd 会话或断网 → 状态条出现「重连」→ 点击恢复，终端历史保留。

```bash
git add -A && git commit -m "feat(app): 断开后状态条显示重连按钮，复用 start_session_io 原地重连"
```

---

### Task 3: 自动重连（指数退避 ≤3 次）

**Files:**
- Modify: `src/app.rs`

- [ ] **Step 3.1: 失败测试**（app.rs 测试模块）

```rust
#[test]
fn auto_reconnect_backoff_caps_at_three_attempts() {
    use std::time::Duration;
    assert_eq!(auto_reconnect_delay(1), Some(Duration::from_secs(2)));
    assert_eq!(auto_reconnect_delay(2), Some(Duration::from_secs(4)));
    assert_eq!(auto_reconnect_delay(3), Some(Duration::from_secs(8)));
    assert_eq!(auto_reconnect_delay(4), None);
}

#[test]
fn auto_reconnect_only_after_established_non_user_close() {
    // (was_user_close, previous_state, attempts_so_far) -> 是否安排自动重连
    assert!(should_auto_reconnect(false, Some(1), 0));
    assert!(!should_auto_reconnect(true, Some(1), 0));   // 用户主动关
    assert!(!should_auto_reconnect(false, Some(0), 0));  // 从未连上（配置错）
    assert!(!should_auto_reconnect(false, Some(1), 3));  // 次数用尽
}
```

- [ ] **Step 3.2: 跑测试确认编译失败**（函数未定义）
- [ ] **Step 3.3: 实现**

```rust
/// 第 attempt 次自动重连前的等待；attempt 从 1 起，>3 不再重试。
fn auto_reconnect_delay(attempt: u8) -> Option<std::time::Duration> {
    (1..=3).contains(&attempt).then(|| std::time::Duration::from_secs(1 << attempt))
}

fn should_auto_reconnect(was_user_close: bool, previous_state: Option<u8>, attempts: u8) -> bool {
    !was_user_close && previous_state == Some(1) && attempts < 3
}
```

`TabStatus` 加 `reconnect_attempts: u8,`（derive Default 已覆盖）。`apply_session_event_to_window` 的 `Closed` 分支改为：

```rust
SessionEvent::Closed(reason) => {
    update_tab(&|t| t.connected = false);
    let schedule = {
        let was_user_close = user_closing.lock().unwrap().remove(tab_id);
        let mut st = statuses.lock().unwrap();
        let previous_state = st.get(tab_id).map(|s| s.state);
        let attempts = st.get(tab_id).map(|s| s.reconnect_attempts).unwrap_or(0);
        let auto = should_auto_reconnect(was_user_close, previous_state, attempts);
        if let Some(s) = st.get_mut(tab_id) {
            s.state = 2;
            if auto { s.reconnect_attempts += 1; }
        }
        if auto {
            auto_reconnect_delay(attempts + 1).map(|d| (d, attempts + 1))
        } else {
            if should_alert_on_close(was_user_close, previous_state) {
                show_connection_failed_alert(win, &reason);
            }
            None
        }
    };
    match schedule {
        Some((delay, n)) => {
            update_terminal(&|t| {
                t.conn_lost = true;
                t.status = format!(
                    "{} - {} ({}/3, {}s)", crate::i18n::t("已断开", "Disconnected"),
                    crate::i18n::t("自动重连", "auto-reconnect"), n, delay.as_secs(),
                ).into();
            });
            let weak = win.as_weak();
            let tid = tab_id.to_string();
            Timer::single_shot(delay, move || {
                if let Some(w) = weak.upgrade() { w.invoke_reconnect_tab(tid.clone().into()); }
            });
        }
        None => {
            update_terminal(&|t| {
                t.conn_lost = true;
                t.status = format!("{} - {reason}", crate::i18n::t("已断开", "Disconnected")).into();
            });
        }
    }
    if win.get_active_tab_id().as_str() == tab_id {
        refresh_sidebar(win, statuses, local, local_net_hist);
    }
}
```

`Connected` 分支：`st.state = 1;` 后加 `st.reconnect_attempts = 0;`。

- [ ] **Step 3.4: `cargo test`（全过）→ commit**

```bash
git add -A && git commit -m "feat(app): 意外断线自动重连，2/4/8s 退避最多 3 次，连上即清零"
```

---

### Task 4: `src/history.rs` — 命令历史 + 输入行跟踪（纯逻辑 + 全单测）

**Files:**
- Create: `src/history.rs`
- Modify: `src/main.rs`（`mod history;` 按字母序插在 `mod config;` 后）

- [ ] **Step 4.1: 失败测试先行**（history.rs 底部）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_dedups_to_most_recent_and_caps() {
        let mut h = CommandHistory::in_memory();
        h.add("ls"); h.add("cd /tmp"); h.add("ls");
        assert_eq!(h.entries(), &["cd /tmp".to_string(), "ls".to_string()]);
        for i in 0..1100 { h.add(&format!("cmd{i}")); }
        assert_eq!(h.entries().len(), MAX_ENTRIES);
    }

    #[test]
    fn history_ignores_blank_and_oversized() {
        let mut h = CommandHistory::in_memory();
        h.add("   ");
        h.add(&"x".repeat(600));
        assert!(h.entries().is_empty());
    }

    #[test]
    fn suggest_matches_prefix_newest_first() {
        let mut h = CommandHistory::in_memory();
        h.add("git status"); h.add("git push"); h.add("ls");
        assert_eq!(h.suggest("git", 8), vec!["git push".to_string(), "git status".to_string()]);
        assert!(h.suggest("", 8).is_empty());
        assert_eq!(h.suggest("git pu", 8), vec!["git push".to_string()]);
    }

    #[test]
    fn history_round_trips_to_disk_and_tolerates_corrupt_file() {
        let dir = std::env::temp_dir().join(format!("libssh-hist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("command_history.json");
        let mut h = CommandHistory::load_at(path.clone());
        h.add("uptime");
        let h2 = CommandHistory::load_at(path.clone());
        assert_eq!(h2.entries(), &["uptime".to_string()]);
        std::fs::write(&path, "{bad json").unwrap();
        assert!(CommandHistory::load_at(path).entries().is_empty());
    }

    #[test]
    fn tracker_records_simple_line() {
        let mut t = InputTracker::default();
        for c in ["l", "s", " ", "-", "l"] { assert_eq!(t.feed_key(c, false, false), None); }
        assert_eq!(t.feed_key("\r", false, false), Some("ls -l".to_string()));
        assert_eq!(t.feed_key("\r", false, false), None); // 空行不记
    }

    #[test]
    fn tracker_backspace_edits_line() {
        let mut t = InputTracker::default();
        t.feed_key("l", false, false); t.feed_key("a", false, false);
        t.feed_key("\u{0008}", false, false); t.feed_key("s", false, false);
        assert_eq!(t.feed_key("\n", false, false), Some("ls".to_string()));
    }

    #[test]
    fn tracker_poisons_on_tab_arrows_and_ctrl() {
        // 远端补全 / 历史导航 / 控制组合 → 本地缓冲不可信，该行放弃。
        for poison in [("\t", false), ("\u{F700}", false), ("a", true)] {
            let mut t = InputTracker::default();
            t.feed_key("l", false, false);
            t.feed_key(poison.0, poison.1, false);
            t.feed_key("s", false, false);
            assert_eq!(t.feed_key("\r", false, false), None, "poison {:?}", poison);
        }
    }

    #[test]
    fn tracker_ctrl_c_and_ctrl_u_reset_clean() {
        let mut t = InputTracker::default();
        t.feed_key("x", false, false);
        t.feed_key("\u{0003}", false, false); // Ctrl+C（控制码形态）
        t.feed_key("l", false, false); t.feed_key("s", false, false);
        assert_eq!(t.feed_key("\r", false, false), Some("ls".to_string()));
        t.feed_key("y", false, false);
        t.feed_key("u", true, false);          // Ctrl+U（modifier 形态）
        t.feed_key("p", false, false); t.feed_key("s", false, false);
        assert_eq!(t.feed_key("\r", false, false), Some("ps".to_string()));
    }

    #[test]
    fn tracker_paste_single_line_appends_multiline_poisons() {
        let mut t = InputTracker::default();
        t.feed_paste("echo hi");
        assert_eq!(t.feed_key("\r", false, false), Some("echo hi".to_string()));
        t.feed_paste("a\nb");
        assert_eq!(t.feed_key("\r", false, false), None);
    }
}
```

- [ ] **Step 4.2: 实现**

```rust
//! 本地命令历史（全局、跨会话持久化）+ 终端输入行尽力跟踪。
//!
//! 跟踪策略：只在我们能确定本地缓冲与远端行一致时记录。Tab 补全、
//! 箭头历史、Ctrl 组合等会让远端行偏离本地缓冲 → 标记 poisoned，
//! 该行在 Enter 时丢弃。Ctrl+C / Ctrl+U 在 shell 里整行作废/清除，
//! 等价于干净的新行。

use std::path::PathBuf;

pub const MAX_ENTRIES: usize = 1000;
const MAX_LINE_LEN: usize = 500;

pub struct CommandHistory {
    path: Option<PathBuf>,
    entries: Vec<String>, // 旧 → 新
}

impl CommandHistory {
    pub fn load_default() -> Self {
        let path = directories::ProjectDirs::from("dev", "LibSSH", "LibSSH")
            .map(|d| d.config_dir().join("command_history.json"));
        match path {
            Some(p) => Self::load_at(p),
            None => Self::in_memory(),
        }
    }

    pub fn load_at(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default();
        Self { path: Some(path), entries }
    }

    pub fn in_memory() -> Self {
        Self { path: None, entries: Vec::new() }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn add(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        if cmd.is_empty() || cmd.chars().count() > MAX_LINE_LEN {
            return;
        }
        self.entries.retain(|e| e != cmd);
        self.entries.push(cmd.to_string());
        if self.entries.len() > MAX_ENTRIES {
            let drop = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(0..drop);
        }
        self.save();
    }

    /// 前缀匹配，最新优先。空前缀不弹建议。
    pub fn suggest(&self, prefix: &str, limit: usize) -> Vec<String> {
        if prefix.is_empty() {
            return Vec::new();
        }
        self.entries.iter().rev()
            .filter(|e| e.starts_with(prefix) && e.as_str() != prefix)
            .take(limit).cloned().collect()
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(raw) = serde_json::to_string(&self.entries) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, raw).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
}

#[derive(Default)]
pub struct InputTracker {
    line: String,
    poisoned: bool,
}

impl InputTracker {
    /// 处理一次按键（与 on_send_key 同源的 key/modifiers）。
    /// 返回 Some(line)：一行被干净地提交。
    pub fn feed_key(&mut self, key: &str, ctrl: bool, alt: bool) -> Option<String> {
        let first = key.chars().next();
        // Enter：提交
        if matches!(key, "\r" | "\n") && !ctrl && !alt {
            let done = (!self.poisoned && !self.line.trim().is_empty())
                .then(|| self.line.trim().to_string());
            self.reset();
            return done;
        }
        // Ctrl+C / Ctrl+U：行作废 → 干净新行
        let is_ctrl_c = (ctrl && matches!(key, "c" | "C")) || key == "\u{0003}";
        let is_ctrl_u = (ctrl && matches!(key, "u" | "U")) || key == "\u{0015}";
        if is_ctrl_c || is_ctrl_u {
            self.reset();
            return None;
        }
        // Backspace：编辑
        if matches!(key, "\u{0008}" | "\u{007f}") && !ctrl && !alt {
            self.line.pop();
            return None;
        }
        // 污染源：Tab、Slint 专用键区（箭头/Home/End/Fn…）、其余控制码、Ctrl/Alt 组合
        let is_special = first.is_some_and(|c| ('\u{F700}'..='\u{F8FF}').contains(&c));
        let is_control = key.chars().count() == 1
            && first.is_some_and(|c| (c as u32) < 0x20);
        if ctrl || alt || key == "\t" || is_special || is_control {
            self.poisoned = true;
            return None;
        }
        // 可打印文本（含 IME 多字符提交）
        self.line.push_str(key);
        None
    }

    /// 粘贴：单行并入缓冲；含换行则远端可能直接执行 → 污染。
    pub fn feed_paste(&mut self, text: &str) {
        if text.contains('\n') || text.contains('\r') {
            self.poisoned = true;
        } else {
            self.line.push_str(text);
        }
    }

    /// 命令栏代发命令后远端行被消费，本地从干净空行重新开始。
    pub fn reset(&mut self) {
        self.line.clear();
        self.poisoned = false;
    }
}
```

- [ ] **Step 4.3: `cargo test history`（9 个新测试过）→ commit**

```bash
git add -A && git commit -m "feat(history): 命令历史持久化 + 终端输入行 dirty-flag 跟踪（纯逻辑层）"
```

---

### Task 5: 终端键入接入历史（send_key / paste）

**Files:**
- Modify: `src/app.rs`

- [ ] **Step 5.1: run() 建容器，传入 wire_callbacks**

```rust
let cmd_history: Rc<RefCell<crate::history::CommandHistory>> =
    Rc::new(RefCell::new(crate::history::CommandHistory::load_default()));
let input_trackers: Rc<RefCell<HashMap<String, crate::history::InputTracker>>> =
    Rc::new(RefCell::new(HashMap::new()));
```

- [ ] **Step 5.2: 接线**
- connect 闭包：`trackers.borrow_mut().insert(tab_id.clone(), Default::default());`
- `on_tab_closed`：`trackers.borrow_mut().remove(&id);`
- `on_send_key`（app.rs:1143，进入函数体先于各种早 return）：

```rust
if let Some(tracker) = send_trackers.borrow_mut().get_mut(tab_id.as_str()) {
    if let Some(line) = tracker.feed_key(&key, ctrl, alt) {
        send_history.borrow_mut().add(&line);
    }
}
```

（注意放在 `let key = key.to_string();` 之后；Shift-only/空 key 早退分支前没有副作用，先 feed 是安全的——空 key 不匹配任何规则，等价 no-op，但为最小化行为面，放在 `if key.is_empty() ...` 早退之后。）
- `on_paste_from_clipboard`：拿到剪贴板文本后 `tracker.feed_paste(&text);`

- [ ] **Step 5.3: `cargo test && cargo build` → commit**

```bash
git add -A && git commit -m "feat(app): 终端键入经 InputTracker 记入全局命令历史"
```

---

### Task 6: 底部命令栏（本地输入框 + 历史建议浮层）

**Files:**
- Create: `ui/command_bar.slint`
- Modify: `ui/app.slint`（import、property/callback、实例化在内容区下方）
- Modify: `src/app.rs`（suggestions 模型 + 两个回调）
- Modify: `lang/*.po`

- [ ] **Step 6.1: 新组件**（完整文件）

```slint
import { Theme } from "theme.slint";

export struct QuickCmdInfo {
    id: string,
    name: string,
    command: string,
}

// 底部本地命令栏：FinalShell 式独立输入框。键位只作用于本框，
// 不与远端 shell 抢 Tab/↑↓。Enter 发送整行（Rust 端先发 Ctrl+U 清远端行）。
export component CommandBar inherits Rectangle {
    in property <[string]> suggestions;
    in property <[QuickCmdInfo]> quick-commands;
    callback send(string);
    callback input-changed(string);
    callback open-quick-manage(string /* id, "" = new */);
    callback quick-delete(string);

    property <int> selected: -1;
    property <bool> dismissed: false;
    property <bool> quick-open: false;
    property <bool> suggestions-visible: !root.dismissed
        && cmd-input.text != "" && root.suggestions.length > 0;

    height: 34px;
    background: Theme.bg-panel-alt;
    border-width: 1px;
    border-color: Theme.border-subtle;

    HorizontalLayout {
        padding-left: 8px; padding-right: 8px; spacing: 6px;

        Text {
            text: ">_";
            color: Theme.text-muted;
            font-family: Theme.font-mono;
            font-size: Theme.fs-sm;
            vertical-alignment: center;
        }

        Rectangle {
            background: Theme.bg-root;
            border-radius: Theme.radius-sm;
            border-width: 1px;
            border-color: cmd-input.has-focus ? Theme.accent : Theme.border-subtle;
            horizontal-stretch: 1;

            cmd-input := TextInput {
                x: 8px;
                width: parent.width - 16px;
                height: parent.height;
                color: Theme.text-primary;
                font-family: Theme.font-mono;
                font-size: Theme.fs-sm;
                vertical-alignment: center;
                single-line: true;
                edited => {
                    root.dismissed = false;
                    root.selected = -1;
                    root.input-changed(self.text);
                }
                key-pressed(e) => {
                    if (e.text == "\n" || e.text == "\r") {
                        if (self.text != "") {
                            root.send(self.text);
                            self.text = "";
                            root.input-changed("");
                            root.selected = -1;
                        }
                        accept
                    } else if (e.text == Key.UpArrow && root.suggestions-visible) {
                        root.selected = root.selected <= 0
                            ? root.suggestions.length - 1 : root.selected - 1;
                        accept
                    } else if (e.text == Key.DownArrow && root.suggestions-visible) {
                        root.selected = root.selected >= root.suggestions.length - 1
                            ? 0 : root.selected + 1;
                        accept
                    } else if (e.text == Key.Tab && root.suggestions-visible) {
                        self.text = root.suggestions[max(0, root.selected)];
                        self.set-selection-offsets(self.text.character-count, self.text.character-count);
                        root.input-changed(self.text);
                        accept
                    } else if (e.text == Key.Escape) {
                        root.dismissed = true;
                        root.quick-open = false;
                        accept
                    } else {
                        reject
                    }
                }
            }
        }

        // 快捷命令开关（星形）
        Rectangle {
            width: 26px;
            border-radius: Theme.radius-sm;
            background: star-ta.has-hover || root.quick-open ? Theme.bg-hover : transparent;
            star-ta := TouchArea {
                mouse-cursor: pointer;
                clicked => { root.quick-open = !root.quick-open; }
            }
            Text {
                text: "\u{E838}";   // Material Icons "star"
                font-family: "Material Icons";
                color: root.quick-open ? Theme.accent : Theme.text-secondary;
                font-size: Theme.fs-md;
                horizontal-alignment: center; vertical-alignment: center;
            }
        }
    }

    // --- 历史建议浮层（命令栏上方） ------------------------------------
    if root.suggestions-visible : Rectangle {
        x: 22px;
        width: min(parent.width - 60px, 560px);
        height: root.suggestions.length * 24px + 8px;
        y: -self.height - 2px;
        background: Theme.bg-panel;
        border-radius: Theme.radius-sm;
        border-width: 1px;
        border-color: Theme.border-strong;
        drop-shadow-blur: 12px;
        drop-shadow-color: #00000060;

        VerticalLayout {
            padding: 4px;
            for s[i] in root.suggestions : Rectangle {
                height: 24px;
                border-radius: 3px;
                background: i == root.selected || sug-ta.has-hover ? Theme.bg-active : transparent;
                sug-ta := TouchArea {
                    mouse-cursor: pointer;
                    clicked => {
                        cmd-input.text = s;
                        cmd-input.set-selection-offsets(s.character-count, s.character-count);
                        root.input-changed(s);
                        cmd-input.focus();
                    }
                }
                Text {
                    x: 8px;
                    width: parent.width - 16px;
                    text: s;
                    color: Theme.text-primary;
                    font-family: Theme.font-mono;
                    font-size: Theme.fs-sm;
                    vertical-alignment: center;
                    overflow: elide;
                }
            }
        }
    }

    // --- 快捷命令浮层（命令栏上方，星形开） -----------------------------
    if root.quick-open : Rectangle {
        x: parent.width - self.width - 8px;
        width: min(parent.width - 60px, 420px);
        height: min(root.quick-commands.length, 10) * 28px + 40px;
        y: -self.height - 2px;
        background: Theme.bg-panel;
        border-radius: Theme.radius-sm;
        border-width: 1px;
        border-color: Theme.border-strong;
        drop-shadow-blur: 12px;
        drop-shadow-color: #00000060;

        VerticalLayout {
            padding: 4px;
            spacing: 1px;

            if root.quick-commands.length == 0 : Rectangle {
                height: 24px;
                Text {
                    text: @tr("No quick commands yet");
                    color: Theme.text-muted; font-size: Theme.fs-xs;
                    horizontal-alignment: center; vertical-alignment: center;
                }
            }

            for qc[i] in root.quick-commands : Rectangle {
                height: 28px;
                border-radius: 3px;
                background: qc-ta.has-hover ? Theme.bg-active : transparent;
                qc-ta := TouchArea {
                    mouse-cursor: pointer;
                    clicked => { root.send(qc.command); root.quick-open = false; }
                }
                HorizontalLayout {
                    padding-left: 8px; padding-right: 4px; spacing: 6px;
                    Text {
                        text: qc.name;
                        color: Theme.text-primary; font-size: Theme.fs-sm;
                        width: 35%; overflow: elide; vertical-alignment: center;
                    }
                    Text {
                        text: qc.command;
                        color: Theme.text-muted;
                        font-family: Theme.font-mono; font-size: Theme.fs-xs;
                        horizontal-stretch: 1; overflow: elide; vertical-alignment: center;
                    }
                    Rectangle {
                        width: 22px;
                        edit-ta := TouchArea {
                            mouse-cursor: pointer;
                            clicked => { root.open-quick-manage(qc.id); root.quick-open = false; }
                        }
                        Text {
                            text: "\u{E3C9}";   // edit
                            font-family: "Material Icons";
                            color: edit-ta.has-hover ? Theme.text-primary : Theme.text-muted;
                            font-size: Theme.fs-sm;
                            horizontal-alignment: center; vertical-alignment: center;
                        }
                    }
                    Rectangle {
                        width: 22px;
                        del-ta := TouchArea {
                            mouse-cursor: pointer;
                            clicked => { root.quick-delete(qc.id); }
                        }
                        Text {
                            text: "\u{E872}";   // delete
                            font-family: "Material Icons";
                            color: del-ta.has-hover ? Theme.danger : Theme.text-muted;
                            font-size: Theme.fs-sm;
                            horizontal-alignment: center; vertical-alignment: center;
                        }
                    }
                }
            }

            Rectangle {
                height: 26px;
                border-radius: 3px;
                background: add-ta.has-hover ? Theme.bg-hover : transparent;
                add-ta := TouchArea {
                    mouse-cursor: pointer;
                    clicked => { root.open-quick-manage(""); root.quick-open = false; }
                }
                Text {
                    text: @tr("+ Add quick command");
                    color: Theme.accent; font-size: Theme.fs-xs;
                    horizontal-alignment: center; vertical-alignment: center;
                }
            }
        }
    }
}
```

- [ ] **Step 6.2: app.slint 接入**
- import：`import { CommandBar, QuickCmdInfo } from "command_bar.slint";`，`export { ..., QuickCmdInfo }`
- AppWindow 属性/回调：

```slint
    in property <[string]> command-suggestions;
    in property <[QuickCmdInfo]> quick-commands;
    callback command-bar-send(string /* tab-id */, string /* text */);
    callback command-bar-input(string /* text */);
    callback quick-cmd-delete(string /* id */);
    // 管理对话框（新建 id 为空）
    in-out property <bool> qc-dialog-open: false;
    in-out property <string> qc-dialog-id;
    in-out property <string> qc-dialog-name;
    in-out property <string> qc-dialog-command;
    callback quick-cmd-submit(string /* id */, string /* name */, string /* command */);
```

- 右侧 VerticalLayout 中、内容区 Rectangle 之后（TabBar/内容区兄弟级）：

```slint
            if root.active-tab-id != "welcome" : CommandBar {
                suggestions: root.command-suggestions;
                quick-commands: root.quick-commands;
                send(text) => { root.command-bar-send(root.active-tab-id, text); }
                input-changed(t) => { root.command-bar-input(t); }
                quick-delete(id) => { root.quick-cmd-delete(id); }
                open-quick-manage(id) => {
                    root.qc-dialog-id = id;
                    root.qc-dialog-name = "";
                    root.qc-dialog-command = "";
                    root.qc-dialog-open = true;   // Rust 在 on_qc_dialog_prefill 后填充编辑值
                }
            }
```

（编辑预填由 Rust 完成：`open-quick-manage` 改为回调到 Rust 更直接——callback `quick-cmd-manage(string)`，Rust 查 store 填 qc-dialog-* 再置 open=true。实施时用 Rust 路径，上面 slint 内联仅作为新建路径保底。）
- 管理对话框（About 对话框同级，仿其遮罩+居中卡片骨架）：标题 @tr("Quick command")、两个输入框（名称 / 命令，TextInput 样式仿 find-input）、底部 GhostButton @tr("Cancel") + PrimaryButton @tr("Save")，Save → `root.quick-cmd-submit(qc-dialog-id, name-input.text, command-input.text); root.qc-dialog-open = false;`

- [ ] **Step 6.3: Rust 接线**

```rust
// run() 初始化
window.set_command_suggestions(ModelRc::from(Rc::new(VecModel::<SharedString>::default())));

// wire_callbacks：
let bar_history = cmd_history.clone();
let weak = window.as_weak();
window.on_command_bar_input(move |text: SharedString| {
    let suggestions: Vec<SharedString> = bar_history.borrow()
        .suggest(text.as_str(), 8)
        .into_iter().map(SharedString::from).collect();
    if let Some(w) = weak.upgrade() {
        w.set_command_suggestions(ModelRc::from(Rc::new(VecModel::from(suggestions))));
    }
});

let bar_handles = handles.clone();
let bar_history2 = cmd_history.clone();
let bar_trackers = input_trackers.clone();
window.on_command_bar_send(move |tab_id: SharedString, text: SharedString| {
    let cmd = text.trim().to_string();
    if cmd.is_empty() { return; }
    if let Some(handle) = bar_handles.borrow().get(tab_id.as_str()) {
        // 0x15 = Ctrl+U：先清掉远端行上可能存在的半截输入，再注入完整命令。
        let mut bytes = vec![0x15];
        bytes.extend_from_slice(cmd.as_bytes());
        bytes.push(b'\n');
        handle.send_raw(bytes);
        bar_history2.borrow_mut().add(&cmd);
        if let Some(t) = bar_trackers.borrow_mut().get_mut(tab_id.as_str()) { t.reset(); }
    }
});
```

- [ ] **Step 6.4: .po 文案**（zh：`No quick commands yet`→`还没有快捷命令`、`+ Add quick command`→`+ 添加快捷命令`、`Quick command`→`快捷命令`、`Save`→`保存`、`Cancel` 已有则跳过；en.po 恒等）
- [ ] **Step 6.5: `cargo test && cargo build` → commit**

```bash
git add -A && git commit -m "feat(ui): 底部本地命令栏——历史建议浮层（↑↓/Tab/Enter/Esc）+ 一键发送"
```

---

### Task 7: 快捷命令存储与管理

**Files:**
- Modify: `src/config.rs`（QuickCommand + 字段 + 方法 + 测试）
- Modify: `src/app.rs`（模型同步 + 回调）

- [ ] **Step 7.1: 失败测试**（config.rs tests）

```rust
#[test]
fn quick_commands_round_trip_upsert_and_remove() {
    let path = test_path("quick-commands");
    let mut store = ConfigStore::load_at(path.clone()).unwrap();
    store.upsert_quick_command(QuickCommand {
        id: "q1".into(), name: "重启nginx".into(), command: "systemctl restart nginx".into(),
    });
    store.upsert_quick_command(QuickCommand {
        id: "q1".into(), name: "重启nginx".into(), command: "sudo systemctl restart nginx".into(),
    });
    store.save().unwrap();
    let loaded = ConfigStore::load_at(path).unwrap();
    assert_eq!(loaded.quick_commands().len(), 1);
    assert_eq!(loaded.quick_commands()[0].command, "sudo systemctl restart nginx");
}
```

- [ ] **Step 7.2: 实现**

```rust
/// 一条用户自定义快捷命令（全局，不区分会话）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuickCommand {
    pub id: String,
    pub name: String,
    pub command: String,
}
```

ConfigFile 加 `#[serde(default)] pub quick_commands: Vec<QuickCommand>,`（Default impl 同步加 `quick_commands: Vec::new(),`）。ConfigStore 加：

```rust
pub fn quick_commands(&self) -> &[QuickCommand] { &self.cache.quick_commands }

pub fn upsert_quick_command(&mut self, qc: QuickCommand) {
    match self.cache.quick_commands.iter_mut().find(|x| x.id == qc.id) {
        Some(existing) => *existing = qc,
        None => self.cache.quick_commands.push(qc),
    }
}

pub fn remove_quick_command(&mut self, id: &str) {
    self.cache.quick_commands.retain(|x| x.id != id);
}
```

- [ ] **Step 7.3: app.rs 同步 + 回调**

```rust
fn sync_quick_commands_to_model(store: &ConfigStore, window: &AppWindow) {
    let rows: Vec<QuickCmdInfo> = store.quick_commands().iter().map(|q| QuickCmdInfo {
        id: q.id.clone().into(), name: q.name.clone().into(), command: q.command.clone().into(),
    }).collect();
    window.set_quick_commands(ModelRc::from(Rc::new(VecModel::from(rows))));
}
```

run()/initialise_models 调一次；wire_callbacks 加：

```rust
let qc_store = store.clone();
let weak = window.as_weak();
window.on_quick_cmd_submit(move |id, name, command| {
    let name = name.trim().to_string();
    let command = command.trim().to_string();
    if command.is_empty() { return; }
    {
        let mut s = qc_store.borrow_mut();
        s.upsert_quick_command(crate::config::QuickCommand {
            id: if id.is_empty() { uuid::Uuid::new_v4().to_string() } else { id.to_string() },
            name: if name.is_empty() { command.clone() } else { name },
            command,
        });
        if let Err(e) = s.save() { tracing::warn!("save quick command failed: {e:#}"); }
    }
    if let Some(w) = weak.upgrade() { sync_quick_commands_to_model(&qc_store.borrow(), &w); }
});

let qcd_store = store.clone();
let weak = window.as_weak();
window.on_quick_cmd_delete(move |id| {
    {
        let mut s = qcd_store.borrow_mut();
        s.remove_quick_command(id.as_str());
        if let Err(e) = s.save() { tracing::warn!("save quick command failed: {e:#}"); }
    }
    if let Some(w) = weak.upgrade() { sync_quick_commands_to_model(&qcd_store.borrow(), &w); }
});

// 编辑预填：CommandBar.open-quick-manage 改连这个回调（Step 6.2 注释处）
let qcm_store = store.clone();
let weak = window.as_weak();
window.on_quick_cmd_manage(move |id| {
    if let Some(w) = weak.upgrade() {
        let s = qcm_store.borrow();
        let existing = s.quick_commands().iter().find(|q| q.id == id.as_str());
        w.set_qc_dialog_id(id.clone());
        w.set_qc_dialog_name(existing.map(|q| q.name.clone()).unwrap_or_default().into());
        w.set_qc_dialog_command(existing.map(|q| q.command.clone()).unwrap_or_default().into());
        w.set_qc_dialog_open(true);
    }
});
```

（app.slint 相应把 `callback quick-cmd-manage(string)` 加上，CommandBar 的 open-quick-manage 转发到它。）

- [ ] **Step 7.4: `cargo test && cargo build` → commit**

```bash
git add -A && git commit -m "feat(app): 快捷命令——config 持久化、浮层一键发送、增删改对话框"
```

---

### Task 8: 密码 keyring 加密存储（无标志位、幂等迁移）

**Files:**
- Modify: `Cargo.toml`
- Create: `src/secrets.rs`
- Modify: `src/main.rs`（`mod secrets;`）
- Modify: `src/app.rs`（启动迁移、connect/重连 resolve、dialog submit、remove 清理）
- Modify: `src/cli.rs`（Run 路径 resolve）

- [ ] **Step 8.1: 依赖**

```toml
# 系统凭据库存密码（macOS Keychain / Windows Credential Manager / Secret Service）。
# 写优先 keyring、失败回退 sessions.json 明文（现状）；读时密码为空才查 keyring。
keyring = { version = "3", features = ["apple-native", "windows-native", "sync-secret-service"] }
```

- [ ] **Step 8.2: 失败测试**（secrets.rs tests——迁移逻辑注入读写闭包，不碰真实 keychain）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Secret, Session};

    fn session_with_pwd(id: &str, pwd: &str) -> Session {
        let mut s = Session::new_empty();
        s.id = id.into();
        s.password = Secret::new(pwd);
        s
    }

    #[test]
    fn migration_moves_plaintext_and_stops_on_first_failure() {
        let mut sessions = vec![
            session_with_pwd("a", "pa"),
            session_with_pwd("b", ""),
            session_with_pwd("c", "pc"),
        ];
        let mut stored: Vec<(String, String)> = Vec::new();
        let moved = migrate_plaintext_passwords(&mut sessions, |id, pwd| {
            stored.push((id.to_string(), pwd.to_string()));
            Ok(())
        });
        assert_eq!(moved, 2);
        assert_eq!(stored, vec![("a".into(), "pa".into()), ("c".into(), "pc".into())]);
        assert!(sessions.iter().all(|s| s.password.as_str().is_empty()));

        // 写入失败：明文必须原样保留（绝不能清掉没存进去的密码）。
        let mut sessions = vec![session_with_pwd("x", "px")];
        let moved = migrate_plaintext_passwords(&mut sessions, |_, _| anyhow::bail!("no backend"));
        assert_eq!(moved, 0);
        assert_eq!(sessions[0].password.as_str(), "px");
    }
}
```

- [ ] **Step 8.3: 实现 secrets.rs**

```rust
//! 系统凭据库里的会话密码。「无标志位」设计：
//! 写 → 先试 keyring，失败回退 sessions.json 明文（与旧版行为一致）；
//! 读 → Session.password 为空才查 keyring（查不到就当真没有）；
//! 迁移 → 启动时把 json 里的明文逐条搬进 keyring，成功一条清一条，
//!        失败立即停（明文保留，下次启动重试）。幂等、自愈、无状态机。

use anyhow::{Context, Result};

use crate::config::Session;

const SERVICE: &str = "LibSSH";

fn entry(session_id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, &format!("session:{session_id}"))
        .context("open keyring entry")
}

pub fn store_password(session_id: &str, password: &str) -> Result<()> {
    entry(session_id)?.set_password(password).context("keyring set_password")
}

pub fn load_password(session_id: &str) -> Option<String> {
    match entry(session_id).ok()?.get_password() {
        Ok(p) => Some(p),
        Err(keyring::Error::NoEntry) => None,
        Err(e) => {
            tracing::warn!("keyring get_password failed: {e}");
            None
        }
    }
}

pub fn delete_password(session_id: &str) {
    if let Ok(entry) = entry(session_id) {
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => tracing::warn!("keyring delete failed: {e}"),
        }
    }
}

/// 把仍以明文存放的密码搬进凭据库。返回迁移条数；
/// `write` 注入便于单测。第一次失败立即停止并保留剩余明文。
pub fn migrate_plaintext_passwords(
    sessions: &mut [Session],
    mut write: impl FnMut(&str, &str) -> Result<()>,
) -> usize {
    let mut moved = 0;
    for s in sessions.iter_mut() {
        if s.password.as_str().is_empty() {
            continue;
        }
        match write(&s.id, s.password.as_str()) {
            Ok(()) => {
                s.password = crate::config::Secret::default();
                moved += 1;
            }
            Err(e) => {
                tracing::warn!("password migration stopped: {e:#}");
                break;
            }
        }
    }
    moved
}

/// 连接前解析会话密码：json 明文优先（兼容回退场景），否则查 keyring。
pub fn resolve_session_password(session: &mut Session) {
    if session.auth == crate::config::AuthMethod::Password
        && session.password.as_str().is_empty()
    {
        if let Some(p) = load_password(&session.id) {
            session.password = crate::config::Secret::new(p);
        }
    }
}
```

（config.rs 需要 `sessions_mut` 已存在——app.rs 迁移用它拿 `&mut Vec<Session>`，去掉其 `#[allow(dead_code)]`。）

- [ ] **Step 8.4: 接线**
- `app.rs run()`（ConfigStore::load 后）：

```rust
{
    let mut s = store.borrow_mut();
    let moved = crate::secrets::migrate_plaintext_passwords(
        s.sessions_mut(), crate::secrets::store_password,
    );
    if moved > 0 {
        if let Err(e) = s.save() { tracing::warn!("save after migration failed: {e:#}"); }
        tracing::info!("migrated {moved} session password(s) into the OS keyring");
    }
}
```

- connect 闭包：`let Some(mut session) = ... else ...` 后 `crate::secrets::resolve_session_password(&mut session);`（在 clone 给 tab_sessions/spawn 之前——重连映射存的是已解析副本，重连无需再查）。
- `on_session_dialog_submit`：构造 `new_session` 后、upsert 前：

```rust
// 新密码优先进 keyring；成功则 json 落空串，失败保持明文（旧行为）。
if !draft.password.is_empty() {
    if crate::secrets::store_password(&new_session.id, draft.password.as_str()).is_ok() {
        new_session.password = Secret::default();
    }
}
```

注意：draft.password 为空沿用旧值的现有分支不动（旧值若是空+keyring 也自然成立）。
- `on_remove_session`：`s.remove(...)` 后 `crate::secrets::delete_password(&id.to_string());`
- `cli.rs` Run 分支：`.clone();` 后加：

```rust
let mut session = session;
crate::secrets::resolve_session_password(&mut session);
```

（`secret_values_for_session` 在 resolve 之后调用，redaction 仍能覆盖真实密码。）

- [ ] **Step 8.5: `cargo test && cargo build` → 手动验收点：启动一次后打开 sessions.json 确认 password 字段为空串；macOS「钥匙串访问」可见 `LibSSH / session:<id>` 条目；连接照常。→ commit**

```bash
git add -A && git commit -m "feat(secrets): 会话密码迁入系统凭据库——写优先 keyring、读空回查、幂等迁移"
```

---

### Task 9: 终验 + 文档

- [ ] **Step 9.1:** `cargo test && cargo build --release`，全过；`cargo run` 冒烟：连接、断开重连、命令栏补全、快捷命令、密码迁移。
- [ ] **Step 9.2:** 更新 OB 清单勾选状态（P0 四项打勾 + 注明版本）；CHANGELOG/README 若有对应段落同步。
- [ ] **Step 9.3:** 最终 commit + 汇报。

---

## Self-Review 备忘

- 类型一致性检查点：`SessionIoCtx` 字段名 / `conn_lost`（Rust）↔ `conn-lost`（slint）/ `QuickCmdInfo` 三字段 / `reconnect_attempts`。
- 已知取舍：编辑会话后自动重连仍用旧配置（tab_sessions 存连接时副本）；快捷命令不支持参数占位（YAGNI，批次 B 后评估）；suggest 大小写敏感。
