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

        let cache = if path.exists() {
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
}
