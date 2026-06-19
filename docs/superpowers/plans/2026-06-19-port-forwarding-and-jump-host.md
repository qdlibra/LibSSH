# 端口转发（-L）+ 跳板机 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 LibSSH 加上本地端口转发（SSH `-L`）和经由已保存会话的单跳跳板机（ProxyJump），会话可配置、连接时自动建立。

**Architecture:** 跳板机复用现有 `client::connect_stream`——连接并认证「跳板会话」后，用 `handle.channel_open_direct_tcpip()` 在跳板上开一条到目标的 direct-tcpip 通道，把它的 `into_stream()` 当作传输流喂给 `connect_stream` 连目标，与现有「经代理连接」完全同形。本地转发在 `run_session` 认证成功后建立：因 russh 的 `Handle` **非 Clone、非 Sync（`Arc<Handle>` 不是 Send，不能跨 `tokio::spawn`）**，采用「run_session 独占 handle 串行开 direct-tcpip 通道；acceptor 子任务只把接受到的 `TcpStream` 经 mpsc 回传；pump 子任务只持有 `Send` 的 `ChannelStream`」的结构。跳板凭据不新增存储——用 `jump_session_id` 引用另一个已保存会话，复用整套凭据加密/解密与会话对话框。

**Tech Stack:** Rust 2021、russh 0.49.2（`channel_open_direct_tcpip` / `Channel::into_stream` / `connect_stream`）、tokio（`TcpListener` / `copy_bidirectional` / mpsc）、Slint、serde。

**一期范围（本计划）：** 本地转发 `-L` + 单跳跳板机 + 会话对话框最小配置 UI。
**明确不做（后续计划）：** 远程转发 `-R`、动态转发 `-D`（本地 SOCKS）、多级跳板链、可视化隧道行编辑器（一期隧道用文本行配置）、CLI `run_exec` 路径接入跳板。

**测试取向：** 纯逻辑（配置往返、隧道行解析/格式化、跳板会话解析）走单元测试，TDD 红绿重构；真正跑 SSH I/O 的转发/跳板建立属集成代码，按既有惯例（ssh.rs 仅 `test_connection_fails_fast_on_closed_port` 一处触网）以**手动验证步骤** + `cargo build` 编译门来保证，不假装能纯单测。

---

## 文件结构

| 文件 | 职责 | 改动 |
|---|---|---|
| `src/config.rs` | 持久化模型 | 新增 `TunnelSpec` 类型 + 解析/格式化；`Session` 加 `tunnels` / `jump_session_id`；`ConfigStore::resolve_jump` |
| `src/ssh.rs` | 会话 worker | `SessionEvent::TunnelStatus`；`connect_transport` / `connect_and_auth` 连接助手（含跳板）；`spawn_session` / `run_session` 增 `jump` 参；本地转发建立 + `pump_forward` |
| `src/app.rs` | UI 接线 | `start_session_io` 增 `jump` 参；各调用点解析跳板会话；对话框 submit/test 读写隧道与跳板字段 |
| `ui/session_dialog.slint` | 会话对话框 | `SessionDraft` 加 `jump-session-id` / `tunnels-text`；跳板下拉 + 隧道多行输入 |
| `lang/zh,en/LC_MESSAGES/LibSSH.po` | 文案 | 新增隧道/跳板相关 `@tr` 词条（Slint 提取） |

---

## Task 1: 配置模型——TunnelSpec 与 Session 新字段

**Files:**
- Modify: `src/config.rs`（`Session` 结构体 ~178-199、`Session::new_empty` ~228-244）
- Test: `src/config.rs`（`#[cfg(test)] mod tests`）

- [ ] **Step 1: 写失败测试**——在 `src/config.rs` 的 `mod tests` 内新增。复用既有 `test_path` / `ConfigStore::load_at` 模式。

```rust
    #[test]
    fn session_round_trips_tunnels_and_jump() {
        let path = test_path("tunnels-jump");
        let mut store = ConfigStore::load_at(path.clone()).unwrap();

        let mut s = Session::new_empty();
        s.id = "tgt".into();
        s.host = "10.0.0.9".into();
        s.tunnels = vec![
            TunnelSpec { bind_addr: String::new(), bind_port: 8080, dest_host: "localhost".into(), dest_port: 80 },
            TunnelSpec { bind_addr: "127.0.0.1".into(), bind_port: 5432, dest_host: "db.internal".into(), dest_port: 5432 },
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib config::tests::session_round_trips_tunnels_and_jump`
Expected: 编译失败——`TunnelSpec` 未定义、`Session` 无 `tunnels` / `jump_session_id` 字段。

- [ ] **Step 3: 最小实现**——在 `src/config.rs` 加 `TunnelSpec`（紧挨 `QuickCommand` 定义之后即可），并给 `Session` 增字段。

`TunnelSpec` 定义：

```rust
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
```

`Session` 结构体在 `group` 字段后追加：

```rust
    /// 本地端口转发（-L）规格列表。
    #[serde(default)]
    pub tunnels: Vec<TunnelSpec>,
    /// 单跳跳板机：经由另一个已保存会话（其 id）建立到本会话的连接。None = 直连。
    #[serde(default)]
    pub jump_session_id: Option<String>,
```

`Session::new_empty()` 在 `group: String::new(),` 后追加：

```rust
            tunnels: Vec::new(),
            jump_session_id: None,
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib 'config::tests::session_round_trips_tunnels_and_jump' 'config::tests::legacy_session_without_tunnel_fields_defaults_empty'`
Expected: PASS（两个都过）。

- [ ] **Step 5: 提交**

```bash
git add src/config.rs
git commit -m "feat(config): 会话增加端口转发(-L)与跳板会话引用字段"
```

---

## Task 2: 隧道行解析与格式化

UI 一期用「每行一条」的文本框配置隧道，格式 `[bind_addr:]bind_port:dest_host:dest_port`。本任务实现并彻底单测解析/格式化纯函数。

**Files:**
- Modify: `src/config.rs`（`impl TunnelSpec` + 自由函数 `parse_tunnel_lines`）
- Test: `src/config.rs`（`mod tests`）

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn tunnel_parse_line_three_and_four_parts() {
        let a = TunnelSpec::parse_line("8080:localhost:80").unwrap();
        assert_eq!(a, TunnelSpec { bind_addr: String::new(), bind_port: 8080, dest_host: "localhost".into(), dest_port: 80 });
        let b = TunnelSpec::parse_line("127.0.0.1:5432:db.internal:5432").unwrap();
        assert_eq!(b, TunnelSpec { bind_addr: "127.0.0.1".into(), bind_port: 5432, dest_host: "db.internal".into(), dest_port: 5432 });
    }

    #[test]
    fn tunnel_parse_line_rejects_bad_input() {
        assert!(TunnelSpec::parse_line("8080:localhost").is_err());        // 段数不足
        assert!(TunnelSpec::parse_line("0:localhost:80").is_err());        // 端口 0
        assert!(TunnelSpec::parse_line("70000:localhost:80").is_err());    // 端口越界
        assert!(TunnelSpec::parse_line("8080::80").is_err());              // 目标主机空
        assert!(TunnelSpec::parse_line("8080:localhost:abc").is_err());    // 目标端口非数字
    }

    #[test]
    fn tunnel_to_line_round_trips_and_omits_default_bind() {
        let s = TunnelSpec { bind_addr: String::new(), bind_port: 8080, dest_host: "localhost".into(), dest_port: 80 };
        assert_eq!(s.to_line(), "8080:localhost:80");
        assert_eq!(TunnelSpec::parse_line(&s.to_line()).unwrap(), s);
        let s2 = TunnelSpec { bind_addr: "0.0.0.0".into(), bind_port: 9000, dest_host: "h".into(), dest_port: 9 };
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib config::tests::tunnel_`
Expected: 编译失败——`parse_line` / `to_line` / `parse_tunnel_lines` 未定义。

- [ ] **Step 3: 最小实现**——在 `src/config.rs` 加（紧接 `TunnelSpec` 定义之后）。

```rust
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
pub fn parse_tunnel_lines(text: &str) -> Vec<TunnelSpec> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| TunnelSpec::parse_line(l).ok())
        .collect()
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib config::tests::tunnel_ config::tests::parse_tunnel_lines`
Expected: PASS（4 个测试）。

- [ ] **Step 5: 提交**

```bash
git add src/config.rs
git commit -m "feat(config): 隧道行解析与格式化(parse_line/to_line/parse_tunnel_lines)"
```

---

## Task 3: 跳板会话解析 ConfigStore::resolve_jump

纯查找逻辑（不解密），可单测：按 `jump_session_id` 找到另一个已存会话并克隆；自跳/空 id/不存在 → None。

**Files:**
- Modify: `src/config.rs`（`impl ConfigStore`）
- Test: `src/config.rs`（`mod tests`）

- [ ] **Step 1: 写失败测试**

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib config::tests::resolve_jump_finds_other_session_and_guards_self_and_missing`
Expected: 编译失败——`resolve_jump` 未定义。

- [ ] **Step 3: 最小实现**——在 `impl ConfigStore`（`get` 方法附近）加：

```rust
    /// 解析跳板会话：按 `jump_session_id` 查另一个已保存会话并克隆返回（不解密密码）。
    /// 自跳 / 空 id / 不存在 → None。调用方负责对返回值做 `resolve_session_password`。
    pub fn resolve_jump(&self, session: &Session) -> Option<Session> {
        let id = session.jump_session_id.as_deref()?;
        if id.is_empty() || id == session.id {
            return None;
        }
        self.get(id).cloned()
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib config::tests::resolve_jump_finds_other_session_and_guards_self_and_missing`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add src/config.rs
git commit -m "feat(config): ConfigStore::resolve_jump 按 id 解析跳板会话"
```

---

## Task 4: ssh.rs 连接助手重构 + spawn/run_session 增 jump 参（不改行为）

把 `run_session` 里「代理或直连」的连接块抽成 `connect_transport`（一期不含跳板逻辑，先保持现有行为），并给 `spawn_session` / `run_session` 加上 `jump: Option<Session>` 参数（暂不使用、调用点传 `None`）。目的：纯重构，所有现有测试保持绿。

**Files:**
- Modify: `src/ssh.rs`（`spawn_session` ~413、`run_session` ~448-499）
- Modify: `src/app.rs`（`spawn_session` 唯一调用点 ~2783）

- [ ] **Step 1: 加 `connect_transport` 助手**——在 `run_session` 之前插入。它把目标的 pre-auth `Handle` 连出来（认证仍在 `run_session` 内做），返回值第二项预留给跳板 keepalive handle（本任务恒为 `None`）。

```rust
/// 建立到目标的**未认证**传输 Handle。第二项为需在整个会话期间保活的跳板 Handle
/// （直连/代理时为 None；跳板逻辑在 Task 6 接入）。
async fn connect_transport(
    session: &Session,
    _jump: &Option<Session>,
    config: Arc<client::Config>,
    events: &UnboundedSender<SessionEvent>,
) -> Result<(Handle<ClientHandler>, Option<Handle<ClientHandler>>)> {
    let handler = ClientHandler {
        host: session.host.clone(),
        port: session.port,
    };
    let addr = format!("{}:{}", session.host, session.port);
    let handle = match crate::proxy::resolve(&session.proxy) {
        Some(proxy) => {
            let _ = events.send(SessionEvent::Status(format!(
                "{} {} -> {}",
                t("经代理连接", "via proxy"),
                crate::proxy::describe(&proxy),
                addr
            )));
            let stream = crate::proxy::connect(&proxy, &session.host, session.port)
                .await
                .with_context(|| format!("proxy connect to {} failed", addr))?;
            client::connect_stream(config, stream, handler)
                .await
                .with_context(|| format!("connect {} failed", addr))?
        }
        None => client::connect(config, addr.as_str(), handler)
            .await
            .with_context(|| format!("connect {} failed", addr))?,
    };
    Ok((handle, None))
}
```

- [ ] **Step 2: `run_session` 改用助手**——把现有 `let mut handle = match crate::proxy::resolve(...) { ... };`（约 481-499 行整块）替换为：

```rust
    let (mut handle, _jump_keepalive) = connect_transport(&session, &jump, config, &events).await?;
```

> 注意：`config` 变量在原代码里是 `Arc<client::Config>`，`connect_transport` 取得其所有权——后续 `run_session` 不再单独用 `config`，符合现状。

- [ ] **Step 3: 给 `run_session` / `spawn_session` 加 `jump` 参**

`run_session` 签名加参数（放在 `session` 之后）：

```rust
async fn run_session(
    session: Session,
    jump: Option<Session>,
    mut commands: UnboundedReceiver<SessionCommand>,
    events: UnboundedSender<SessionEvent>,
    initial_cols: u32,
    initial_rows: u32,
) -> Result<()> {
```

`spawn_session` 签名加参数 + 透传：

```rust
pub fn spawn_session(
    runtime: &tokio::runtime::Handle,
    tab_id: String,
    session: Session,
    jump: Option<Session>,
    initial_cols: u32,
    initial_rows: u32,
) -> (SessionHandle, UnboundedReceiver<SessionEvent>) {
```

在 `spawn_session` 内的 `run_session(` 调用处把 `jump` 透传进去（紧跟 `session` 之后）：

```rust
        if let Err(err) = run_session(
            session,
            jump,
            cmd_rx,
            evt_tx_for_task.clone(),
            initial_cols,
            initial_rows,
        )
        .await
```

- [ ] **Step 4: 更新 `spawn_session` 调用点**——`src/app.rs` 的 `start_session_io`（约 2783）。本任务先传 `None`（Task 7 改为真实跳板）：

```rust
    let (handle, mut rx) = spawn_session(
        ctx.runtime.handle(),
        tab_id.clone(),
        session,
        None,
        initial_cols,
        initial_rows,
    );
```

- [ ] **Step 5: 跑全量测试 + 编译确认零回归**

Run: `cargo test --lib && cargo build`
Expected: 全绿、编译通过（这是纯重构，行为不变）。

- [ ] **Step 6: 提交**

```bash
git add src/ssh.rs src/app.rs
git commit -m "refactor(ssh): 抽出 connect_transport 助手并为会话预留 jump 参数"
```

---

## Task 5: 本地端口转发（-L）建立

在 `run_session` 认证成功、打开 shell/monitor 通道之后，按 `session.tunnels` 建立监听。Handle 由本任务独占串行开 direct-tcpip 通道；acceptor 子任务只回传 `TcpStream`；pump 子任务只持 `ChannelStream`。

**Files:**
- Modify: `src/ssh.rs`（`SessionEvent` 枚举、`run_session` select 循环、新增 `ForwardReq` / `pump_forward`）

- [ ] **Step 1: 加 `SessionEvent::TunnelStatus` 变体**——在 `SessionEvent` 枚举末尾（`SftpTransfer { ... }` 之后）追加：

```rust
    /// 端口转发监听状态：listening=true 表示已开始监听，false 表示绑定失败。
    TunnelStatus {
        spec: String,
        listening: bool,
        msg: String,
    },
```

- [ ] **Step 2: 加 `ForwardReq` 与 `pump_forward`**——放在 `run_session` 之后、`parse_monitor_block` 之前。

```rust
/// 一条「本地监听口接到的连接」请求，由 acceptor 子任务发回 run_session 主任务，
/// 后者用独占的 handle 串行开 direct-tcpip 通道。
struct ForwardReq {
    stream: tokio::net::TcpStream,
    peer: std::net::SocketAddr,
    dest_host: String,
    dest_port: u16,
}

/// 在本地 TCP 连接与 SSH direct-tcpip 通道之间双向搬运字节，直到任一端关闭。
/// `ChannelStream` 自动 `Unpin + Send`，可安全移入独立任务并参与 copy_bidirectional。
async fn pump_forward(mut tcp: tokio::net::TcpStream, channel: russh::Channel<client::Msg>) {
    let mut chan = channel.into_stream();
    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut chan).await;
}
```

- [ ] **Step 3: 认证成功后建立监听**——在 `run_session` 内、`let _ = events.send(SessionEvent::Connected);` 之后、`let mut prompt_injected = false;` 之前插入：

```rust
    // 本地端口转发（-L）：每条隧道起一个 acceptor 子任务（只 bind+accept+回传，
    // 不碰 handle），接受到的连接经 mpsc 回到本任务串行开 direct-tcpip 通道。
    let (fwd_tx, mut fwd_rx) = mpsc::unbounded_channel::<ForwardReq>();
    let mut tunnel_acceptors: Vec<JoinHandle<()>> = Vec::new();
    let mut pump_tasks: Vec<JoinHandle<()>> = Vec::new();
    for spec in &session.tunnels {
        let bind_addr = if spec.bind_addr.is_empty() {
            "127.0.0.1"
        } else {
            spec.bind_addr.as_str()
        };
        let listen = format!("{}:{}", bind_addr, spec.bind_port);
        match tokio::net::TcpListener::bind(&listen).await {
            Ok(listener) => {
                let _ = events.send(SessionEvent::TunnelStatus {
                    spec: spec.to_line(),
                    listening: true,
                    msg: format!("{} {}", t("本地转发监听", "forwarding on"), listen),
                });
                let tx = fwd_tx.clone();
                let dest_host = spec.dest_host.clone();
                let dest_port = spec.dest_port;
                tunnel_acceptors.push(tokio::spawn(async move {
                    loop {
                        match listener.accept().await {
                            Ok((stream, peer)) => {
                                if tx
                                    .send(ForwardReq {
                                        stream,
                                        peer,
                                        dest_host: dest_host.clone(),
                                        dest_port,
                                    })
                                    .is_err()
                                {
                                    break; // run_session 已退出
                                }
                            }
                            Err(e) => {
                                tracing::warn!("tunnel accept error on {listen}: {e}");
                                break;
                            }
                        }
                    }
                }));
            }
            Err(e) => {
                let _ = events.send(SessionEvent::TunnelStatus {
                    spec: spec.to_line(),
                    listening: false,
                    msg: format!("{}: {e}", t("本地转发监听失败", "forward bind failed")),
                });
            }
        }
    }
    // 仅留各 acceptor 持有的 sender 克隆；它们全部退出后 fwd_rx 自然结束。
    drop(fwd_tx);
```

- [ ] **Step 4: select 循环加转发分支**——在 `run_session` 的 `loop { tokio::select! { ... } }` 内，与 `cmd = commands.recv()` 等并列，新增一个分支：

```rust
            maybe_fwd = fwd_rx.recv() => {
                if let Some(req) = maybe_fwd {
                    // handle 为 &self 调用，串行开通道；await 期间由 russh 会话任务驱动应答，不阻塞本循环逻辑。
                    match handle
                        .channel_open_direct_tcpip(
                            req.dest_host.clone(),
                            req.dest_port as u32,
                            req.peer.ip().to_string(),
                            req.peer.port() as u32,
                        )
                        .await
                    {
                        Ok(channel) => {
                            pump_tasks.push(tokio::spawn(pump_forward(req.stream, channel)));
                        }
                        Err(e) => {
                            tracing::warn!(
                                "direct-tcpip to {}:{} failed: {e}",
                                req.dest_host,
                                req.dest_port
                            );
                        }
                    }
                }
            }
```

- [ ] **Step 5: 退出时清理子任务**——在 `run_session` 末尾、`let _ = handle.disconnect(...)` 之前插入：

```rust
    for h in tunnel_acceptors {
        h.abort();
    }
    for h in pump_tasks {
        h.abort();
    }
```

- [ ] **Step 6: 编译 + 既有测试**

Run: `cargo build && cargo test --lib`
Expected: 编译通过、全绿（未触及现有测试逻辑）。

- [ ] **Step 7: 手动验证（集成）**——需要一台可 SSH 的主机。临时给某会话的 `sessions.json` 加：`"tunnels": [{"bind_port": 8022, "dest_host": "127.0.0.1", "dest_port": 22}]`，连接后另开终端：

```bash
ssh -p 8022 <user>@127.0.0.1   # 经隧道应连到远端的 sshd
```

Expected: 能握手（即把本地 8022 转发到了远端 127.0.0.1:22）；关闭 LibSSH 标签后本地 8022 不再监听（`lsof -i :8022` 为空）。

- [ ] **Step 8: 提交**

```bash
git add src/ssh.rs
git commit -m "feat(ssh): 本地端口转发(-L)——acceptor/direct-tcpip/pump 隔离 handle 所有权"
```

---

## Task 6: 单跳跳板机

在 `connect_transport` 里接入跳板：当 `jump` 为 `Some` 时，先连+认证跳板会话，再在其上开 direct-tcpip 到目标，把 `into_stream()` 喂给 `connect_stream` 得到目标 pre-auth handle，并返回跳板 handle 供保活。

**Files:**
- Modify: `src/ssh.rs`（新增 `connect_and_auth`；`connect_transport` 用 `jump`）

- [ ] **Step 1: 加 `connect_and_auth` 助手**（连接+认证某会话，返回**已认证** handle，供跳板使用）——放在 `connect_transport` 之前：

```rust
/// 连接并认证一个会话（直连/代理），返回已认证的 Handle。用于跳板机：
/// 跳板自身必须先认证，才能在其上开到目标的 direct-tcpip 通道。
async fn connect_and_auth(
    session: &Session,
    config: Arc<client::Config>,
) -> Result<Handle<ClientHandler>> {
    let handler = ClientHandler {
        host: session.host.clone(),
        port: session.port,
    };
    let addr = format!("{}:{}", session.host, session.port);
    let mut handle = match crate::proxy::resolve(&session.proxy) {
        Some(proxy) => {
            let stream = crate::proxy::connect(&proxy, &session.host, session.port)
                .await
                .with_context(|| format!("jump proxy connect to {} failed", addr))?;
            client::connect_stream(config, stream, handler)
                .await
                .with_context(|| format!("jump connect {} failed", addr))?
        }
        None => client::connect(config, addr.as_str(), handler)
            .await
            .with_context(|| format!("jump connect {} failed", addr))?,
    };

    let authed = match session.auth {
        AuthMethod::Password => handle
            .authenticate_password(&session.user, session.password.as_str())
            .await
            .context("jump password auth failed")?,
        AuthMethod::Key => {
            let key_with_hash = load_private_key_for_auth(&session.private_key_path)?;
            handle
                .authenticate_publickey(&session.user, key_with_hash)
                .await
                .context("jump publickey auth failed")?
        }
    };
    if !authed {
        let _ = handle
            .disconnect(Disconnect::ByApplication, "jump auth failed", "")
            .await;
        bail!(t(
            "跳板机认证失败",
            "jump host authentication failed"
        ));
    }
    Ok(handle)
}
```

- [ ] **Step 2: `connect_transport` 用 `jump`**——把 Task 4 里 `_jump` 改名为 `jump` 并在函数开头优先处理跳板分支：

```rust
async fn connect_transport(
    session: &Session,
    jump: &Option<Session>,
    config: Arc<client::Config>,
    events: &UnboundedSender<SessionEvent>,
) -> Result<(Handle<ClientHandler>, Option<Handle<ClientHandler>>)> {
    let handler = ClientHandler {
        host: session.host.clone(),
        port: session.port,
    };
    let addr = format!("{}:{}", session.host, session.port);

    // 跳板机：先连+认证跳板，再在其上开到目标的 direct-tcpip，把通道流当传输层连目标。
    if let Some(j) = jump {
        let _ = events.send(SessionEvent::Status(format!(
            "{} {}@{}:{} -> {}",
            t("经跳板机连接", "via jump host"),
            j.user,
            j.host,
            j.port,
            addr
        )));
        let jump_handle = connect_and_auth(j, Arc::new(client::Config::default())).await?;
        let channel = jump_handle
            .channel_open_direct_tcpip(session.host.clone(), session.port as u32, "127.0.0.1", 0)
            .await
            .with_context(|| format!("jump direct-tcpip to {} failed", addr))?;
        let stream = channel.into_stream();
        let handle = client::connect_stream(config, stream, handler)
            .await
            .with_context(|| format!("connect {} via jump failed", addr))?;
        return Ok((handle, Some(jump_handle)));
    }

    // 直连 / 代理（与 Task 4 相同）。
    let handle = match crate::proxy::resolve(&session.proxy) {
        Some(proxy) => {
            let _ = events.send(SessionEvent::Status(format!(
                "{} {} -> {}",
                t("经代理连接", "via proxy"),
                crate::proxy::describe(&proxy),
                addr
            )));
            let stream = crate::proxy::connect(&proxy, &session.host, session.port)
                .await
                .with_context(|| format!("proxy connect to {} failed", addr))?;
            client::connect_stream(config, stream, handler)
                .await
                .with_context(|| format!("connect {} failed", addr))?
        }
        None => client::connect(config, addr.as_str(), handler)
            .await
            .with_context(|| format!("connect {} failed", addr))?,
    };
    Ok((handle, None))
}
```

> `_jump_keepalive`（Task 4 在 `run_session` 里引入）现在会在跳板场景持有 `Some(jump_handle)`，其生命周期贯穿整个 `run_session`，保证 direct-tcpip 隧道不被提前关闭——无需再改 `run_session`。

- [ ] **Step 3: 编译 + 既有测试**

Run: `cargo build && cargo test --lib`
Expected: 编译通过、全绿。

- [ ] **Step 4: 手动验证（集成）**——临时把某会话 `sessions.json` 的 `jump_session_id` 设为另一个可达会话的 id（跳板与目标可为同一台机器做冒烟）。连接目标会话。

Expected: 状态栏出现「经跳板机连接 …」，最终正常进入目标 shell；跳板认证失败时报「跳板机认证失败」。

- [ ] **Step 5: 提交**

```bash
git add src/ssh.rs
git commit -m "feat(ssh): 单跳跳板机——经已存会话的 direct-tcpip + connect_stream"
```

---

## Task 7: app.rs 连接路径解析并传入跳板

在 `start_session_io` 增 `jump` 参，并在其调用点用 `ConfigStore::resolve_jump` + `resolve_session_password` 解析跳板会话后传入。

**Files:**
- Modify: `src/app.rs`（`start_session_io` 签名 ~2774；`spawn_session` 调用 ~2783；各 `start_session_io` 调用点——新开会话的 connect 处理器、原地重连处理器）

- [ ] **Step 1: `start_session_io` 增 `jump` 参并透传**

签名加参数（放在 `session: Session` 之后）：

```rust
fn start_session_io(
    weak: slint::Weak<AppWindow>,
    ctx: &SessionIoCtx,
    tab_id: String,
    session: Session,
    jump: Option<Session>,
    initial_cols: u32,
    initial_rows: u32,
) {
```

把 Task 4 改过的 `spawn_session(...)` 调用里的 `None` 换成 `jump`：

```rust
    let (handle, mut rx) = spawn_session(
        ctx.runtime.handle(),
        tab_id.clone(),
        session,
        jump,
        initial_cols,
        initial_rows,
    );
```

- [ ] **Step 2: 各调用点解析跳板**——`grep -n "start_session_io(" src/app.rs` 找到全部调用点（connect 新开标签、reconnect 原地重连）。每处在调用前、拿到要连接的 `session`（已 `resolve_session_password`）之后，插入：

```rust
        // 解析跳板会话（引用另一个已存会话）；克隆后解密其密码，仅用于本次连接。
        let jump = {
            let store = /* 该处理器持有的 Rc<RefCell<ConfigStore>> */.borrow();
            store.resolve_jump(&session).map(|mut j| {
                crate::secrets::resolve_session_password(&mut j);
                j
            })
        };
```

并把 `jump` 作为新参数传入 `start_session_io(...)`。

> 实现注记：各处理器里 store 的克隆名不同（如 `connect_store` / `rc_store` 等）。用该闭包已捕获的 store 句柄即可；若某重连处理器未捕获 store，则在其闭包定义处补 `let xxx_store = store.clone();` 一份 `Rc<RefCell<ConfigStore>>`（与文件内既有 `submit_store` 等同一模式）。

- [ ] **Step 3: 编译 + 测试**

Run: `cargo build && cargo test --lib`
Expected: 编译通过、全绿。

- [ ] **Step 4: 提交**

```bash
git add src/app.rs
git commit -m "feat(app): 连接时解析跳板会话并传入会话 IO"
```

---

## Task 8: 会话对话框配置 UI（跳板下拉 + 隧道文本）

给 `SessionDialog` 加「跳板会话」下拉与「本地转发」多行输入，并在 `submit` / `test` 接线读写。

**Files:**
- Modify: `ui/session_dialog.slint`（`SessionDraft` 结构、对话框卡片、submit/test 内联构造）
- Modify: `src/app.rs`（`on_session_dialog_submit` ~1037、`on_session_dialog_test` ~1123，以及打开对话框时回填 draft 的位置）
- Modify: `lang/zh/LC_MESSAGES/LibSSH.po`、`lang/en/LC_MESSAGES/LibSSH.po`

- [ ] **Step 1: 扩展 Slint `SessionDraft` 结构**——在 `ui/session_dialog.slint` 顶部结构体末尾（`remember: bool,` 后）加：

```slint
    jump-session-id: string,   // "" = 不经跳板
    tunnels-text: string,      // 每行一条 [bind:]port:host:port
```

并在组件属性区（`draft-key-path` 附近）加对应 in-out 属性 + 跳板候选列表：

```slint
    in-out property <string> draft-jump-session-id;
    in-out property <string> draft-tunnels-text;
    in property <[GroupInfo]> jump-candidates;   // 复用 GroupInfo{name,label,...}：name=会话 id，label=会话名
```

> 复用既有 `GroupInfo` 结构承载「会话 id + 展示名」，避免新增 Slint 结构；`jump-candidates` 由 Rust 端填充为「除当前会话外的所有会话」。

- [ ] **Step 2: 在卡片里加控件**——在 `Rectangle { vertical-stretch: 1; }` 之前插入一段（跟随 auth 区的视觉风格）。跳板用与 Group 同款的 PopupWindow 下拉；隧道用多行 `TextInput`：

```slint
            // 跳板机（可选）
            VerticalLayout {
                spacing: 4px;
                Text {
                    text: @tr("Jump host (via saved session)");
                    color: Theme.text-secondary;
                    font-size: Theme.fs-sm;
                }
                Rectangle {
                    height: 32px;
                    border-radius: Theme.radius-sm;
                    border-width: 1px;
                    border-color: jmp-ta.has-hover ? Theme.border-strong : Theme.border-subtle;
                    background: Theme.bg-root;
                    HorizontalLayout {
                        padding-left: 10px; padding-right: 8px; spacing: 8px;
                        Text {
                            text: root.draft-jump-session-id == "" ? @tr("Direct (no jump)")
                                : root.jump-candidates.length > 0 ? root.draft-jump-session-id : root.draft-jump-session-id;
                            color: Theme.text-primary; font-size: Theme.fs-md;
                            vertical-alignment: center; horizontal-stretch: 1; overflow: elide;
                        }
                        Text {
                            text: "\u{E5CF}"; font-family: "Material Icons";
                            color: Theme.text-muted; font-size: 14px; vertical-alignment: center;
                        }
                    }
                    jmp-ta := TouchArea { mouse-cursor: pointer; clicked => { jmp-pop.show(); } }
                    jmp-pop := PopupWindow {
                        x: 0; y: parent.height + 2px; width: parent.width;
                        height: min(root.jump-candidates.length + 1, 8) * 29px + 8px;
                        Rectangle {
                            background: Theme.bg-panel; border-radius: Theme.radius-sm;
                            border-width: 1px; border-color: Theme.border-strong;
                            VerticalLayout {
                                padding: 4px; spacing: 1px;
                                Rectangle {
                                    height: 28px; border-radius: 4px;
                                    background: jn-ta.has-hover ? Theme.bg-hover : transparent;
                                    Text {
                                        x: 8px; text: @tr("Direct (no jump)");
                                        color: Theme.text-primary; font-size: Theme.fs-md; vertical-alignment: center;
                                    }
                                    jn-ta := TouchArea { mouse-cursor: pointer; clicked => { root.draft-jump-session-id = ""; jmp-pop.close(); } }
                                }
                                for c in root.jump-candidates : Rectangle {
                                    height: 28px; border-radius: 4px;
                                    background: jc-ta.has-hover ? Theme.bg-hover : transparent;
                                    Text {
                                        x: 8px; text: c.label;
                                        color: Theme.text-primary; font-size: Theme.fs-md; vertical-alignment: center;
                                    }
                                    jc-ta := TouchArea { mouse-cursor: pointer; clicked => { root.draft-jump-session-id = c.name; jmp-pop.close(); } }
                                }
                            }
                        }
                    }
                }
            }

            // 本地端口转发（每行一条 [bind:]port:host:port）
            VerticalLayout {
                spacing: 4px;
                Text {
                    text: @tr("Local forwards (one per line: [bind:]port:host:port)");
                    color: Theme.text-secondary;
                    font-size: Theme.fs-sm;
                }
                Rectangle {
                    height: 56px;
                    border-radius: Theme.radius-sm;
                    border-width: 1px;
                    border-color: tin.has-focus ? Theme.accent : Theme.border-subtle;
                    background: Theme.bg-root;
                    tin := TextInput {
                        x: 10px; y: 6px; width: parent.width - 20px; height: parent.height - 12px;
                        text <=> root.draft-tunnels-text;
                        color: Theme.text-primary; font-size: Theme.fs-md;
                        single-line: false; wrap: no-wrap;
                    }
                }
            }
```

> 卡片当前固定 `height: 560px` 且内容已较满。本步把卡片高度调到 `height: 660px`（`ui/session_dialog.slint` 约第 58 行）以容纳两段新控件；若仍偏挤，后续可改为可滚动容器（一期不做）。

- [ ] **Step 3: 把新字段塞进 submit/test 内联构造**——在 `root.test-connection({ ... })`（~377）与 `root.submit({ ... })`（~400）两处对象字面量里，于 `remember:` 后各加：

```slint
                                jump-session-id: root.draft-jump-session-id,
                                tunnels-text: root.draft-tunnels-text,
```

- [ ] **Step 4: Rust 端 submit 写入隧道/跳板**——`src/app.rs` `on_session_dialog_submit`（~1048 的 `Session { ... }` 字面量）把 `proxy`/`group` 同级补上：

```rust
            tunnels: crate::config::parse_tunnel_lines(draft.tunnels_text.as_ref()),
            jump_session_id: {
                let j = draft.jump_session_id.to_string();
                if j.is_empty() { None } else { Some(j) }
            },
```

- [ ] **Step 5: Rust 端 test 与 draft 回填**——
  (a) `on_session_dialog_test`（~1145 的 `Session { ... }` 字面量）同样补 `tunnels` / `jump_session_id` 两字段（值同 Step 4，从 `draft` 取）。
  (b) 找到「打开编辑对话框时回填 draft」的位置（设置 `w.set_draft_host(...)` 等的那段），追加：

```rust
            w.set_draft_jump_session_id(session.jump_session_id.clone().unwrap_or_default().into());
            w.set_draft_tunnels_text(
                session
                    .tunnels
                    .iter()
                    .map(|t| t.to_line())
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into(),
            );
```

  (c) 填充 `jump-candidates`：在打开对话框处，构造「除当前会话外的所有会话」为 `GroupInfo{ name: id, label: name, ... }` 的 `VecModel` 并 `w.set_jump_candidates(...)`（仿现有 `set_groups` 的填充方式）。

- [ ] **Step 6: 更新 i18n po**——`lang/{zh,en}/LC_MESSAGES/LibSSH.po` 为新增 `@tr` 文案加条目：`Jump host (via saved session)` / `Direct (no jump)` / `Local forwards (one per line: [bind:]port:host:port)`。zh 译文示例：「跳板机（经已保存会话）」「直连（不经跳板）」「本地转发（每行一条：[bind:]端口:主机:端口）」。

- [ ] **Step 7: 编译 + 运行冒烟**

Run: `cargo build && cargo test --lib`
Expected: 编译通过、测试全绿。
手动：`cargo run` → 编辑一个会话 → 看到「跳板机」下拉与「本地转发」文本框；填入隧道行并保存 → 重开对话框该行回显；`sessions.json` 出现 `tunnels` / `jump_session_id`。

- [ ] **Step 8: 提交**

```bash
git add ui/session_dialog.slint src/app.rs lang/
git commit -m "feat(ui): 会话对话框支持配置本地转发与跳板会话"
```

---

## Task 9: 收尾——全量校验

- [ ] **Step 1: clippy 零告警**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: 无告警（CI/`.githooks` 同口径）。如有未用导入/变量按提示清理。

- [ ] **Step 2: 全量测试**

Run: `cargo test`
Expected: 全绿（含新增的 config 单测与既有 ~120 个测试）。

- [ ] **Step 3: 端到端手动回归**——`cargo run`：
  1. 无隧道无跳板的普通会话正常连接（零回归）。
  2. 带 `-L` 隧道会话：连接后经本地口可达远端服务；关标签后端口释放。
  3. 跳板会话：状态栏显示「经跳板机连接」，正常进入目标 shell。

- [ ] **Step 4: 最终提交（如有收尾改动）**

```bash
git add -A
git commit -m "chore: 端口转发与跳板机收尾(clippy/测试)"
```

---

## Self-Review（写计划者已核对）

**Spec 覆盖：** 本地转发(-L)=Task 1/2/5/8；单跳跳板=Task 1/3/4/6/7/8；配置持久化=Task 1；UI=Task 8；i18n=Task 8 Step 6。远程/动态转发、可视化隧道编辑器、CLI 跳板已在「明确不做」声明为后续。

**类型/签名一致性核对：**
- `TunnelSpec{bind_addr,bind_port:u16,dest_host,dest_port:u16}` 在 Task 1 定义，Task 2/5/8 一致引用。
- `Session.tunnels: Vec<TunnelSpec>` / `jump_session_id: Option<String>`（Task 1）→ Task 5 读 `session.tunnels`、Task 6 经 `connect_transport(jump)`、Task 7/8 读写一致。
- `spawn_session(.., session, jump, cols, rows)` / `run_session(session, jump, ..)`（Task 4）→ Task 7 调用点一致传 `Option<Session>`。
- `connect_transport(session, jump:&Option<Session>, config, events)`（Task 4 建，Task 6 用 jump）→ `run_session` 解构 `(handle, _jump_keepalive)` 一致。
- `SessionEvent::TunnelStatus{spec,listening,msg}`（Task 5）字段在发送处一致。
- `pump_forward(TcpStream, russh::Channel<client::Msg>)`（Task 5）与 `channel_open_direct_tcpip` 返回类型 `Channel<Msg>` 一致。
- `ConfigStore::resolve_jump(&self, &Session)->Option<Session>`（Task 3）→ Task 7 调用一致。

**关键正确性约束（已在相应 Task 注明）：** `Handle` 非 Clone/非 Sync → 不跨 spawn 共享，acceptor 只回传 `TcpStream`、pump 只持 `ChannelStream`(Send)；`_jump_keepalive` 必须存活至 `run_session` 结束以保活 direct-tcpip 隧道；跳板凭据经 `resolve_session_password` 在 app.rs 解密后传入。

**占位符扫描：** 无 TODO/“类似上文”/空泛“错误处理”；唯一非字面值是 Task 7 Step 2 的 store 句柄名（因各调用点闭包捕获名不同，已给出确定的获取规则与兜底 `store.clone()` 模式）。
