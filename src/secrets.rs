//! 会话密码的机器绑定加密存储（替代系统钥匙串）。
//!
//! - 密文格式 `enc:v1:…` 直接存在 `Session.password`，随 sessions.json 落盘。
//! - 保存会话时 `encrypt_password` 把明文加密成密文；连接前
//!   `resolve_session_password` 把密文原地解密成明文。
//! - 首次启动 `migrate_passwords` 把旧 keyring / 旧明文搬成密文，成功即删旧
//!   keyring 条目；失败保留原状下次重试。幂等、自愈。
//! - 加密失败时绝不明文落盘：宁可不持久化，连接时要求现场输入。

use crate::config::{AuthMethod, Secret, Session};

const SERVICE: &str = "LibSSH";

/// 明文 → `enc:v1:…` 密文；机器 ID 不可得或加密失败返回 None（上层据此不持久化）。
pub fn encrypt_password(plain: &str) -> Option<String> {
    crypto::encrypt(plain)
}

/// 连接前解析密码：`password` 为密文则原地解密成明文（仅密码认证需要）。
/// 空串、旧明文残留、解密失败都保持原样。
pub fn resolve_session_password(session: &mut Session) {
    if session.auth == AuthMethod::Password && crypto::is_ciphertext(session.password.as_str()) {
        if let Some(plain) = crypto::decrypt(session.password.as_str()) {
            session.password = Secret::new(plain);
        }
    }
}

/// 读取旧 keyring 条目（仅迁移用）。不存在返回 None。
pub fn keyring_read(session_id: &str) -> Option<String> {
    let entry = keyring::Entry::new(SERVICE, &format!("session:{session_id}")).ok()?;
    entry.get_password().ok()
}

/// 删除旧 keyring 条目（迁移后、或删除会话时清理残留）。不存在则忽略。
pub fn keyring_delete(session_id: &str) {
    if let Ok(entry) = keyring::Entry::new(SERVICE, &format!("session:{session_id}")) {
        let _ = entry.delete_credential();
    }
}

/// 把旧明文 / 旧 keyring 密码搬成密文，返回迁移条数。幂等：已是密文 / 空且
/// keyring 无值 / 非密码认证一律跳过。`read_keyring` / `delete_keyring` 注入
/// 便于单测。加密失败的条目原样保留（不删 keyring、不动明文），下次重试。
pub fn migrate_passwords(
    sessions: &mut [Session],
    read_keyring: impl Fn(&str) -> Option<String>,
    delete_keyring: impl Fn(&str),
) -> usize {
    let mut moved = 0;
    for s in sessions.iter_mut() {
        if s.auth != AuthMethod::Password {
            continue;
        }
        let raw = s.password.as_str();
        if crypto::is_ciphertext(raw) {
            continue; // 已迁移
        }
        // 非空 = 旧明文；空 = 回查 keyring。
        let from_keyring = raw.is_empty();
        let plain = if from_keyring {
            read_keyring(&s.id)
        } else {
            Some(raw.to_string())
        };
        let Some(plain) = plain else { continue };
        match crypto::encrypt(&plain) {
            Some(token) => {
                s.password = Secret::new(token);
                if from_keyring {
                    delete_keyring(&s.id);
                }
                moved += 1;
            }
            None => continue, // 加密不可用：保留原状，下次启动重试
        }
    }
    moved
}

/// 机器绑定的密码加密。密钥 = HKDF-SHA256(机器ID ‖ 内置盐 ‖ 用户名)，
/// 每次启动派生一次并缓存；密文格式 `enc:v1:base64(nonce[24] ‖ ciphertext)`。
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

    fn pwd_session(id: &str, pwd: &str) -> Session {
        let mut s = Session::new_empty();
        s.id = id.into();
        s.auth = AuthMethod::Password;
        s.password = Secret::new(pwd);
        s
    }

    #[test]
    fn migrates_plaintext_to_ciphertext() {
        let mut sessions = vec![pwd_session("a", "plain-pw")];
        let moved = migrate_passwords(&mut sessions, |_| None, |_| {});
        assert_eq!(moved, 1);
        assert!(crypto::is_ciphertext(sessions[0].password.as_str()));
        assert_eq!(
            crypto::decrypt(sessions[0].password.as_str()).as_deref(),
            Some("plain-pw")
        );
    }

    #[test]
    fn migrates_keyring_password_and_deletes_entry() {
        let mut sessions = vec![pwd_session("b", "")];
        let deleted = std::cell::RefCell::new(Vec::new());
        let moved = migrate_passwords(
            &mut sessions,
            |id| (id == "b").then(|| "from-keyring".to_string()),
            |id| deleted.borrow_mut().push(id.to_string()),
        );
        assert_eq!(moved, 1);
        assert_eq!(
            crypto::decrypt(sessions[0].password.as_str()).as_deref(),
            Some("from-keyring")
        );
        assert_eq!(*deleted.borrow(), vec!["b".to_string()]);
    }

    #[test]
    fn migration_is_idempotent_on_ciphertext() {
        let token = crypto::encrypt("x").unwrap();
        let mut sessions = vec![pwd_session("c", &token)];
        let moved = migrate_passwords(&mut sessions, |_| panic!("不应回查 keyring"), |_| {});
        assert_eq!(moved, 0);
        assert_eq!(sessions[0].password.as_str(), token);
    }
}
