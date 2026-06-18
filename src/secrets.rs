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

/// 机器绑定的密码加密。密钥 = HKDF-SHA256(机器ID ‖ 内置盐 ‖ 用户名)，
/// 每次启动派生一次并缓存；密文格式 `enc:v1:base64(nonce[24] ‖ ciphertext)`。
#[allow(dead_code)]
mod crypto {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use chacha20poly1305::{
        aead::{Aead, AeadCore, KeyInit, OsRng},
        Key, XChaCha20Poly1305, XNonce,
    };
    use hkdf::Hkdf;
    use sha2::Sha256;
    use std::sync::OnceLock;

    const PREFIX: &str = "enc:v1:";
    const SALT: &[u8; 16] = b"LibSSH/secret/01";

    fn username() -> String {
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "default".into())
    }

    fn derive_key() -> Option<Key> {
        let mid = machine_uid::get().ok()?;
        let ikm = format!("{mid}:{}", username());
        let hk = Hkdf::<Sha256>::new(Some(SALT), ikm.as_bytes());
        let mut okm = [0u8; 32];
        hk.expand(b"LibSSH-secret-store-v1", &mut okm).ok()?;
        Some(*Key::from_slice(&okm))
    }

    fn machine_key() -> Option<&'static Key> {
        static KEY: OnceLock<Option<Key>> = OnceLock::new();
        KEY.get_or_init(derive_key).as_ref()
    }

    fn encrypt_with_key(key: &Key, plain: &str) -> Option<String> {
        let cipher = XChaCha20Poly1305::new(key);
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ct = cipher.encrypt(&nonce, plain.as_bytes()).ok()?;
        let mut buf = nonce.to_vec();
        buf.extend_from_slice(&ct);
        Some(format!("{PREFIX}{}", STANDARD.encode(buf)))
    }

    fn decrypt_with_key(key: &Key, token: &str) -> Option<String> {
        let b64 = token.strip_prefix(PREFIX)?;
        let buf = STANDARD.decode(b64).ok()?;
        if buf.len() < 24 {
            return None;
        }
        let (nonce, ct) = buf.split_at(24);
        let cipher = XChaCha20Poly1305::new(key);
        let pt = cipher.decrypt(XNonce::from_slice(nonce), ct).ok()?;
        String::from_utf8(pt).ok()
    }

    pub fn is_ciphertext(s: &str) -> bool {
        s.starts_with(PREFIX)
    }

    pub fn encrypt(plain: &str) -> Option<String> {
        encrypt_with_key(machine_key()?, plain)
    }

    pub fn decrypt(token: &str) -> Option<String> {
        decrypt_with_key(machine_key()?, token)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn test_key() -> Key {
            *Key::from_slice(&[7u8; 32])
        }

        #[test]
        fn round_trip_recovers_plaintext() {
            let k = test_key();
            let token = encrypt_with_key(&k, "p@ss w0rd 中文").unwrap();
            assert!(token.starts_with(PREFIX));
            assert_eq!(
                decrypt_with_key(&k, &token).as_deref(),
                Some("p@ss w0rd 中文")
            );
        }

        #[test]
        fn wrong_key_fails_to_decrypt() {
            let token = encrypt_with_key(&test_key(), "secret").unwrap();
            let other = *Key::from_slice(&[9u8; 32]);
            assert_eq!(decrypt_with_key(&other, &token), None);
        }

        #[test]
        fn tampered_ciphertext_returns_none() {
            let k = test_key();
            let token = encrypt_with_key(&k, "secret").unwrap();
            let mut bad = token.clone();
            bad.pop();
            bad.push('A');
            assert_eq!(decrypt_with_key(&k, &bad), None);
        }

        #[test]
        fn non_ciphertext_inputs_return_none() {
            let k = test_key();
            assert_eq!(decrypt_with_key(&k, ""), None);
            assert_eq!(decrypt_with_key(&k, "plain-old-password"), None);
            assert!(!is_ciphertext("plain-old-password"));
            assert!(is_ciphertext("enc:v1:abc"));
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
        let moved =
            migrate_plaintext_passwords(&mut sessions, |_, _| panic!("不应为已空密码调用写入"));
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
