# App 自管加密密码存储 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用机器绑定密钥加密的本地存储替代 macOS 系统钥匙串，消除每次连接的钥匙串授权弹窗，连接零交互、三平台统一。

**Architecture:** `secrets.rs` 从「keyring 存取」转为「加解密 + 迁移」。会话密码用 `HKDF-SHA256(machine_uid ‖ 内置盐 ‖ 用户名)` 派生的密钥经 `XChaCha20-Poly1305` 加密，密文带 `enc:v1:` 前缀存入 `Session.password`、随 `sessions.json` 落盘。首次启动把旧 keyring / 明文密码自动迁移成密文。

**Tech Stack:** Rust, Slint, `machine-uid`, `chacha20poly1305`(XChaCha20), `hkdf`+`sha2`, `base64`, `zeroize`, `keyring`(仅迁移保留)。

**Spec:** `docs/superpowers/specs/2026-06-18-app-managed-secret-store-design.md`

---

## File Structure

- **Modify** `Cargo.toml` — 新增 `machine-uid` / `chacha20poly1305` / `hkdf` / `base64` 依赖。
- **Modify** `src/secrets.rs` — 新增内联 `mod crypto`（密钥派生 + 加解密）；公开接口改为 `encrypt_password` / `resolve_session_password`（解密语义）/ `migrate_passwords` / `keyring_read` / `keyring_delete`。
- **Modify** `src/app.rs` — 启动迁移接线（`run()` 内）；保存会话按「记住」开关加密写入；删除会话清理旧 keyring 残留。
- **Modify** `ui/session_dialog.slint` — `SessionDraft` 加 `remember`；密码框下加「记住密码」复选框；两处构造体补 `remember`。
- **Modify** `ui/app.slint` — 打开会话对话框时设 `draft-remember` 初值。

`Session` / `Secret` / `AuthMethod` 定义在 `src/config.rs`（`Secret(String)`：`Secret::new(s)` / `as_str()` / `Secret::default()`；`Session.password: Secret`、`Session.auth: AuthMethod`、`AuthMethod::Password`）。本计划不改 `config.rs`。

---

## Task 1: 依赖 + crypto 模块（密钥派生 + 加解密）

**Files:**
- Modify: `Cargo.toml`（依赖区，`keyring` 行附近）
- Modify: `src/secrets.rs`（文件末尾、`#[cfg(test)] mod tests` 之前新增 `mod crypto`）
- Test: `src/secrets.rs`（`mod crypto` 内 `#[cfg(test)]`）

- [ ] **Step 1: 加依赖**

在 `Cargo.toml` 的 `keyring = { ... }` 行下方加入：

```toml
# app 自管密码加密：机器绑定密钥（HKDF-SHA256）+ XChaCha20-Poly1305 AEAD。
machine-uid = "0.5"
chacha20poly1305 = "0.10"
hkdf = "0.12"
base64 = "0.22"
```

- [ ] **Step 2: 写 crypto 模块（含可注入 key 的内部函数，便于测试）**

在 `src/secrets.rs` 末尾、`#[cfg(test)]` 测试模块之前加入：

```rust
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
    // 内置盐：区分「不同机器」。换机后即便机器ID巧合也无法解密。
    const SALT: &[u8; 16] = b"LibSSH/secret/01";

    /// 当前登录用户名（绑定到账户）；取不到时回退固定串（仍受机器ID+盐保护）。
    fn username() -> String {
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "default".into())
    }

    /// 派生 32B 密钥；机器ID 不可得时返回 None（上层据此拒绝持久化，绝不明文落盘）。
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
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng); // 24B
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

    /// `true` 表示该串是本模块产出的密文（用于区分密文 / 旧明文 / 空串）。
    pub fn is_ciphertext(s: &str) -> bool {
        s.starts_with(PREFIX)
    }

    /// 明文 → 密文 token；机器ID不可得或加密失败返回 None。
    pub fn encrypt(plain: &str) -> Option<String> {
        encrypt_with_key(machine_key()?, plain)
    }

    /// 密文 token → 明文；非本机/损坏/非密文一律 None。
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
            assert_eq!(decrypt_with_key(&k, &token).as_deref(), Some("p@ss w0rd 中文"));
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
            bad.push('A'); // 破坏最后一个 base64 字符
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
```

- [ ] **Step 3: 运行测试，确认通过**

Run: `cargo test --bin LibSSH secrets::crypto`
Expected: 4 个测试 PASS（`round_trip_recovers_plaintext`、`wrong_key_fails_to_decrypt`、`tampered_ciphertext_returns_none`、`non_ciphertext_inputs_return_none`）。

- [ ] **Step 4: clippy 干净**

Run: `cargo clippy`
Expected: `Finished`，无 warning。

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/secrets.rs
git commit -m "feat(secrets): add machine-bound XChaCha20 crypto module"
```

---

## Task 2: secrets.rs 公开接口（encrypt_password / resolve 解密 / keyring helpers）

**Files:**
- Modify: `src/secrets.rs:1-78`（替换 keyring CRUD 与文件头注释、`resolve_session_password`）
- Test: `src/secrets.rs`（现有 `#[cfg(test)] mod tests`）

- [ ] **Step 1: 替换文件头注释与公开函数**

把 `src/secrets.rs` 顶部到 `resolve_session_password` 结束（第 1–78 行，即 `migrate_plaintext_passwords` 之外的 keyring 部分；保留 `migrate_plaintext_passwords` 由 Task 3 替换）替换为：

```rust
//! 会话密码的机器绑定加密存储（替代系统钥匙串）。
//!
//! - 密文格式 `enc:v1:…` 直接存在 `Session.password`，随 sessions.json 落盘。
//! - 保存会话时 `encrypt_password` 把明文加密成密文；连接前
//!   `resolve_session_password` 把密文原地解密成明文。
//! - 首次启动 `migrate_passwords` 把旧 keyring / 旧明文搬成密文（见 Task 3）。
//! - 加密失败时绝不明文落盘：宁可不持久化，连接时要求现场输入。

use crate::config::{AuthMethod, Secret, Session};

const SERVICE: &str = "LibSSH";

/// 明文 → `enc:v1:…` 密文；机器ID不可得或加密失败返回 None（上层据此不持久化）。
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

/// 读取旧 keyring 条目（仅迁移用）。条目不存在返回 None。
pub fn keyring_read(session_id: &str) -> Option<String> {
    let entry = keyring::Entry::new(SERVICE, &format!("session:{session_id}")).ok()?;
    match entry.get_password() {
        Ok(p) => Some(p),
        Err(_) => None,
    }
}

/// 删除旧 keyring 条目（迁移后、或删除会话时清理残留）。不存在则忽略。
pub fn keyring_delete(session_id: &str) {
    if let Ok(entry) = keyring::Entry::new(SERVICE, &format!("session:{session_id}")) {
        let _ = entry.delete_credential();
    }
}
```

> 说明：删除了旧的 `entry()` / `store_password` / `load_password` / `delete_password`。下一步修正所有调用点。

- [ ] **Step 2: 修正调用点（编译驱动）**

Run: `cargo build 2>&1 | grep -E 'error|store_password|load_password|delete_password'`
Expected: 报错指向 `src/app.rs`（`delete_password`、`store_password` 调用）。逐个改：

- `src/app.rs` 删除会话处（`crate::secrets::delete_password(id.as_ref());`）→ 改为
  `crate::secrets::keyring_delete(id.as_ref());`
（`store_password` 与迁移调用点分别由 Task 5 / Task 3 处理；本步只确保 Task 2 改动能编译，可暂时保留它们报错，或先注释——推荐直接进 Task 3、Task 5 一并修复后再整体编译。）

- [ ] **Step 3: 更新现有单测**

`src/secrets.rs` 现有 `mod tests` 里依赖 `store_password` / `migrate_plaintext_passwords` 的用例，迁移测试留待 Task 3 重写。本步先删除/标注 `migration_*` 与 `real_keyring_round_trip` 旧用例（它们针对已删除的 keyring 写入路径），保留文件可编译。

- [ ] **Step 4: 运行加解密测试**

Run: `cargo test --bin LibSSH secrets::crypto`
Expected: PASS（Task 1 的 4 个用例不受影响）。

- [ ] **Step 5: Commit**

```bash
git add src/secrets.rs src/app.rs
git commit -m "refactor(secrets): replace keyring CRUD with encrypt/resolve API"
```

---

## Task 3: 自动迁移（旧 keyring / 旧明文 → 密文）+ 启动接线

**Files:**
- Modify: `src/secrets.rs`（新增 `migrate_passwords`，替换旧 `migrate_plaintext_passwords`）
- Modify: `src/app.rs:89-103`（启动迁移接线）
- Test: `src/secrets.rs`（`mod tests`）

- [ ] **Step 1: 写迁移函数**

在 `src/secrets.rs` 加入（替换原 `migrate_plaintext_passwords`）：

```rust
/// 把仍为旧明文或仍存于 keyring 的密码搬成密文，返回迁移条数。幂等：
/// 已是密文 / 空 / 非密码认证一律跳过。`read_keyring` / `delete_keyring`
/// 注入便于单测。加密失败的条目原样保留（不删 keyring、不动明文），下次重试。
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
        // 来源：非空 = 旧明文；空 = 回查 keyring。
        let plain = if !raw.is_empty() {
            Some(raw.to_string())
        } else {
            read_keyring(&s.id)
        };
        let Some(plain) = plain else { continue };
        match crypto::encrypt(&plain) {
            Some(token) => {
                let from_keyring = s.password.as_str().is_empty();
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
```

- [ ] **Step 2: 写迁移单测**

在 `src/secrets.rs` 的 `#[cfg(test)] mod tests` 中加入：

```rust
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
    assert_eq!(crypto::decrypt(sessions[0].password.as_str()).as_deref(), Some("plain-pw"));
}

#[test]
fn migrates_keyring_password_and_deletes_entry() {
    let mut sessions = vec![pwd_session("b", "")]; // 空 → 回查 keyring
    let deleted = std::cell::RefCell::new(Vec::new());
    let moved = migrate_passwords(
        &mut sessions,
        |id| (id == "b").then(|| "from-keyring".to_string()),
        |id| deleted.borrow_mut().push(id.to_string()),
    );
    assert_eq!(moved, 1);
    assert_eq!(crypto::decrypt(sessions[0].password.as_str()).as_deref(), Some("from-keyring"));
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
```

- [ ] **Step 3: 运行测试**

Run: `cargo test --bin LibSSH secrets`
Expected: crypto 4 个 + 迁移 3 个全部 PASS。

- [ ] **Step 4: 启动接线**

把 `src/app.rs` 第 89–103 行的迁移块替换为：

```rust
    // 启动迁移：把旧明文 / 旧 keyring 里的密码搬成机器绑定密文（幂等；
    // keyring 读取在 macOS 上可能一次性弹授权，点「允许」，迁移完即删条目）。
    {
        let mut s = store.borrow_mut();
        let moved = crate::secrets::migrate_passwords(
            s.sessions_mut(),
            crate::secrets::keyring_read,
            crate::secrets::keyring_delete,
        );
        if moved > 0 {
            if let Err(e) = s.save() {
                tracing::warn!("save after password migration failed: {e:#}");
            } else {
                tracing::info!("migrated {moved} session password(s) to encrypted store");
            }
        }
    }
```

- [ ] **Step 5: Commit**

```bash
git add src/secrets.rs src/app.rs
git commit -m "feat(secrets): auto-migrate keyring/plaintext passwords to encrypted store"
```

---

## Task 4: UI —— 「记住密码」复选框

**Files:**
- Modify: `ui/session_dialog.slint:4-13`（struct）、`:27` 附近（属性）、`:154-159`（密码框后加复选框）、`:242-251` 与 `:263-272`（两处构造体）
- Modify: `ui/app.slint`（打开对话框处设 `draft-remember` 初值）

- [ ] **Step 1: SessionDraft 加字段 + dialog 加属性**

`ui/session_dialog.slint` struct（第 4-13 行）末尾加 `remember`：

```slint
export struct SessionDraft {
    id: string,
    name: string,
    host: string,
    port: int,
    user: string,
    auth: string,      // "password" | "key"
    password: string,
    private-key-path: string,
    remember: bool,
}
```

在 `in-out property <string> draft-password;`（第 27 行）下方加：

```slint
    in-out property <bool> draft-remember: true;
```

- [ ] **Step 2: 密码框下加复选框**

把第 154-159 行的密码 `LabeledInput` 块替换为它 + 一个自绘复选框（与项目自绘控件风格一致）：

```slint
            if root.draft-auth == "password" : LabeledInput {
                label: @tr("Password");
                placeholder: root.is-editing ? @tr("Leave blank to keep the current password") : "••••••••";
                password: true;
                value <=> root.draft-password;
            }

            if root.draft-auth == "password" : HorizontalLayout {
                spacing: 8px;
                Rectangle {
                    width: 18px; height: 18px;
                    border-radius: Theme.radius-sm;
                    border-width: 1px;
                    border-color: root.draft-remember ? Theme.accent : Theme.border-subtle;
                    background: root.draft-remember ? Theme.accent : Theme.bg-root;
                    Text {
                        text: root.draft-remember ? "✓" : "";
                        color: white;
                        font-size: Theme.fs-sm;
                        horizontal-alignment: center;
                        vertical-alignment: center;
                    }
                    TouchArea {
                        mouse-cursor: pointer;
                        clicked => { root.draft-remember = !root.draft-remember; }
                    }
                }
                Text {
                    text: @tr("Remember password (encrypted on this machine)");
                    color: Theme.text-secondary;
                    font-size: Theme.fs-sm;
                    vertical-alignment: center;
                }
            }
```

- [ ] **Step 3: 两处构造体补 remember**

第 242-251 行 `test-connection({...})` 与第 263-272 行 `submit({...})` 两处 struct 字面量，各在 `private-key-path: root.draft-key-path,` 后加一行：

```slint
                                remember: root.draft-remember,
```

（test-connection 处该值不参与逻辑，仅满足结构完整。）

- [ ] **Step 4: app.slint 设初值**

在 `ui/app.slint` 中找到打开会话对话框（新建 / 编辑）设置 `draft-*` 的回调处，补设 `draft-remember`：新建时 `true`；编辑时按「是否已存有密码」决定（已存 → `true`）。定位命令：

Run: `rg -n 'set_draft_password|draft-password|session-dialog-open|open.*dialog' ui/app.slint src/app.rs`

在设 `draft-password` 的同一处，新建分支设 `draft-remember = true`；编辑分支设 `draft-remember = <该会话已存密码?>`（无现成标志时默认 `true`）。

- [ ] **Step 5: 编译验证（Slint 语法 + 生成的 Rust 类型）**

Run: `cargo build`
Expected: 编译通过（`SessionDraft` 新增字段后，Rust 侧 `on_session_dialog_submit` 闭包参数自动带 `remember`，Task 5 使用）。

- [ ] **Step 6: Commit**

```bash
git add ui/session_dialog.slint ui/app.slint
git commit -m "feat(ui): add 'remember password' checkbox to session dialog"
```

---

## Task 5: 保存会话——按「记住」开关加密写入

**Files:**
- Modify: `src/app.rs:858-901`（`on_session_dialog_submit`）

- [ ] **Step 1: 改写保存逻辑**

把 `src/app.rs` 第 893-901 行（`// 新输入的密码优先进系统凭据库…` 注释及其后的 `if !draft.password.is_empty() && store_password(...) {...}` 块）替换为：

```rust
        // 「记住」开关决定是否持久化：
        // - 不记住 → 清空，连接时现场输入；
        // - 记住 + 新输入明文 → 加密成 enc:v1: 密文存入 password；
        //   加密不可用时不持久化（绝不明文落盘）；
        // - 记住 + 未改密码（draft 为空）→ 沿用上面取出的旧密文，不动。
        if !draft.remember {
            new_session.password = Secret::default();
        } else if !draft.password.is_empty() {
            new_session.password = crate::secrets::encrypt_password(new_session.password.as_str())
                .map(Secret::new)
                .unwrap_or_default();
        }
```

- [ ] **Step 2: 全量编译 + clippy**

Run: `cargo clippy --all-targets`
Expected: `Finished`，无 error、无 warning。

- [ ] **Step 3: 全量测试**

Run: `cargo test --bin LibSSH`
Expected: 全绿（含 secrets::crypto 4 + 迁移 3）。

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "feat(app): encrypt session password on save, gated by remember toggle"
```

---

## Task 6: 收尾——清理与文档

**Files:**
- Modify: `Cargo.toml`（`keyring` 注释说明仅迁移保留）
- Modify: `README.md` / `AGENTS.md`（如有密码存储相关描述则更新）

- [ ] **Step 1: 标注 keyring 仅迁移保留**

把 `Cargo.toml` 的 `keyring` 行上方注释改为：

```toml
# keyring 现仅用于「自动迁移旧版钥匙串密码」（读取 + 删除旧条目）。
# 新写入走机器绑定加密存储；迁移在用户群普及后的未来版本可移除此依赖。
keyring = { version = "3", features = ["apple-native", "windows-native", "sync-secret-service"] }
```

- [ ] **Step 2: 文档同步（若存在相关段落）**

Run: `rg -n -i 'keyring|钥匙串|keychain|密码.*存储|凭据库' README.md AGENTS.md docs/*.md`
对命中的「密码存入系统钥匙串」类描述，改为「密码经机器绑定加密存于本地配置」。无命中则跳过。

- [ ] **Step 3: 手动验收**

```
1. 新建/编辑会话，输入密码，勾「记住密码」→ 保存。
2. 检查 ~/Library/Application Support/dev.LibSSH.LibSSH/sessions.json：
   该会话 password 字段为 "enc:v1:..." 密文（非明文、非空）。
3. 连接该会话 → 无钥匙串弹窗、直接登录。
4. 重启 app → 再连接 → 仍无弹窗、自动登录。
5. 取消勾选「记住密码」保存 → password 为空 → 连接时要求现输。
6. 把 sessions.json 拷到另一台机器 → 解密失败 → 提示现输（不崩溃）。
7. 迁移验证：用旧版存过密码（keyring 里有），升级后首次启动 →
   点几次「允许」→ sessions.json 变密文、keyring 条目被删 → 此后零弹窗。
```

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml README.md AGENTS.md
git commit -m "docs: note keyring retained only for password migration"
```

---

## Self-Review（已核对）

- **Spec 覆盖**：安全模型/密钥派生→Task 1；组件 B 接口→Task 2；组件 C 迁移→Task 3；组件 D 复选框→Task 4；保存加密→Task 5；依赖/错误处理「加密失败不落盘」→Task 1(`encrypt` 返回 Option)+Task 5(`unwrap_or_default`)；keyring 分两步→Task 6。
- **类型一致**：`encrypt_password`/`resolve_session_password`/`migrate_passwords`/`keyring_read`/`keyring_delete`/`crypto::{encrypt,decrypt,is_ciphertext}` 跨任务命名一致；`Secret::new`/`as_str`/`default`、`AuthMethod::Password`、`Session.{id,auth,password}` 与 `config.rs` 一致。
- **占位**：无 TBD；唯一需现场定位的是 Task 4 Step 4（`ui/app.slint` 打开对话框处），已给定位命令与取值规则。
