# 设计：App 自管加密密码存储（替代系统钥匙串）

日期：2026-06-18
状态：需求与方案已逐项确认；本文档待用户审阅后转实施计划。

## 背景与根因

macOS 上每次连接服务器都弹「LibSSH 想访问钥匙串中的密钥 "LibSSH"」。根因：会话密码存入系统钥匙串（`src/secrets.rs`，`keyring` crate），而钥匙串条目的 ACL（trusted-app + partition list）由早期 **adhoc 签名**的二进制创建，与当前稳定签名脱节；`set_password` 更新已存在条目时不重置 ACL，「始终允许」也只改 trusted-app 一侧、补不回 partition list，于是每次连接照弹。

有两条根治路线：

- **路线 A**：修复钥匙串 ACL（删除重建条目，由稳定签名 app 重新创建）。不改架构、安全性最高，但依赖系统钥匙串机制，且自动更新装的 adhoc 包会让它复发。
- **路线 B（本设计）**：放弃系统钥匙串，改由 app 自己用**机器绑定密钥**加密存储密码——像 `known_hosts` 那样完全由 app 掌控，从根上不再触发任何系统弹窗。

用户选择路线 B，并确认了下述全部取舍。

## 目标与非目标

**目标**：一次性无感设置后，连接服务器时零系统弹窗、零解锁交互；三平台行为统一；现有钥匙串里的密码自动迁移过来。

**非目标**：

- 不抵御「本机上的其他程序/恶意软件」——机器绑定密钥可由本机特征重算，这是用体验换安全的**既定取舍**。
- 不引入主密码 / 启动口令（零交互优先）。
- 不做独立密钥文件、不做居中浮层提示（见 YAGNI）。

## 安全模型与边界

- **密钥来源**：`HKDF-SHA256(ikm = machine_uid::get(), salt = 编译期内置 32B 常量, info = "LibSSH-secret-store-v1" + 当前用户名)` → 32 字节密钥。
  - `machine_uid::get()`：macOS = IOPlatformUUID（`ioreg`）、Linux = `/etc/machine-id`、Windows = 注册表 `MachineGuid`。无需 root。
  - 内置盐区分「换一台机器」；当前用户名把密钥绑定到当前账户。
  - 密钥用 `OnceLock` 缓存，每次启动只派生一次（避免重复 `ioreg` 子进程开销）。
- **加密**：`XChaCha20-Poly1305`（AEAD，24B 随机 nonce，碰撞概率可忽略）。
- **能挡住**：`sessions.json` 被拷到另一台机器 / 另一个用户账户 → 解密失败。
- **挡不住**：本机同账户下的其他进程（可读取 `machine_uid` 与算法、重算密钥）。

## 组件 A：加密原语（`secrets.rs` 内新增 `crypto` 子模块）

- `machine_key() -> &'static Key`：按上述 HKDF 派生并 `OnceLock` 缓存。
- `encrypt(plain: &str) -> Result<String>`：随机 24B nonce → 加密 → 输出 `"enc:v1:" + base64(nonce ‖ ciphertext)`。
- `decrypt(token: &str) -> Option<String>`：校验 `enc:v1:` 前缀 → 解析 → 解密；前缀不符 / base64 损坏 / 认证失败 / 换机换用户一律返回 `None`，**绝不 panic**。
- 版本前缀 `enc:v1:` 为日后算法升级预留。

## 组件 B：存储与 `secrets.rs` 接口

- `Session.password` 字段（迁移后本为空串）改存密文 `enc:v1:…`，随 `sessions.json` 落盘。
- `secrets.rs` 的角色从「keyring 存取」转为「加解密 + 迁移」：
  - `encrypt_password(plain) -> Option<String>`：明文 → `enc:v1:…` 密文；失败返回 `None`。
  - `resolve_session_password(&mut Session)`（**签名不变**）：`password` 为 `enc:v1:` 密文则原地解密成明文供连接；否则（空 / 旧明文残留）不动。
  - `keyring_read(id)` / `keyring_delete(id)`：仅供组件 C 迁移、以及删除会话时清理可能的旧 keyring 残留。
  - 移除 keyring 时代的 `store_password` / `load_password`（密文改由保存会话时 `encrypt_password` 写入 `Session.password`）。
- 调用点改动小：保存会话（`app.rs` `on_session_dialog_submit`）改为按「记住」开关 `encrypt_password` 后写入 `Session.password`；连接侧（`cli.rs` / `app.rs`）仅依赖 `resolve_session_password`，零改动。

## 组件 C：自动迁移（首次启动，幂等）

启动时对每个会话：

1. **旧 keyring 密码**：从系统钥匙串读出（macOS 上每条可能弹一次授权，点「允许」）→ `encrypt` 写回 `sessions.json` → 删除 keyring 条目。迁移完该条目永不再被访问。
2. **更老的 json 明文**（历史遗留）：直接 `encrypt` 写回，覆盖明文。

任一条迁移失败：保留原状（keyring 条目不删 / 明文不动），记 `warn`，下次启动重试。幂等、自愈、无中间态。

## 组件 D：UI —— 「记住密码」复选框

- `ui/session_dialog.slint`：密码输入框下方新增「☑ 记住密码（本机加密）」复选框，默认勾选；draft 增加 `remember: bool`。
- `app.rs` 保存会话逻辑（现 `app.rs:896` 一带）：仅当 `remember` 勾选且密码非空时调 `store_password`；不勾选则不持久化、`password` 留空，每次连接现场输入。

## 数据流

- **存**：填密码 + ☑记住 → `store_password` → `encrypt` → 密文写入 `password` 字段 → 落盘。
- **读**：连接前 `resolve_session_password` 见 `enc:v1:` → `decrypt` → 明文连接（`zeroize` 护内存）。零弹窗、零交互。
- **迁移**：见组件 C。

## 错误处理

- `machine_uid::get()` 失败（极罕见）：`encrypt` 返回 `Err` → `store_password` 失败 → 调用方**不持久化**（`password` 留空、连接时现输），**绝不回退明文落盘**（这是相对旧版「失败回退明文」的安全收紧）。
- `decrypt` 失败（密文损坏 / 换主板 / 文件来自他机他户）：视为「无保存密码」→ 提示重新输入，不崩溃、不弹错误框，记 `warn`。
- 机器 ID 变化：等价于全部密文失效 → 优雅降级为「需重新输入」。

## 依赖变更

- 新增：`machine-uid`、`chacha20poly1305`（含 `rand_core` feature）、`hkdf`。已有 `rand` / `zeroize` / `sha2` 复用。
- `keyring`：**保留**，但仅用于组件 C 的迁移路径（读旧条目 + 删除）；新写入只走加密文件。待迁移在用户群中普及后的未来版本再彻底移除并下线三个 native feature。

## 测试

- 单元：`encrypt`/`decrypt` round-trip；篡改密文返回 `None`；错误前缀返回 `None`；机器 ID 以注入方式 mock，验证「换 ID 即解不开」。
- 迁移：keyring→加密文件 幂等；迁移失败保留 keyring 条目（复用现有 `migrate_plaintext_passwords` 测试框架的注入写闭包思路）。
- 手动验收：填密码勾「记住」→ 连接零弹窗 → 重启 app 仍零弹窗 → 把 `sessions.json` 拷到另一台机器 → 解密失败、提示重输。

## 不做的事（YAGNI）

主密码 / 启动口令、独立密钥文件、居中浮层「保存成功」提示、密钥轮换、Windows/Linux 的差异化加固（三平台统一处理）。
