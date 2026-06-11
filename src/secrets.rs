//! 系统凭据库里的会话密码。「无标志位」设计：
//!
//! - 写 → 先试 keyring，成功则 sessions.json 落空串；失败回退明文（旧行为）。
//! - 读 → `Session.password` 为空才回查 keyring（查不到就当真没有）。
//! - 迁移 → 启动时把 json 里的明文逐条搬进 keyring，成功一条清一条，
//!   失败立即停（剩余明文保留，下次启动重试）。幂等、自愈、无状态机：
//!   即使迁移中途断电，也不存在「密码已清空但没存进 keyring」的窗口。

use anyhow::{Context, Result};

use crate::config::{AuthMethod, Secret, Session};

const SERVICE: &str = "LibSSH";

fn entry(session_id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, &format!("session:{session_id}")).context("open keyring entry")
}

pub fn store_password(session_id: &str, password: &str) -> Result<()> {
    entry(session_id)?
        .set_password(password)
        .context("keyring set_password")
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

/// 把仍以明文存放的密码搬进凭据库，返回迁移条数。
/// `write` 注入便于单测；第一次失败立即停止并保留剩余明文。
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
                s.password = Secret::default();
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

/// 连接前解析会话密码：json 明文优先（迁移失败的回退场景），
/// 否则回查 keyring。仅密码认证需要。
pub fn resolve_session_password(session: &mut Session) {
    if session.auth == AuthMethod::Password && session.password.as_str().is_empty() {
        if let Some(p) = load_password(&session.id) {
            session.password = Secret::new(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with_pwd(id: &str, pwd: &str) -> Session {
        let mut s = Session::new_empty();
        s.id = id.into();
        s.password = Secret::new(pwd);
        s
    }

    #[test]
    fn migration_moves_plaintext_and_clears_json_copies() {
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
        assert_eq!(
            stored,
            vec![("a".into(), "pa".into()), ("c".into(), "pc".into())]
        );
        assert!(sessions.iter().all(|s| s.password.as_str().is_empty()));
    }

    #[test]
    fn migration_failure_keeps_remaining_plaintext_untouched() {
        // 写入失败：明文必须原样保留 —— 绝不能清掉没存进凭据库的密码。
        let mut sessions = vec![session_with_pwd("x", "px"), session_with_pwd("y", "py")];
        let mut calls = 0;
        let moved = migrate_plaintext_passwords(&mut sessions, |_, _| {
            calls += 1;
            anyhow::bail!("no backend")
        });
        assert_eq!(moved, 0);
        assert_eq!(calls, 1, "首败即停，不再骚扰后续条目");
        assert_eq!(sessions[0].password.as_str(), "px");
        assert_eq!(sessions[1].password.as_str(), "py");
    }

    #[test]
    fn migration_is_idempotent_on_already_empty_passwords() {
        let mut sessions = vec![session_with_pwd("a", "")];
        let moved = migrate_plaintext_passwords(&mut sessions, |_, _| {
            panic!("不应为已空密码调用写入")
        });
        assert_eq!(moved, 0);
    }

    /// 触碰真实系统凭据库的冒烟测试：`cargo test real_keyring -- --ignored`。
    /// macOS 上首次运行可能弹钥匙串授权框，CI / 常规 cargo test 不跑。
    #[test]
    #[ignore = "touches the real OS keychain; run manually"]
    fn real_keyring_round_trip() {
        let id = format!("smoke-{}", std::process::id());
        store_password(&id, "p@ss").unwrap();
        assert_eq!(load_password(&id).as_deref(), Some("p@ss"));
        delete_password(&id);
        assert_eq!(load_password(&id), None);
    }
}
