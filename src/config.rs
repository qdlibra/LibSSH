use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;
use zeroize::Zeroize;

const BUILTIN_DENIED_COMMANDS: &[&str] = &[
    "rm",
    "dd",
    "mkfs",
    "shutdown",
    "reboot",
    "halt",
    "poweroff",
    "passwd",
    "userdel",
    "groupdel",
    "chown",
    "chmod",
    "sudo",
    "su",
    "env",
    "printenv",
    "set",
    "history",
    "kubectl get secret",
    "kubectl describe secret",
    "aws secretsmanager",
    "gcloud secrets",
    "op item",
    "pass",
    "security find-generic-password",
];

/// 只读诊断命令预设：`LibSSH skill policy allow-preset readonly` 一键导入。
/// 多词条目只放行该子命令（前缀匹配按词边界，见 `command_matches_rule`）。
/// 刻意不收裸 `ip` / `docker` / `kubectl` / `mount`——它们的子命令含写操作；
/// `find` / `wget` / `curl` 的写能力是宽松集已知权衡（spec「安全边界」节）。
pub const READONLY_PRESET: &[&str] = &[
    // 系统状态
    "uptime",
    "w",
    "who",
    "last",
    "date",
    "hostname",
    "uname",
    "whoami",
    "id",
    "nproc",
    "lscpu",
    "lsblk",
    "findmnt",
    // 文件只读
    "ls",
    "cat",
    "head",
    "tail",
    "stat",
    "file",
    "wc",
    "du",
    "df",
    "find",
    "grep",
    // 进程/资源
    "ps",
    "top",
    "free",
    "vmstat",
    "iostat",
    "lsof",
    // 服务/日志
    "systemctl status",
    "journalctl",
    "dmesg",
    // 网络诊断
    "netstat",
    "ss",
    "ip addr show",
    "ip route show",
    "ping",
    "traceroute",
    "dig",
    "nslookup",
    "host",
    "curl",
    "wget",
    // 容器/编排（内置 deny 已拦 kubectl get/describe secret）
    "docker ps",
    "docker logs",
    "docker images",
    "docker stats",
    "kubectl get",
    "kubectl describe",
    "kubectl logs",
    // 计划任务
    "crontab -l",
];

/// 按名字取预设清单；未知名字返回 None（调用方负责报错口径）。
pub fn preset_commands(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "readonly" => Some(READONLY_PRESET),
        _ => None,
    }
}

/// A secret string whose heap buffer is zeroed when dropped.
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() {
            "Secret(\"\")"
        } else {
            "Secret(***)"
        })
    }
}

impl Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Ok(Secret(String::deserialize(d)?))
    }
}

/// How a session authenticates.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    Password,
    Key,
}

impl AuthMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthMethod::Password => "password",
            AuthMethod::Key => "key",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "key" => AuthMethod::Key,
            _ => AuthMethod::Password,
        }
    }
}

/// A single saved SSH target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthMethod,
    #[serde(default)]
    pub password: Secret,
    #[serde(default)]
    pub private_key_path: String,
    /// Optional outbound proxy, e.g. "socks5://127.0.0.1:1080" or
    /// "http://user:pass@host:8080". Empty = use $ALL_PROXY, else direct.
    #[serde(default)]
    pub proxy: String,
    #[serde(default)]
    pub last_used: Option<String>,
    /// User-defined group/folder label for the connection list. Empty = ungrouped.
    #[serde(default)]
    pub group: String,
    /// 本地端口转发（-L）规格列表。
    #[serde(default)]
    pub tunnels: Vec<TunnelSpec>,
    /// 单跳跳板机：经由另一个已保存会话（其 id）建立到本会话的连接。None = 直连。
    #[serde(default)]
    pub jump_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiSessionSummary {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthMethod,
    pub has_password: bool,
    pub has_private_key: bool,
}

impl From<&Session> for AiSessionSummary {
    fn from(session: &Session) -> Self {
        Self {
            id: session.id.clone(),
            name: session.name.clone(),
            host: session.host.clone(),
            port: session.port,
            user: session.user.clone(),
            auth: session.auth,
            has_password: !session.password.as_str().is_empty(),
            has_private_key: !session.private_key_path.trim().is_empty(),
        }
    }
}

impl Session {
    pub fn new_empty() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: String::new(),
            host: String::new(),
            port: 22,
            user: "root".into(),
            auth: AuthMethod::Password,
            password: Secret::default(),
            private_key_path: String::new(),
            proxy: String::new(),
            last_used: None,
            group: String::new(),
            tunnels: Vec::new(),
            jump_session_id: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiSkillConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    #[serde(default)]
    pub denied_commands: Vec<String>,
}

impl AiSkillConfig {
    pub fn evaluate_command(&self, command: &str) -> std::result::Result<(), String> {
        let command = command.trim();
        if command.is_empty() {
            return Err("empty command is not allowed".to_string());
        }
        if !self.enabled {
            return Err("AI skill CLI is disabled".to_string());
        }
        if matches_any_command(command, BUILTIN_DENIED_COMMANDS.iter().copied()) {
            return Err("command is blocked by the built-in safety policy".to_string());
        }
        if contains_sensitive_assignment(command) {
            return Err("command appears to contain sensitive inline data".to_string());
        }
        if matches_any_command(command, self.denied_commands.iter().map(String::as_str)) {
            return Err("command is blocked by the configured deny list".to_string());
        }
        if self.allowed_commands.is_empty() {
            return Err("no allowed commands are configured".to_string());
        }
        if !matches_any_command(command, self.allowed_commands.iter().map(String::as_str)) {
            return Err("command is not in the configured allow list".to_string());
        }
        Ok(())
    }
}

fn matches_any_command<'a>(command: &str, rules: impl Iterator<Item = &'a str>) -> bool {
    rules
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
        .any(|rule| command_matches_rule(command, rule))
}

fn command_matches_rule(command: &str, rule: &str) -> bool {
    command == rule
        || command
            .strip_prefix(rule)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

fn contains_sensitive_assignment(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "token",
        "api_key",
        "apikey",
        "secret",
        "credential",
        "private_key",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        && (command.contains('=') || command.contains(':'))
}

/// 一条用户自定义快捷命令（全局，不区分会话）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuickCommand {
    pub id: String,
    pub name: String,
    pub command: String,
}

/// 一个用户可管理的连接分组：名称 + 颜色（hex 如 "#2563eb"，"" = 无色）。
/// `name == ""` 表示内置「默认」分组——未分组会话的兜底，不可删除/改名。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    #[serde(default)]
    pub color: String,
}

/// 一条本地端口转发（-L）规格：本机 `bind_addr:bind_port` 上的连接经 SSH 隧道
/// 转发到 `dest_host:dest_port`（由远端服务器解析）。`bind_addr` 空 = 仅听 127.0.0.1。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelSpec {
    #[serde(default)]
    pub bind_addr: String,
    pub bind_port: u16,
    pub dest_host: String,
    pub dest_port: u16,
}

// parse_line/to_line 在 Task 8（UI 隧道编辑）接线前是死代码；接线后移除本豁免。
#[allow(dead_code)]
impl TunnelSpec {
    /// 解析一行 `[bind_addr:]bind_port:dest_host:dest_port`。
    pub fn parse_line(line: &str) -> std::result::Result<TunnelSpec, String> {
        let line = line.trim();
        let parts: Vec<&str> = line.split(':').collect();
        let (bind_addr, bind_port, dest_host, dest_port) = match parts.as_slice() {
            [bp, dh, dp] => (String::new(), *bp, *dh, *dp),
            [ba, bp, dh, dp] => (ba.to_string(), *bp, *dh, *dp),
            _ => return Err(format!("隧道格式应为 [bind:]port:host:port，得到 `{line}`")),
        };
        let bind_port: u16 = bind_port
            .parse()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| format!("本地端口非法：`{bind_port}`"))?;
        let dest_port: u16 = dest_port
            .parse()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| format!("目标端口非法：`{dest_port}`"))?;
        if dest_host.trim().is_empty() {
            return Err("目标主机为空".into());
        }
        Ok(TunnelSpec {
            bind_addr,
            bind_port,
            dest_host: dest_host.trim().to_string(),
            dest_port,
        })
    }

    /// 反向格式化为规范行（bind_addr 为空时省略），供 UI 文本框回显。
    pub fn to_line(&self) -> String {
        if self.bind_addr.is_empty() {
            format!("{}:{}:{}", self.bind_port, self.dest_host, self.dest_port)
        } else {
            format!(
                "{}:{}:{}:{}",
                self.bind_addr, self.bind_port, self.dest_host, self.dest_port
            )
        }
    }
}

/// 把多行文本解析为隧道列表：跳过空行与非法行（一期宽松，UI 内联校验留后续）。
// Task 8（UI submit）接线前是死代码；接线后移除本豁免。
#[allow(dead_code)]
pub fn parse_tunnel_lines(text: &str) -> Vec<TunnelSpec> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| TunnelSpec::parse_line(l).ok())
        .collect()
}

/// 内置预设分组（首次运行 seed）：默认(无色)/本地(蓝)/测试(橙)/生产(绿)。
fn preset_groups() -> Vec<Group> {
    vec![
        Group {
            name: String::new(),
            color: String::new(),
        },
        Group {
            name: "本地".into(),
            color: "#2563eb".into(),
        },
        Group {
            name: "测试".into(),
            color: "#c2740a".into(),
        },
        Group {
            name: "生产".into(),
            color: "#16a34a".into(),
        },
    ]
}

fn default_true() -> bool {
    true
}

/// On-disk layout. Keep additive to ease forward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub sessions: Vec<Session>,
    /// Preset SFTP download directory. Empty = ask each time.
    #[serde(default)]
    pub download_dir: String,
    /// UI language code: "zh" (default) or "en".
    #[serde(default)]
    pub language: String,
    /// Local-only guardrails for CLI access by AI coding agents.
    #[serde(default)]
    pub ai_skill: AiSkillConfig,
    /// 启动时是否自动检查更新（默认开）。
    #[serde(default = "default_true")]
    pub auto_check_update: bool,
    /// 上次检查更新的 unix 时间戳（秒），用于 24h 节流。
    #[serde(default)]
    pub last_update_check: Option<i64>,
    /// 用户"跳过此版本"记录的 tag，如 "v0.2.4"。
    #[serde(default)]
    pub skipped_version: Option<String>,
    /// 底部命令栏的快捷命令（全局共享）。
    #[serde(default)]
    pub quick_commands: Vec<QuickCommand>,
    /// 连接分组注册表（有序，顺序即快速连接里的展示序）。
    #[serde(default)]
    pub groups: Vec<Group>,
}

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
            quick_commands: Vec::new(),
            groups: Vec::new(),
        }
    }
}

pub struct ConfigStore {
    path: PathBuf,
    cache: ConfigFile,
}

impl ConfigStore {
    pub fn load() -> Result<Self> {
        Self::load_at(Self::config_path()?)
    }

    fn load_at(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create config dir {}", parent.display()))?;
        }

        let mut cache = if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            match serde_json::from_str::<ConfigFile>(&raw) {
                Ok(cfg) => cfg,
                Err(err) => {
                    let backup = path.with_extension("json.broken");
                    let _ = fs::rename(&path, &backup);
                    tracing::warn!(
                        "config file was corrupt ({err}); backed up to {}",
                        backup.display()
                    );
                    ConfigFile::default()
                }
            }
        } else {
            ConfigFile::default()
        };

        // Seed built-in groups on a fresh config; always keep a default "" group
        // as the fallback bucket for ungrouped sessions.
        if cache.groups.is_empty() {
            cache.groups = preset_groups();
        } else if !cache.groups.iter().any(|g| g.name.is_empty()) {
            cache.groups.insert(
                0,
                Group {
                    name: String::new(),
                    color: String::new(),
                },
            );
        }

        Ok(Self { path, cache })
    }

    fn config_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("dev", "LibSSH", "LibSSH")
            .context("could not determine user config directory")?;
        Ok(dirs.config_dir().join("sessions.json"))
    }

    pub fn sessions(&self) -> &[Session] {
        &self.cache.sessions
    }

    pub fn sessions_mut(&mut self) -> &mut Vec<Session> {
        &mut self.cache.sessions
    }

    pub fn upsert(&mut self, session: Session) {
        if let Some(existing) = self.cache.sessions.iter_mut().find(|s| s.id == session.id) {
            *existing = session;
        } else {
            self.cache.sessions.push(session);
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.cache.sessions.retain(|s| s.id != id);
    }

    pub fn get(&self, id: &str) -> Option<&Session> {
        self.cache.sessions.iter().find(|s| s.id == id)
    }

    /// 解析跳板会话：按 `jump_session_id` 查另一个已保存会话并克隆返回（不解密密码）。
    /// 自跳 / 空 id / 不存在 → None。调用方负责对返回值做 `resolve_session_password`。
    pub fn resolve_jump(&self, session: &Session) -> Option<Session> {
        let id = session.jump_session_id.as_deref()?;
        if id.is_empty() || id == session.id {
            return None;
        }
        self.get(id).cloned()
    }

    pub fn download_dir(&self) -> &str {
        &self.cache.download_dir
    }

    pub fn set_download_dir(&mut self, dir: String) {
        self.cache.download_dir = dir;
    }

    pub fn language(&self) -> &str {
        if self.cache.language.is_empty() {
            "zh"
        } else {
            &self.cache.language
        }
    }

    pub fn set_language(&mut self, lang: String) {
        self.cache.language = lang;
    }

    pub fn auto_check_update(&self) -> bool {
        self.cache.auto_check_update
    }

    // 预留给设置界面的自动更新开关；UI 接入前仅测试使用。
    #[allow(dead_code)]
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

    pub fn quick_commands(&self) -> &[QuickCommand] {
        &self.cache.quick_commands
    }

    pub fn upsert_quick_command(&mut self, qc: QuickCommand) {
        match self.cache.quick_commands.iter_mut().find(|x| x.id == qc.id) {
            Some(existing) => *existing = qc,
            None => self.cache.quick_commands.push(qc),
        }
    }

    pub fn remove_quick_command(&mut self, id: &str) {
        self.cache.quick_commands.retain(|x| x.id != id);
    }

    // --- Connection groups -------------------------------------------------
    pub fn groups(&self) -> &[Group] {
        &self.cache.groups
    }

    /// Append a new named group. Empty / duplicate names are ignored (the empty
    /// name is reserved for the built-in default group).
    pub fn add_group(&mut self, name: &str, color: &str) {
        let name = name.trim();
        if name.is_empty() || self.cache.groups.iter().any(|g| g.name == name) {
            return;
        }
        self.cache.groups.push(Group {
            name: name.to_string(),
            color: color.to_string(),
        });
    }

    /// Remove the group at `idx` (the default "" group is protected). Sessions
    /// that referenced it fall back to the default group.
    pub fn remove_group_at(&mut self, idx: usize) {
        let Some(g) = self.cache.groups.get(idx) else {
            return;
        };
        if g.name.is_empty() {
            return;
        }
        let name = g.name.clone();
        self.cache.groups.remove(idx);
        for s in self.cache.sessions.iter_mut() {
            if s.group == name {
                s.group.clear();
            }
        }
    }

    /// Rename the group at `idx` (default group protected; empty / duplicate new
    /// names ignored), cascading the change to every session that referenced it.
    pub fn rename_group_at(&mut self, idx: usize, new_name: &str) {
        let new_name = new_name.trim().to_string();
        if new_name.is_empty() {
            return;
        }
        let Some(g) = self.cache.groups.get(idx) else {
            return;
        };
        let old = g.name.clone();
        if old.is_empty() || old == new_name || self.cache.groups.iter().any(|x| x.name == new_name)
        {
            return;
        }
        self.cache.groups[idx].name = new_name.clone();
        for s in self.cache.sessions.iter_mut() {
            if s.group == old {
                s.group = new_name.clone();
            }
        }
    }

    pub fn set_group_color_at(&mut self, idx: usize, color: &str) {
        if let Some(g) = self.cache.groups.get_mut(idx) {
            g.color = color.to_string();
        }
    }

    /// Move the group at `idx` one slot up (dir < 0) or down (dir > 0).
    pub fn move_group(&mut self, idx: usize, dir: i32) {
        let len = self.cache.groups.len();
        let target = idx as i32 + dir;
        if idx >= len || target < 0 || target as usize >= len {
            return;
        }
        self.cache.groups.swap(idx, target as usize);
    }

    pub fn ai_skill(&self) -> &AiSkillConfig {
        &self.cache.ai_skill
    }

    pub fn ai_skill_mut(&mut self) -> &mut AiSkillConfig {
        &mut self.cache.ai_skill
    }

    pub fn save(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(&self.cache)?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, raw).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("failed to finalise {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_path(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("LibSSH-config-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("sessions.json")
    }

    #[test]
    fn save_then_load_round_trips_sessions_and_settings() {
        let path = test_path("round-trip");
        let mut store = ConfigStore::load_at(path.clone()).unwrap();

        let mut session = Session::new_empty();
        session.id = "session-1".to_string();
        session.name = "Production".to_string();
        session.host = "prod.example.com".to_string();
        session.port = 2200;
        session.user = "deploy".to_string();
        session.auth = AuthMethod::Key;
        session.password = Secret::new("secret");
        session.private_key_path = "/keys/prod".to_string();
        session.proxy = "socks5://127.0.0.1:1080".to_string();
        session.last_used = Some("2026-06-08T00:00:00Z".to_string());

        store.upsert(session);
        store.set_download_dir("/tmp/downloads".to_string());
        store.set_language("en".to_string());
        store.save().unwrap();

        let loaded = ConfigStore::load_at(path).unwrap();
        let session = loaded.get("session-1").unwrap();
        assert_eq!(session.name, "Production");
        assert_eq!(session.host, "prod.example.com");
        assert_eq!(session.port, 2200);
        assert_eq!(session.user, "deploy");
        assert_eq!(session.auth, AuthMethod::Key);
        assert_eq!(session.password.as_str(), "secret");
        assert_eq!(session.private_key_path, "/keys/prod");
        assert_eq!(session.proxy, "socks5://127.0.0.1:1080");
        assert_eq!(session.last_used.as_deref(), Some("2026-06-08T00:00:00Z"));
        assert_eq!(loaded.download_dir(), "/tmp/downloads");
        assert_eq!(loaded.language(), "en");
    }

    #[test]
    fn corrupt_json_is_backed_up_and_loads_default_config() {
        let path = test_path("corrupt-json");
        fs::write(&path, "{ not valid json").unwrap();

        let loaded = ConfigStore::load_at(path.clone()).unwrap();

        assert!(loaded.sessions().is_empty());
        assert_eq!(loaded.download_dir(), "");
        assert_eq!(loaded.language(), "zh");
        assert!(!path.exists());
        assert!(path.with_extension("json.broken").exists());
    }

    #[test]
    fn missing_optional_fields_load_with_defaults() {
        let path = test_path("missing-optional-fields");
        fs::write(
            &path,
            r#"{
  "sessions": [
    {
      "id": "session-1",
      "name": "Legacy",
      "host": "legacy.example.com",
      "port": 22,
      "user": "root",
      "auth": "password"
    }
  ]
}"#,
        )
        .unwrap();

        let loaded = ConfigStore::load_at(path).unwrap();
        let session = loaded.get("session-1").unwrap();

        assert!(session.password.as_str().is_empty());
        assert_eq!(session.private_key_path, "");
        assert_eq!(session.proxy, "");
        assert_eq!(session.last_used, None);
        assert_eq!(loaded.download_dir(), "");
        assert_eq!(loaded.language(), "zh");
    }

    #[test]
    fn upsert_replaces_existing_session_and_remove_deletes_by_id() {
        let path = test_path("upsert-remove");
        let mut store = ConfigStore::load_at(path).unwrap();

        let mut first = Session::new_empty();
        first.id = "session-1".to_string();
        first.name = "Old".to_string();
        store.upsert(first);

        let mut replacement = Session::new_empty();
        replacement.id = "session-1".to_string();
        replacement.name = "New".to_string();
        store.upsert(replacement);

        assert_eq!(store.sessions().len(), 1);
        assert_eq!(store.get("session-1").unwrap().name, "New");

        store.remove("session-1");
        assert!(store.sessions().is_empty());
    }

    #[test]
    fn ai_skill_policy_is_disabled_and_rejects_commands_by_default() {
        let policy = AiSkillConfig::default();

        assert!(!policy.enabled);
        assert!(policy.allowed_commands.is_empty());
        assert!(policy.denied_commands.is_empty());
        assert!(policy.evaluate_command("uptime").is_err());
    }

    #[test]
    fn ai_skill_policy_denied_commands_take_precedence_over_allowed_commands() {
        let mut policy = AiSkillConfig {
            enabled: true,
            allowed_commands: vec!["systemctl".to_string()],
            denied_commands: vec!["systemctl reboot".to_string()],
        };

        assert!(policy.evaluate_command("systemctl status sshd").is_ok());
        assert!(policy.evaluate_command("systemctl reboot").is_err());

        policy.denied_commands.clear();
        assert!(policy.evaluate_command("rm -rf /").is_err());
        assert!(policy.evaluate_command("env").is_err());
        assert!(policy.evaluate_command("echo token=abc123").is_err());
    }

    #[test]
    fn readonly_preset_commands_all_pass_policy_evaluation() {
        let policy = AiSkillConfig {
            enabled: true,
            allowed_commands: READONLY_PRESET.iter().map(|s| s.to_string()).collect(),
            denied_commands: Vec::new(),
        };
        for cmd in READONLY_PRESET {
            assert!(
                policy.evaluate_command(cmd).is_ok(),
                "preset command should pass: {cmd}"
            );
        }
        // 带参数的典型形态也要落在词边界前缀内
        assert!(policy.evaluate_command("df -h").is_ok());
        assert!(policy
            .evaluate_command("journalctl -u nginx --since today")
            .is_ok());
        assert!(policy
            .evaluate_command("docker logs --tail 100 web")
            .is_ok());
        assert!(policy.evaluate_command("systemctl status sshd").is_ok());
    }

    #[test]
    fn readonly_preset_does_not_unlock_dangerous_commands() {
        let policy = AiSkillConfig {
            enabled: true,
            allowed_commands: READONLY_PRESET.iter().map(|s| s.to_string()).collect(),
            denied_commands: Vec::new(),
        };
        for cmd in [
            "rm -rf /",           // 内置 deny
            "sudo ls",            // 内置 deny
            "env",                // 内置 deny
            "kubectl get secret", // 内置 deny 优先于预设的 kubectl get
            "docker rm web",      // 预设只收 docker 只读子命令
            "docker exec -it web sh",
            "kubectl delete pod web",
            "mount /dev/sda1 /mnt",    // 刻意不收 mount
            "crontab -r",              // 预设只收 crontab -l
            "ip link set eth0 down",   // 预设只收 ip addr show / ip route show
            "systemctl restart nginx", // 预设只收 systemctl status
        ] {
            assert!(
                policy.evaluate_command(cmd).is_err(),
                "must stay blocked: {cmd}"
            );
        }
    }

    #[test]
    fn preset_lookup_finds_readonly_and_rejects_unknown() {
        assert_eq!(preset_commands("readonly"), Some(READONLY_PRESET));
        assert_eq!(preset_commands("yolo"), None);
    }

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

    #[test]
    fn groups_seed_presets_and_cascade_rename_delete_move() {
        let path = test_path("groups-cascade");
        let mut store = ConfigStore::load_at(path).unwrap();

        // Fresh config seeds the four presets (default "" first).
        let names: Vec<&str> = store.groups().iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["", "本地", "测试", "生产"]);

        // A session that lives in the 测试 group.
        store.upsert(Session {
            group: "测试".into(),
            ..Session::new_empty()
        });
        let sid = store.sessions()[0].id.clone();

        // Rename 测试 → 预发 cascades to the session.
        store.rename_group_at(2, "预发");
        assert_eq!(store.groups()[2].name, "预发");
        assert_eq!(store.get(&sid).unwrap().group, "预发");

        // The default "" group (idx 0) is protected from rename and delete.
        store.rename_group_at(0, "X");
        store.remove_group_at(0);
        assert_eq!(store.groups()[0].name, "");

        // Deleting 预发 drops its session back to the default ("").
        store.remove_group_at(2);
        assert!(store.groups().iter().all(|g| g.name != "预发"));
        assert_eq!(store.get(&sid).unwrap().group, "");

        // Reorder: move 本地 (idx 1) down one slot.
        store.move_group(1, 1);
        assert_eq!(store.groups()[1].name, "生产");
        assert_eq!(store.groups()[2].name, "本地");
    }

    #[test]
    fn quick_commands_round_trip_upsert_and_remove() {
        let path = test_path("quick-commands");
        let mut store = ConfigStore::load_at(path.clone()).unwrap();
        store.upsert_quick_command(QuickCommand {
            id: "q1".into(),
            name: "重启nginx".into(),
            command: "systemctl restart nginx".into(),
        });
        store.upsert_quick_command(QuickCommand {
            id: "q1".into(),
            name: "重启nginx".into(),
            command: "sudo systemctl restart nginx".into(),
        });
        store.save().unwrap();

        let loaded = ConfigStore::load_at(path.clone()).unwrap();
        assert_eq!(loaded.quick_commands().len(), 1);
        assert_eq!(
            loaded.quick_commands()[0].command,
            "sudo systemctl restart nginx"
        );

        let mut loaded = loaded;
        loaded.remove_quick_command("q1");
        loaded.save().unwrap();
        assert!(ConfigStore::load_at(path)
            .unwrap()
            .quick_commands()
            .is_empty());
    }

    #[test]
    fn ai_visible_session_summary_redacts_credentials() {
        let mut session = Session::new_empty();
        session.id = "session-1".to_string();
        session.name = "Production".to_string();
        session.host = "prod.example.com".to_string();
        session.port = 2200;
        session.user = "deploy".to_string();
        session.auth = AuthMethod::Key;
        session.password = Secret::new("super-secret");
        session.private_key_path = "/Users/me/.ssh/prod.pem".to_string();

        let summary = AiSessionSummary::from(&session);
        let json = serde_json::to_string(&summary).unwrap();

        assert!(json.contains("Production"));
        assert!(json.contains("prod.example.com"));
        assert!(json.contains("\"has_password\":true"));
        assert!(json.contains("\"has_private_key\":true"));
        assert!(!json.contains("super-secret"));
        assert!(!json.contains("prod.pem"));
        assert!(!json.contains("/Users/me/.ssh"));
    }

    #[test]
    fn session_round_trips_tunnels_and_jump() {
        let path = test_path("tunnels-jump");
        let mut store = ConfigStore::load_at(path.clone()).unwrap();

        let mut s = Session::new_empty();
        s.id = "tgt".into();
        s.host = "10.0.0.9".into();
        s.tunnels = vec![
            TunnelSpec {
                bind_addr: String::new(),
                bind_port: 8080,
                dest_host: "localhost".into(),
                dest_port: 80,
            },
            TunnelSpec {
                bind_addr: "127.0.0.1".into(),
                bind_port: 5432,
                dest_host: "db.internal".into(),
                dest_port: 5432,
            },
        ];
        s.jump_session_id = Some("bastion".into());
        store.upsert(s);
        store.save().unwrap();

        let loaded = ConfigStore::load_at(path).unwrap();
        let s = loaded.get("tgt").unwrap();
        assert_eq!(s.tunnels.len(), 2);
        assert_eq!(s.tunnels[0].bind_port, 8080);
        assert_eq!(s.tunnels[1].dest_host, "db.internal");
        assert_eq!(s.jump_session_id.as_deref(), Some("bastion"));
    }

    #[test]
    fn legacy_session_without_tunnel_fields_defaults_empty() {
        let path = test_path("legacy-no-tunnels");
        fs::write(&path, r#"{ "sessions": [ { "id": "x", "name": "X", "host": "h", "port": 22, "user": "root", "auth": "password" } ] }"#).unwrap();
        let loaded = ConfigStore::load_at(path).unwrap();
        let s = loaded.get("x").unwrap();
        assert!(s.tunnels.is_empty());
        assert_eq!(s.jump_session_id, None);
    }

    #[test]
    fn tunnel_parse_line_three_and_four_parts() {
        let a = TunnelSpec::parse_line("8080:localhost:80").unwrap();
        assert_eq!(
            a,
            TunnelSpec {
                bind_addr: String::new(),
                bind_port: 8080,
                dest_host: "localhost".into(),
                dest_port: 80
            }
        );
        let b = TunnelSpec::parse_line("127.0.0.1:5432:db.internal:5432").unwrap();
        assert_eq!(
            b,
            TunnelSpec {
                bind_addr: "127.0.0.1".into(),
                bind_port: 5432,
                dest_host: "db.internal".into(),
                dest_port: 5432
            }
        );
    }

    #[test]
    fn tunnel_parse_line_rejects_bad_input() {
        assert!(TunnelSpec::parse_line("8080:localhost").is_err()); // 段数不足
        assert!(TunnelSpec::parse_line("0:localhost:80").is_err()); // 端口 0
        assert!(TunnelSpec::parse_line("70000:localhost:80").is_err()); // 端口越界
        assert!(TunnelSpec::parse_line("8080::80").is_err()); // 目标主机空
        assert!(TunnelSpec::parse_line("8080:localhost:abc").is_err()); // 目标端口非数字
    }

    #[test]
    fn tunnel_to_line_round_trips_and_omits_default_bind() {
        let s = TunnelSpec {
            bind_addr: String::new(),
            bind_port: 8080,
            dest_host: "localhost".into(),
            dest_port: 80,
        };
        assert_eq!(s.to_line(), "8080:localhost:80");
        assert_eq!(TunnelSpec::parse_line(&s.to_line()).unwrap(), s);
        let s2 = TunnelSpec {
            bind_addr: "0.0.0.0".into(),
            bind_port: 9000,
            dest_host: "h".into(),
            dest_port: 9,
        };
        assert_eq!(s2.to_line(), "0.0.0.0:9000:h:9");
    }

    #[test]
    fn parse_tunnel_lines_skips_blank_and_invalid() {
        let text = "8080:localhost:80\n\n  \nGARBAGE\n127.0.0.1:5432:db:5432\n";
        let v = parse_tunnel_lines(text);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].bind_port, 8080);
        assert_eq!(v[1].dest_host, "db");
    }

    #[test]
    fn resolve_jump_finds_other_session_and_guards_self_and_missing() {
        let path = test_path("resolve-jump");
        let mut store = ConfigStore::load_at(path).unwrap();
        let mut bastion = Session::new_empty();
        bastion.id = "bastion".into();
        bastion.host = "jump.example".into();
        store.upsert(bastion);
        let mut target = Session::new_empty();
        target.id = "tgt".into();
        target.jump_session_id = Some("bastion".into());
        store.upsert(target.clone());

        // 正常解析
        assert_eq!(store.resolve_jump(&target).unwrap().host, "jump.example");
        // 自跳 → None
        let mut self_jump = target.clone();
        self_jump.jump_session_id = Some("tgt".into());
        assert!(store.resolve_jump(&self_jump).is_none());
        // 不存在的 id → None
        let mut missing = target.clone();
        missing.jump_session_id = Some("ghost".into());
        assert!(store.resolve_jump(&missing).is_none());
        // 无跳板 → None
        let mut none = target.clone();
        none.jump_session_id = None;
        assert!(store.resolve_jump(&none).is_none());
    }
}
