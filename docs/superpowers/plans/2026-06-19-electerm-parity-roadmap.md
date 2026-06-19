# electerm 功能对标路线图（4 项）

> **形态说明：** 这是一份**总览路线图**，不是可逐任务执行的完整计划。它锁定每项功能的架构方向、要动的文件、任务骨架、工作量/风险/依赖，用来排期与对齐。确定优先级后，再用 `superpowers:writing-plans` 把**单个功能**展开成 file-by-file、红-绿-重构、含真实代码的完整 TDD 计划（每份一个 `docs/superpowers/plans/YYYY-MM-DD-<feature>.md`，按现有惯例可配一份 `specs/` 设计稿）。

**目标：** 把 LibSSH 从「SSH/SFTP 轻量客户端」补齐到具备 electerm 的 4 项核心能力——端口转发/跳板机、终端分屏、Zmodem/Trzsz 终端内传输、批量命令广播——同时不偏离轻量 Rust+Slint 定位。

**对标项目：** electerm `/Library/Data/project/github/ssh_tool/electerm`（Electron 全协议终端套件，本路线图的功能参照系）。

---

## 0. 现有架构事实（落地这 4 项的地基）

读 `src/ssh.rs` / `src/app.rs` / `src/config.rs` / `ui/*.slint` 后确认的关键点：

| 维度 | 现状 | 对本路线图的意义 |
|---|---|---|
| 会话模型 | 每个 tab = 一个 tokio worker task。`spawn_session` → `run_session`，内部 `tokio::select!` 轮询 命令 / `channel.wait()` / 监控通道 | 端口转发、Zmodem 都在 `run_session` 的 select 循环里加分支即可，不动整体骨架 |
| 命令/事件 | `SessionCommand{RawInput,Resize,Close}` 进，`SessionEvent{Output,...,SftpTransfer{...}}` 出（MPSC） | 4 项都靠**新增 enum variant** 扩展，加法式、低风险 |
| 连接句柄 | `handle: russh::client::Handle<ClientHandler>`；代理路径已用 `client::connect_stream(stream, ...)`（`ssh.rs:279/342/492`） | **跳板机 = 把代理流换成 SSH channel 流**，复用现成的 `connect_stream` 通道 |
| tab 注册表 | `handles: HashMap<tab_id, SessionHandle>`、`tab_sessions: HashMap<tab_id, Session>`，单 `active_tab_id` | 批量广播直接遍历 `handles` 调 `send_raw`；分屏需把「tab→1 终端」改成「tab→N 窗格」 |
| `SessionHandle` | 已有 `send_raw(Vec<u8>)` / `resize` / `close`（`ssh.rs:206-218`） | 批量广播**零 ssh.rs 改动** |
| 配置 | `Session` 全字段 `#[serde(default)]`；`missing_optional_fields_load_with_defaults` 测试证明加字段免迁移 | 端口转发/跳板的配置扩展**无需写迁移代码** |
| UI | `SessionDialog`(session_dialog.slint，`SessionDraft`+一堆 draft 属性)、`CommandBar`(command_bar.slint，`send(string)`)、`TabBar`(tabs.slint) | 端口转发扩 `SessionDialog`；批量广播仿 `CommandBar`；分屏改终端内容区 |
| i18n | Rust 内联 `t("中","en")` + `lang/{zh,en}/LC_MESSAGES/LibSSH.po` | 每项新增文案两处都要补 |
| 质量门 | 全项目 ~120 个测试，TDD 是硬惯例；CI/`.githooks` 跑 `cargo test` + `cargo clippy -D warnings` | 每项计划首/末步必须是测试 + clippy 零告警 |

---

## 1. 推荐实施顺序与依赖

排序原则：**风险升序 + 价值 + 复用关系**（与你给的原始优先级不同——把最稳的当热身，最重的 UI 放后面，高风险的先验证）。

```
阶段1  D 批量命令广播      ← 最低风险，建立「多 handle 扇出」范式（2-3d）
          │ 扇出能力被 B 的「同步输入」复用
阶段2  A 端口转发 + 跳板机  ← 最高价值，自包含在 ssh.rs/config.rs，可与 spike 并行（5-8d）
阶段0  C-spike Zmodem/Trzsz ← 时间盒 1-2d，尽早跑、并行不阻塞，产出 go/no-go + 选型
          │ 结论 gate 住 C 的正式计划
阶段3  B 终端分屏          ← 最大 UI 重构，等核心稳定后做（5-8d）
阶段4  C Zmodem/Trzsz 实现 ← 受 spike 结论门控（工作量待定）
```

依赖关系：
- **D → B**：D 的「向多个 SessionHandle 扇出」直接被 B 的「窗格同步输入」复用。
- **A 独立**：只碰 `ssh.rs`/`config.rs`/`session_dialog.slint`，可任意时段并行。
- **spike → C**：spike 结论决定 C 走 Zmodem 还是 Trzsz，以及是否可行。
- **B 独立但最重**：UI 改动面最大，放最后降低返工。

> 工作量为单人粗估，仅供排期参照，不是承诺。

---

## 2. 功能 A — 端口转发 / 隧道 + 跳板机

**价值：** SSH 客户端最该有却唯独缺的能力（你的原始 #1）。内网穿透、堡垒机、本地访问远端数据库等强需求。

**为什么对 LibSSH 友好：** russh 原生支持 channel 级转发；跳板机本质是「代理流换成 SSH channel 流」，而 `connect_stream` 这条路代理功能已经在用。改动高度集中在 `ssh.rs`。

### 架构方向
- **本地转发 -L**：`run_session` 认证后，对每条启用的 Local 隧道起一个 `tokio::net::TcpListener`；每次 accept → `handle.channel_open_direct_tcpip(dest_host, dest_port, origin, origin_port)` → 拿到 `Channel` 转 `into_stream()` → 与 `TcpStream` 跑 `tokio::io::copy_bidirectional`。
- **远程转发 -R**：`handle.tcpip_forward(bind_addr, bind_port)` + 在 `ClientHandler` 实现 `server_channel_open_forwarded_tcpip`，把回推的 channel 连到本地目标。
- **动态转发 -D**：本地起一个 SOCKS5 server，每个请求 `channel_open_direct_tcpip` 到目标（可复用 `proxy.rs` 的 SOCKS 解析思路）。
- **跳板机（单跳先行）**：先连 + 认证跳板机（复用现有连接逻辑）→ `jump_handle.channel_open_direct_tcpip(target_host, target_port, ...)` → `into_stream()` → `client::connect_stream(config, jump_stream, handler)` 连目标。**与代理路径同形**（`ssh.rs:279/342/492`），代理 + 跳板可叠加。

### 要动的文件
- `src/config.rs`：`Session` 加 `#[serde(default)] tunnels: Vec<Tunnel>` 与 `#[serde(default)] jump: Option<JumpHost>`；新增 `enum TunnelKind{Local,Remote,Dynamic}`、`struct Tunnel{...}`、`struct JumpHost{...}`（或用 `jump_session_id: Option<String>` 引用已存会话，避免重复存凭据——**待定项**）。
- `src/ssh.rs`：转发建立逻辑 + `Handler::server_channel_open_forwarded_tcpip`；跳板嵌套连接；新增 `SessionEvent::TunnelStatus{...}` 反馈监听/失败。
- `ui/session_dialog.slint`：`SessionDraft` 扩字段 + 新增「隧道 / 跳板」分区（仿现有 auth 分区的 `if root.draft-auth==...` 结构）。
- `src/app.rs`：会话对话框读写新字段；隧道状态展示。
- `lang/{zh,en}/...po`：新文案。

### 任务骨架（每项→红绿重构 TDD）
1. config：`Tunnel`/`JumpHost` 类型 + 序列化往返测试（扩 `save_then_load_round_trips`）。
2. 隧道规格校验（端口范围、bind 地址、空目标）纯函数 + 单测。
3. 字节泵 helper：用 `tokio::io::duplex` 内存双工流测双向拷贝与半关闭。
4. 本地转发 -L：起 listener + direct-tcpip + 泵（SSH 级走集成/手测）。
5. 远程转发 -R：`tcpip_forward` + handler 回调。
6. 动态转发 -D：本地 SOCKS server。
7. 跳板单跳：跳板流 + `connect_stream` 嵌套。
8. UI 接线 + i18n + clippy 零告警。

**工作量：** 5-8d。**风险：** 中（russh 转发 API 正确性，但支持完善；-R/-D 比 -L 略繁）。
**待定项：** 跳板凭据是内联存还是引用已存会话；-R/-D 是否一期就做（建议一期先 -L + 单跳跳板，-R/-D 二期）。

---

## 3. 功能 B — 终端分屏

**价值：** 一个 tab 内同时盯多个终端（你的原始 #2）。

**为什么最重：** 当前「tab → 单个终端视图（按 `active_tab_id` 渲染）」「`TermBuffer` 按 tab 存」的模型要改成「tab → N 个窗格，每窗格独立会话 + 缓冲」。UI 改动面最大。

### 架构方向（一期收敛到两分屏）
- **窗格 = 独立会话**：分屏 tab 容纳一个布局（一期：水平二分 / 垂直二分），每个窗格有自己的 pane_id、`SessionHandle`、`TermBuffer`、PTY 尺寸。
- 终端内容区从「渲染 1 个 `terminal_view`」改成「渲染一个 `PaneContainer`（1 / 2-水平 / 2-垂直）」，每窗格复用 `terminal_view.slint`。
- 输入只进**聚焦窗格**；每窗格独立 resize PTY；`SessionEvent::Output` 按 pane_id 路由到对应缓冲。
- 可选「同步输入」开关：把按键扇出到本 tab 所有窗格——**复用功能 D 的扇出**。

### 要动的文件
- `ui/app.slint`：终端内容区 → `PaneContainer`（布局 + 焦点边框）。
- `ui/terminal_view.slint`：参数化以按 pane_id 复用。
- `ui/tabs.slint`：分屏按钮（水平/垂直）。
- `src/app.rs`：pane 注册表（pane_id → 会话/缓冲/handle）、焦点跟踪、按窗格 resize、Output 路由改 pane_id。
- `lang/...po`。

### 任务骨架
1. pane 数据模型 + Slint `PaneInfo` 结构（先渲染、再接线）。
2. 容器布局：单窗格 → 二分（水平/垂直）切换。
3. 焦点跟踪 + 输入路由到聚焦窗格。
4. 每窗格 PTY resize（窗格尺寸变化→`SessionCommand::Resize`）。
5. Output 事件按 pane_id 路由到对应 `TermBuffer`。
6. 关闭单个窗格 / 关闭整个分屏 tab 的生命周期。
7.（可选）同步输入开关，复用 D 的扇出。
8. i18n + clippy。

**工作量：** 5-8d。**风险：** 中高（终端渲染/焦点/resize 正确性；`TermBuffer` 由按 tab 改为按 pane）。
**待定项：** 一期只做二分还是直接四宫格；窗格是否允许跨 tab 拖拽（建议一期二分、不跨 tab）。

---

## 4. 功能 C — Zmodem / Trzsz 终端内传输（**spike 先行**）

**价值：** 不开 SFTP 面板、或穿过跳板机直接传文件（你的原始 #3）。

**决策（已定）：** 先做 1-2 天**技术验证 spike**，再决定协议与是否可行。这是 4 项里风险最高的——ZMODEM 二进制帧协议繁琐，Rust 侧 `zmodem2` / `trzsz-rs` 成熟度一般，且本项目当前**未引入**任何相关 crate。

### 阶段 0：spike（一次性、时间盒、可丢弃分支）
目标产出一份 findings + 选型建议，**不求并入主干**：
1. 验证 `zmodem2`：能否在任意 `AsyncRead+AsyncWrite`（即 SSH channel 流）上驱动一次传输？对真实服务器的 `sz`/`rz`（lrzsz）跑通文件往返。
2. 验证 `trzsz-rs`：同上，针对 `trz`/`tsz`。
3. 验证**检测**：如何在 PTY 输出流里识别启动序列——Zmodem 的 `**\x18B00...`、Trzsz 的 `::TRZSZ:TRANSFER:`——并把会话切入二进制传输模式。
4. 评估 tmux/跳板兼容性。
5. 输出：可行性结论 + 选 Zmodem / Trzsz / 都做。

### 阶段 4：正式实现（受 spike 门控，结论出来后写完整计划）
预期形态：
- `run_session` 引入 `SessionMode{Shell, Transfer}` 状态；输出流命中启动序列→切 Transfer。
- Transfer 模式下把字节泵从「PTY↔UI」切到「协议驱动 ↔ SSH channel」。
- 文件选择走 `rfd`（**已是依赖**）。
- 进度复用现成的 `SessionEvent::SftpTransfer{id,name,transferred,total,state,...}` 事件形状，UI 几乎不用新做。
- 新增 crate（`zmodem2` 或 `trzsz` 系）进 `Cargo.toml`。

**工作量：** spike 1-2d；实现待定。**风险：** 高（协议 + crate 成熟度）。

---

## 5. 功能 D — 批量命令广播

**价值：** 一条命令群发到多个终端，集群运维（你的原始 #4）。

**为什么当热身：** `SessionHandle::send_raw` 已存在（`ssh.rs:207`），`handles: HashMap<tab_id, SessionHandle>` 已存在。**零 ssh.rs 改动、零 config 改动**（命名目标组可二期），主要是 UI + app.rs 扇出。最低风险，先建立「多 handle 扇出」范式供 B 复用。

### 架构方向
- 一个「广播条」UI（可开关）：开启后，输入的命令扇出到**选中的一组终端 tab**。
- 目标选择：一期给「全部终端 tab」+「勾选子集」。
- app.rs 广播回调遍历选中 tab_id → 查 `handles` → 对每个 `send_raw(cmd + "\r")`。

### 要动的文件
- `ui/app.slint` + 新 `ui/broadcast_bar.slint`（仿 `command_bar.slint` 的 `CommandBar`：`send(string)` + 目标选择器）。
- `src/app.rs`：广播回调 + 选中目标集合状态。
- `lang/...po`。
- `src/ssh.rs`：**不动**。`src/config.rs`：一期不动（命名目标组二期再加）。

### 任务骨架
1. 目标解析纯函数：选中 tab_id 集合 + 当前 `handles` → 实际可达 handle 列表（跳过未连接/welcome），单测覆盖。
2. 广播条 Slint 组件（先渲染、再接线）。
3. app.rs 接线：回调 → 扇出 `send_raw`。
4. 目标选择交互（全选 / 勾选子集 / 高亮被广播的 tab）。
5. i18n + clippy。

**工作量：** 2-3d。**风险：** 低。
**待定项：** 是否一期就做「同步输入」（每次按键实时镜像，而非整行发送）——建议一期先整行广播，实时同步并入 B 的窗格同步输入。

---

## 6. 横切关注点（每份单功能计划都要带）

- **i18n**：新文案两处——Rust 内联 `t("中","en")` + `lang/{zh,en}/LC_MESSAGES/LibSSH.po`。表格类布局注意既有惯例（多弹性列 `preferred-width:0`、表头/行两处同步）。
- **测试**：延续 TDD。纯逻辑（隧道规格校验、广播目标解析、Zmodem 检测、字节泵）走单元测试；SSH/终端集成部分标注为集成/手测，别假装能纯单测。每份计划首步写失败测试、末步 `cargo test` + `cargo clippy --all-targets -- -D warnings` 零告警。
- **配置兼容**：`Session`/`ConfigFile` 一律加法式 `#[serde(default)]`，无需迁移代码（`missing_optional_fields_load_with_defaults` 已证）。
- **文档惯例**：每项展开成 `docs/superpowers/plans/YYYY-MM-DD-<feature>.md`，复杂项配 `docs/superpowers/specs/...-design.md`（沿用仓库现有成对结构）。
- **安全基线**：转发/跳板涉及新监听端口与新出站连接，沿用项目对监控通道那种「防恶意服务器」的克制（限量、饱和运算、不无限缓冲）。

---

## 7. 下一步

确认后，我用 `superpowers:writing-plans` 把**某一项**展开成完整的、含真实 Rust/Slint 代码和红绿重构步骤的 TDD 计划。建议起点二选一：

- **功能 D 批量命令广播** —— 最快见效、最低风险，先把多终端扇出范式跑通；或
- **功能 A 端口转发 + 跳板机** —— 你的原始最高价值项，自包含、可与 Zmodem spike 并行。

并行上可随时启动 **C 的 spike**（1-2 天、可丢弃），尽早拿到 Zmodem/Trzsz 的 go/no-go。

你说从哪项开始细化。
