use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;
use zeroize::Zeroize;

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

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
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
    fn deserialize<D: serde::Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<Self, D::Error> {
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

/// On-disk layout. Keep additive to ease forward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub sessions: Vec<Session>,
    /// Preset SFTP download directory. Empty = ask each time.
    #[serde(default)]
    pub download_dir: String,
    /// UI language code: "zh" (default) or "en".
    #[serde(default)]
    pub language: String,
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
        let dirs = ProjectDirs::from("dev", "meatshell", "meatshell")
            .context("could not determine user config directory")?;
        Ok(dirs.config_dir().join("sessions.json"))
    }

    pub fn sessions(&self) -> &[Session] {
        &self.cache.sessions
    }

    #[allow(dead_code)]
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
        let dir = std::env::temp_dir().join(format!(
            "meatshell-config-test-{}-{name}",
            std::process::id()
        ));
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

        assert!(session.password.is_empty());
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
}
