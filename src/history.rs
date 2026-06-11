//! 本地命令历史（全局、跨会话持久化）+ 终端输入行尽力跟踪。
//!
//! 跟踪策略：只在能确定本地缓冲与远端行一致时记录该行。Tab 补全、
//! 箭头历史导航、Ctrl/Alt 组合等会让远端行偏离本地缓冲 → 标记
//! poisoned，该行在 Enter 时丢弃。Ctrl+C / Ctrl+U 在 shell 里把整行
//! 作废/清空，等价于干净的新行。

use std::path::PathBuf;

pub const MAX_ENTRIES: usize = 1000;
const MAX_LINE_LEN: usize = 500;

/// 全局命令历史。`entries` 按旧 → 新存放，重复命令去重后挪到最新。
pub struct CommandHistory {
    path: Option<PathBuf>,
    entries: Vec<String>,
}

impl CommandHistory {
    pub fn load_default() -> Self {
        match directories::ProjectDirs::from("dev", "LibSSH", "LibSSH") {
            Some(dirs) => Self::load_at(dirs.config_dir().join("command_history.json")),
            None => Self::in_memory(),
        }
    }

    pub fn load_at(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            entries,
        }
    }

    pub fn in_memory() -> Self {
        Self {
            path: None,
            entries: Vec::new(),
        }
    }

    #[cfg(test)]
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

    /// 前缀匹配，最新优先；完全等于前缀的条目不重复给出。空前缀不弹建议。
    pub fn suggest(&self, prefix: &str, limit: usize) -> Vec<String> {
        if prefix.is_empty() {
            return Vec::new();
        }
        self.entries
            .iter()
            .rev()
            .filter(|e| e.starts_with(prefix) && e.as_str() != prefix)
            .take(limit)
            .cloned()
            .collect()
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

/// 终端输入行的尽力跟踪器（每个终端标签一个）。
#[derive(Default)]
pub struct InputTracker {
    line: String,
    poisoned: bool,
}

impl InputTracker {
    /// 处理一次按键（与 on_send_key 同源的 key / modifiers）。
    /// 返回 `Some(line)` 表示一行被干净地提交，应记入历史。
    pub fn feed_key(&mut self, key: &str, ctrl: bool, alt: bool) -> Option<String> {
        let first = key.chars().next();
        // Enter：提交（poisoned 或空行 → 不记录，但都重置）
        if matches!(key, "\r" | "\n") && !ctrl && !alt {
            let done = (!self.poisoned && !self.line.trim().is_empty())
                .then(|| self.line.trim().to_string());
            self.reset();
            return done;
        }
        // Ctrl+C / Ctrl+U：当前行作废/清空 → 干净的新行
        let is_ctrl_c = (ctrl && matches!(key, "c" | "C")) || key == "\u{0003}";
        let is_ctrl_u = (ctrl && matches!(key, "u" | "U")) || key == "\u{0015}";
        if is_ctrl_c || is_ctrl_u {
            self.reset();
            return None;
        }
        // Backspace：本地同步删除
        if matches!(key, "\u{0008}" | "\u{007f}") && !ctrl && !alt {
            self.line.pop();
            return None;
        }
        // 污染源：Tab（远端补全）、Slint 专用键区（箭头/Home/End/F1…）、
        // 其余 C0 控制码、任何 Ctrl/Alt 组合 —— 本地不再可信。
        let is_special = first.is_some_and(|c| ('\u{F700}'..='\u{F8FF}').contains(&c));
        let is_control = key.chars().count() == 1 && first.is_some_and(|c| (c as u32) < 0x20);
        if ctrl || alt || key == "\t" || is_special || is_control {
            self.poisoned = true;
            return None;
        }
        // 可打印文本（含 IME 多字符提交）
        self.line.push_str(key);
        None
    }

    /// 粘贴：单行并入缓冲；含换行则远端可能直接执行若干行 → 污染。
    pub fn feed_paste(&mut self, text: &str) {
        if text.contains('\n') || text.contains('\r') {
            self.poisoned = true;
        } else {
            self.line.push_str(text);
        }
    }

    /// 命令栏代发命令后远端行被消费（Ctrl+U + 命令 + 回车），
    /// 本地从干净空行重新开始。
    pub fn reset(&mut self) {
        self.line.clear();
        self.poisoned = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_dedups_to_most_recent_and_caps() {
        let mut h = CommandHistory::in_memory();
        h.add("ls");
        h.add("cd /tmp");
        h.add("ls");
        assert_eq!(h.entries(), &["cd /tmp".to_string(), "ls".to_string()]);
        for i in 0..1100 {
            h.add(&format!("cmd{i}"));
        }
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
        h.add("git status");
        h.add("git push");
        h.add("ls");
        assert_eq!(
            h.suggest("git", 8),
            vec!["git push".to_string(), "git status".to_string()]
        );
        assert!(h.suggest("", 8).is_empty());
        assert_eq!(h.suggest("git pu", 8), vec!["git push".to_string()]);
        // 输入与历史完全一致时不重复建议
        assert_eq!(h.suggest("ls", 8), Vec::<String>::new());
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
        for c in ["l", "s", " ", "-", "l"] {
            assert_eq!(t.feed_key(c, false, false), None);
        }
        assert_eq!(t.feed_key("\r", false, false), Some("ls -l".to_string()));
        assert_eq!(t.feed_key("\r", false, false), None); // 空行不记
    }

    #[test]
    fn tracker_backspace_edits_line() {
        let mut t = InputTracker::default();
        t.feed_key("l", false, false);
        t.feed_key("a", false, false);
        t.feed_key("\u{0008}", false, false);
        t.feed_key("s", false, false);
        assert_eq!(t.feed_key("\n", false, false), Some("ls".to_string()));
    }

    #[test]
    fn tracker_poisons_on_tab_arrows_and_ctrl() {
        // 远端补全 / 历史导航 / 控制组合 → 本地缓冲不可信，该行放弃。
        for (key, ctrl) in [("\t", false), ("\u{F700}", false), ("a", true)] {
            let mut t = InputTracker::default();
            t.feed_key("l", false, false);
            t.feed_key(key, ctrl, false);
            t.feed_key("s", false, false);
            assert_eq!(t.feed_key("\r", false, false), None, "poison {key:?}");
        }
    }

    #[test]
    fn tracker_ctrl_c_and_ctrl_u_reset_clean() {
        let mut t = InputTracker::default();
        t.feed_key("x", false, false);
        t.feed_key("\u{0003}", false, false); // Ctrl+C（控制码形态）
        t.feed_key("l", false, false);
        t.feed_key("s", false, false);
        assert_eq!(t.feed_key("\r", false, false), Some("ls".to_string()));
        t.feed_key("y", false, false);
        t.feed_key("u", true, false); // Ctrl+U（modifier 形态）
        t.feed_key("p", false, false);
        t.feed_key("s", false, false);
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
