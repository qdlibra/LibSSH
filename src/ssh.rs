//! SSH session manager.
//!
//! Each open terminal tab maps to exactly one worker task. Commands come in via
//! an MPSC channel and session events are pushed back to the UI.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use russh::client::{self, Handle, Handler};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::load_secret_key;
use russh::{ChannelId, ChannelMsg, Disconnect};
use ssh_key::{HashAlg, PublicKey};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::config::{AuthMethod, Session};
use crate::i18n::t;

// `known_hosts.rs` 存在于 src/ 但未在 crate 根（main.rs）注册为模块；本轮硬约束是
// 只改 ssh.rs，故在此把它挂为 ssh 的子模块（不触碰 main.rs）。known_hosts.rs 内部
// 不含任何 crate::/super::/self:: 引用，作为子模块挂载不影响其路径解析。
//
// allow(dead_code)：作为私有子模块挂载后，其公开 API 的可见面收敛到本 crate 内；
// `verify`/`remember` 已被 check_server_key 调用，但 `fingerprint` 留待后续「带指纹的
// 终端报错/UI」（本轮明确不做）才接入，现阶段无调用方。不能改 known_hosts.rs，故在
// 模块声明处统一豁免，避免 clippy -D warnings 因这一处未用公开函数而失败。
#[allow(dead_code)]
#[path = "known_hosts.rs"]
mod known_hosts;

/// Metadata for one remote filesystem entry returned by SFTP listing.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub name: String,
    pub full_path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: u32,
}

/// One node in the remote directory tree panel.
#[derive(Debug, Clone)]
pub struct RemoteTreeNode {
    pub path: String,
    pub name: String,
    pub depth: u32,
    pub expanded: bool,
    pub has_children: bool,
}

pub fn format_size(bytes: u64) -> String {
    if bytes < 1_024 {
        format!("{} B", bytes)
    } else if bytes < 1_024 * 1_024 {
        format!("{:.1} KB", bytes as f64 / 1_024.0)
    } else if bytes < 1_024 * 1_024 * 1_024 {
        format!("{:.1} MB", bytes as f64 / (1_024.0 * 1_024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1_024.0 * 1_024.0 * 1_024.0))
    }
}

pub fn format_mtime(ts: u32) -> String {
    use chrono::{DateTime, TimeZone, Utc};
    let dt: DateTime<Utc> = Utc
        .timestamp_opt(ts as i64, 0)
        .single()
        .unwrap_or_else(Utc::now);
    dt.format("%Y-%m-%d %H:%M").to_string()
}

/// Extract the remote path from an OSC 7 sequence embedded in `text`.
pub fn extract_osc7_path(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != 0x1b || bytes[i + 1] != b']' {
            i += 1;
            continue;
        }
        let osc_start = i + 2;
        i += 2;
        let mut end = i;
        while end < bytes.len() {
            if bytes[end] == 0x07
                || (bytes[end] == 0x1b && end + 1 < bytes.len() && bytes[end + 1] == b'\\')
            {
                break;
            }
            end += 1;
        }
        if end >= bytes.len() {
            break;
        }
        if let Ok(content) = std::str::from_utf8(&bytes[osc_start..end]) {
            if let Some(rest) = content.strip_prefix("7;file://") {
                let path = if rest.starts_with('/') {
                    rest.to_string()
                } else if let Some(slash) = rest.find('/') {
                    rest[slash..].to_string()
                } else {
                    "/".to_string()
                };
                return Some(url_decode(&path));
            }
        }
        i = end + 1;
    }
    None
}

fn url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next();
            let h2 = chars.next();
            match (h1, h2) {
                (Some(a), Some(b)) => {
                    let hex = format!("{a}{b}");
                    if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                        result.push(byte as char);
                    } else {
                        result.push('%');
                        result.push(a);
                        result.push(b);
                    }
                }
                (Some(a), None) => {
                    result.push('%');
                    result.push(a);
                }
                _ => result.push('%'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

#[derive(Debug)]
pub enum SessionCommand {
    RawInput(Vec<u8>),
    Resize(u32, u32),
    Close,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // SFTP variants are wired in M8; the shared event bus exists now.
pub enum SessionEvent {
    Status(String),
    Output(String),
    Connected,
    Closed(String),
    ResourceStats {
        cpu_percent: f32,
        mem_used_kib: u64,
        mem_total_kib: u64,
        swap_used_kib: u64,
        swap_total_kib: u64,
        net: Vec<(String, u64, u64)>,
        disks: Vec<(String, u64, u64)>,
    },
    CwdChanged(String),
    SftpEntries {
        path: String,
        entries: Vec<RemoteEntry>,
    },
    SftpStatus(String),
    /// 一次目录列举以失败告终（连接/认证失败、权限不足、路径不存在等）。
    /// 携带错误文案。与 `SftpEntries`（成功）配对：二者都必须复位前端的
    /// `sftp_loading`，否则 `sftp_loading` 会变成只进不出的陷阱状态 ——
    /// SFTP 面板永久停在「加载中…」，连刷新都救不回来（刷新只是重跑同一次
    /// 失败的 read_dir）。
    SftpLoadFailed(String),
    SftpFileContent {
        remote: String,
        filename: String,
        content: String,
    },
    SftpTreeUpdate(Vec<RemoteTreeNode>),
    SftpTransfer {
        id: String,
        name: String,
        is_upload: bool,
        transferred: u64,
        total: u64,
        state: u8,
        msg: String,
    },
    /// 端口转发监听状态：listening=true 表示已开始监听，false 表示绑定失败。
    TunnelStatus {
        spec: String,
        listening: bool,
        msg: String,
    },
}

pub struct SessionHandle {
    #[allow(dead_code)]
    pub tab_id: String,
    pub commands: UnboundedSender<SessionCommand>,
    #[allow(dead_code)]
    pub join: JoinHandle<()>,
}

impl SessionHandle {
    pub fn send_raw(&self, bytes: Vec<u8>) {
        let _ = self.commands.send(SessionCommand::RawInput(bytes));
    }

    pub fn resize(&self, cols: u32, rows: u32) {
        let _ = self.commands.send(SessionCommand::Resize(cols, rows));
    }

    pub fn close(&self) {
        let _ = self.commands.send(SessionCommand::Close);
    }
}

pub struct ExecResult {
    pub exit_status: Option<u32>,
    pub stdout: String,
    pub stderr: String,
}

pub(crate) fn private_key_path_for_auth(raw: &str) -> Result<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(anyhow!(t("私钥路径为空", "private key path is empty")));
    }

    let normalised = raw.replace('\\', "/");
    let without_public_suffix = normalised
        .strip_suffix(".pub")
        .map(str::to_string)
        .unwrap_or(normalised);

    if without_public_suffix == "~" {
        if let Some(home) = directories::UserDirs::new() {
            return Ok(home.home_dir().to_path_buf());
        }
    } else if let Some(rest) = without_public_suffix.strip_prefix("~/") {
        if let Some(home) = directories::UserDirs::new() {
            return Ok(home.home_dir().join(rest));
        }
    }

    Ok(PathBuf::from(without_public_suffix))
}

pub(crate) fn load_private_key_for_auth(raw: &str) -> Result<PrivateKeyWithHashAlg> {
    let key_path = private_key_path_for_auth(raw)?;
    let keypair = load_secret_key(&key_path, None)
        .with_context(|| format!("failed to load key {}", key_path.display()))?;
    let hash = if keypair.algorithm().is_rsa() {
        Some(HashAlg::Sha256)
    } else {
        None
    };
    PrivateKeyWithHashAlg::new(Arc::new(keypair), hash)
        .context("invalid private key / hash algorithm combination")
}

/// 交互式长连接（终端 PTY、SFTP）共用的客户端配置。
///
/// **keepalive 是这两条独立连接的生命线**：每 60s 发一次 SSH keepalive
/// （keepalive@openssh.com）。断网 / 电脑休眠唤醒后 TCP 可能已半开——既不再有
/// 数据、也收不到 FIN，`channel.wait()` 会永久挂起、UI 卡在「已连接」；纯空闲
/// 连接也会被中间 NAT / 防火墙静默回收。keepalive 主动探测，连续 `keepalive_max`
/// 次无响应即判定断开，russh 随即关闭会话（终端据此走 Closed 流程并自动重连）。
/// `inactivity_timeout` 仅作兜底，有 keepalive 时通常不会先触发。
///
/// 终端与 SFTP **必须共用本函数**：两者是同一个 tab 下的两条独立 TCP 连接，若只给
/// 一条配 keepalive，另一条会在空闲期被静默断开——SFTP 曾因漏配而在终端仍正常时
/// 报「列目录失败 / session closed」。
pub(crate) fn keepalive_client_config() -> client::Config {
    client::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(60 * 10)),
        keepalive_interval: Some(std::time::Duration::from_secs(60)),
        keepalive_max: 3,
        ..<_>::default()
    }
}

/// 连接建立（DNS + TCP + SSH 握手 + 认证 + 开通道）的总超时。握手阶段 russh 的
/// keepalive / inactivity_timeout 尚未生效（二者由连接建立后的后台任务驱动），
/// 缺这层超时会在"TCP 可达但 SSH 无响应"（服务器重启中 / 防火墙黑洞 / 半开连接）时
/// 永久阻塞 —— 重连便卡在「连接中」、状态机永远到不了「已断开」而无法恢复。
/// 终端 run_session、SFTP run_sftp、连通性 test_connection 三处共用，防止分叉。
pub(crate) const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// 唤醒探测：进程被挂起（系统睡眠 / App Nap 等）时，tokio 基于单调时钟的定时器随进程
/// 冻结；唤醒后 keepalive 需重新累计 keepalive_interval × keepalive_max（最长 180s）才能
/// 判定半开连接，这段窗口里 channel.wait() 仍挂起、UI 卡「已连接」、输入无回显也不重连
/// （见 keepalive_client_config 注释）。run_session 用挂钟时间（SystemTime）跨轮询的真实
/// 流逝识别「刚从挂起恢复」，立即主动探测连接活性，把这段被动等待从最长 180s 收敛到数秒。
///
/// 轮询间隔：正常运行每 PROBE_INTERVAL 比较一次挂钟时间（开销仅一次 SystemTime 相减）。
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
/// 跨轮询真实流逝超过此值即判定刚从挂起/睡眠恢复（远大于 PROBE_INTERVAL 的正常抖动）。
const RESUME_GAP: std::time::Duration = std::time::Duration::from_secs(45);
/// 恢复后主动探测（开一条临时通道）的超时：半开连接收不到 channel-open 确认即在此超时。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 距上次探测的真实（挂钟）流逝是否大到说明进程刚从挂起/睡眠恢复，需主动探测连接活性。
/// 抽成纯函数便于单测锚定阈值不变量（本项目惯例：纯函数 + 单测）。
fn resumed_from_suspend(real_elapsed: std::time::Duration) -> bool {
    real_elapsed >= RESUME_GAP
}

/// 按会话配置测试连通性：TCP/代理直连 + SSH 握手 + 身份认证，成功即断开。
/// 整体 20 秒超时（覆盖 DNS、TCP、握手、认证任何一步卡住的情况）。
pub async fn test_connection(session: Session) -> Result<()> {
    let attempt = async move {
        let config = Arc::new(client::Config::default());
        let handler = ClientHandler {
            host: session.host.clone(),
            port: session.port,
        };
        let addr = format!("{}:{}", session.host, session.port);
        let mut handle = match crate::proxy::resolve(&session.proxy) {
            Some(proxy) => {
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

        let authed = match session.auth {
            AuthMethod::Password => handle
                .authenticate_password(&session.user, session.password.as_str())
                .await
                .context("password auth failed")?,
            AuthMethod::Key => {
                let key_with_hash = load_private_key_for_auth(&session.private_key_path)?;
                handle
                    .authenticate_publickey(&session.user, key_with_hash)
                    .await
                    .context("publickey auth failed")?
            }
        };

        if !authed {
            let _ = handle
                .disconnect(Disconnect::ByApplication, "auth failed", "")
                .await;
            bail!(t(
                "认证失败（用户名、密码或密钥不正确）",
                "authentication failed (wrong username, password or key)"
            ));
        }

        let _ = handle
            .disconnect(Disconnect::ByApplication, "bye", "")
            .await;
        Ok(())
    };

    match tokio::time::timeout(CONNECT_TIMEOUT, attempt).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!(t(
            "连接超时（20 秒）",
            "connection timed out (20s)"
        ))),
    }
}

pub async fn run_exec(session: Session, command: &str) -> Result<ExecResult> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(60 * 10)),
        ..<_>::default()
    });
    let handler = ClientHandler {
        host: session.host.clone(),
        port: session.port,
    };
    let addr = format!("{}:{}", session.host, session.port);
    let mut handle = match crate::proxy::resolve(&session.proxy) {
        Some(proxy) => {
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

    let authed = match session.auth {
        AuthMethod::Password => handle
            .authenticate_password(&session.user, session.password.as_str())
            .await
            .context("password auth failed")?,
        AuthMethod::Key => {
            let key_with_hash = load_private_key_for_auth(&session.private_key_path)?;
            handle
                .authenticate_publickey(&session.user, key_with_hash)
                .await
                .context("publickey auth failed")?
        }
    };

    if !authed {
        let _ = handle
            .disconnect(Disconnect::ByApplication, "auth failed", "")
            .await;
        bail!("authentication failed");
    }

    let mut channel = handle
        .channel_open_session()
        .await
        .context("open exec channel")?;
    channel
        .exec(true, command.as_bytes())
        .await
        .context("start remote command")?;

    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_status = None;
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(120), channel.wait())
            .await
            .context("remote command timed out")?;
        match msg {
            Some(ChannelMsg::Data { data }) => {
                stdout.push_str(&String::from_utf8_lossy(&data));
            }
            Some(ChannelMsg::ExtendedData { data, ext: _ }) => {
                stderr.push_str(&String::from_utf8_lossy(&data));
            }
            Some(ChannelMsg::ExitStatus { exit_status: code }) => {
                exit_status = Some(code);
            }
            Some(ChannelMsg::Close) | None => break,
            _ => {}
        }
    }

    let _ = handle
        .disconnect(Disconnect::ByApplication, "bye", "")
        .await;
    Ok(ExecResult {
        exit_status,
        stdout,
        stderr,
    })
}

pub fn spawn_session(
    runtime: &tokio::runtime::Handle,
    tab_id: String,
    session: Session,
    jump: Option<Session>,
    initial_cols: u32,
    initial_rows: u32,
) -> (SessionHandle, UnboundedReceiver<SessionEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<SessionEvent>();

    let evt_tx_for_task = evt_tx.clone();
    let join = runtime.spawn(async move {
        if let Err(err) = run_session(
            session,
            jump,
            cmd_rx,
            evt_tx_for_task.clone(),
            initial_cols,
            initial_rows,
        )
        .await
        {
            let _ = evt_tx_for_task.send(SessionEvent::Closed(format!("{err:#}")));
        }
    });

    (
        SessionHandle {
            tab_id,
            commands: cmd_tx,
            join,
        },
        evt_rx,
    )
}

/// 连接并认证一个会话（直连/代理），返回**已认证**的 Handle。用于跳板机：
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
        bail!(t("跳板机认证失败", "jump host authentication failed"));
    }
    Ok(handle)
}

/// 建立到目标的**未认证**传输 Handle。第二项为需在整个会话期间保活的跳板 Handle
/// （直连/代理时为 None；经跳板时为已认证的跳板 Handle，其生命周期须贯穿会话）。
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

    // 跳板机：先连+认证跳板，再在其上开到目标的 direct-tcpip，把通道流当作传输层连目标。
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

    // 直连 / 代理。
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

async fn run_session(
    session: Session,
    jump: Option<Session>,
    mut commands: UnboundedReceiver<SessionCommand>,
    events: UnboundedSender<SessionEvent>,
    initial_cols: u32,
    initial_rows: u32,
) -> Result<()> {
    let _ = events.send(SessionEvent::Status(format!(
        "{} {}@{}:{} ...",
        t("连接中", "Connecting"),
        session.user,
        session.host,
        session.port
    )));

    // 连接建立（传输握手 + 认证 + 开 PTY 通道）整体限时 CONNECT_TIMEOUT：握手阶段
    // russh 的 keepalive / inactivity_timeout 尚未生效，缺这层超时会在"TCP 可达但
    // SSH 无响应"（服务器重启 / 防火墙黑洞 / 半开连接）时永久阻塞，使重连卡在「连接中」、
    // 状态机永远到不了「已断开」而无法恢复。其后长期运行的事件循环不在超时内。
    // keepalive 配置见 keepalive_client_config（与 SFTP 共用，防保活配置分叉）。
    let connect = async {
        let config = Arc::new(keepalive_client_config());
        let (mut handle, jump_keepalive) =
            connect_transport(&session, &jump, config, &events).await?;

        let authed = match session.auth {
            AuthMethod::Password => handle
                .authenticate_password(&session.user, session.password.as_str())
                .await
                .context("password auth failed")?,
            AuthMethod::Key => {
                let key_with_hash = load_private_key_for_auth(&session.private_key_path)?;
                handle
                    .authenticate_publickey(&session.user, key_with_hash)
                    .await
                    .context("publickey auth failed")?
            }
        };

        if !authed {
            let _ = handle
                .disconnect(Disconnect::ByApplication, "auth failed", "")
                .await;
            bail!(t("认证失败", "authentication failed"));
        }

        let channel = handle
            .channel_open_session()
            .await
            .context("open session channel")?;
        channel
            .request_pty(
                true,
                "xterm-256color",
                initial_cols,
                initial_rows,
                0,
                0,
                &[],
            )
            .await
            .context("request PTY")?;
        channel.request_shell(true).await.context("request shell")?;
        Ok::<_, anyhow::Error>((handle, channel, jump_keepalive))
    };

    let (handle, mut channel, _jump_keepalive) =
        match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(result) => result?,
            Err(_) => bail!(t("连接超时", "connection timed out")),
        };

    let _ = events.send(SessionEvent::Connected);
    let _ = events.send(SessionEvent::Status(format!(
        "{} {}@{}",
        t("已连接", "Connected"),
        session.user,
        session.host
    )));

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
    // 保留 fwd_tx 存活至会话结束——它是 fwd_rx 的最后一个 sender。
    // 切勿在此 drop:普通会话 session.tunnels 为空时上面的循环不执行，没有任何
    // acceptor 克隆 sender；一旦 drop 掉这唯一的 fwd_tx，fwd_rx 即刻无 sender，
    // 下面 select! 的 `fwd_rx.recv()` 会立即且永久返回 None，而该分支对 None 是
    // no-op（不 break、不禁用），于是主 select! 每轮瞬间就绪 → 整个 run_session
    // 退化为纯 CPU 忙循环（实测单会话吃满一核、idle wakeups 爆表）。
    // 留住这个 sender 后，无转发请求时 recv() 正常挂起(Pending)，select! 得以休眠。
    // 会话结束(loop break)时本绑定随栈释放，acceptor 的 tx.send 随之失败而退出。
    let _keep_fwd_tx = fwd_tx;

    let mut prompt_injected = false;
    let mut echo_suppressor = EchoSuppressor::new();

    // 先把 PATH 重置为标准系统目录(#27 防护)：监控跑在 exec 通道上，被劫持 PATH
    // (或指向恶意文件的 BASH_ENV)的服务器否则可用任意二进制顶替 awk/cat/df/sleep。
    // 固定 PATH 覆盖 /usr/bin 与 /bin 比逐个硬编码绝对路径更可移植；监控本就尽力而为。
    const MON_CMD: &[u8] = b"PATH=/usr/bin:/bin:/usr/sbin:/sbin; export PATH; while :; do awk '/^cpu /{print}' /proc/stat; awk '/^(MemTotal|MemAvailable|SwapTotal|SwapFree):/{print}' /proc/meminfo; cat /proc/net/dev; echo __DF__; df -kP 2>/dev/null; echo __MSTICK__; sleep 2; done\n";
    let mut mon_channel = match handle.channel_open_session().await {
        Ok(ch) => match ch.exec(true, MON_CMD).await {
            Ok(()) => Some(ch),
            Err(e) => {
                tracing::warn!("monitor exec failed: {e}");
                None
            }
        },
        Err(e) => {
            tracing::warn!("monitor channel open failed: {e}");
            None
        }
    };
    let mut mon_buf = String::new();
    let mut prev_cpu: Option<(u64, u64)> = None;
    let mut prev_net: std::collections::HashMap<String, (u64, u64)> =
        std::collections::HashMap::new();
    let mut prev_net_at = std::time::Instant::now();

    // 唤醒探测计时器：正常运行每 PROBE_INTERVAL tick 一次，仅做一次挂钟时间比较；真正的
    // 连接探测只在检测到「刚从挂起/睡眠恢复」时才发生（见下方 probe_timer 分支）。
    // interval 首个 tick 立即就绪，先消费掉；Skip 防睡眠期间积压的 tick 在唤醒后暴发。
    let mut probe_timer = tokio::time::interval(PROBE_INTERVAL);
    probe_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    probe_timer.tick().await;
    let mut last_probe = std::time::SystemTime::now();

    loop {
        tokio::select! {
            cmd = commands.recv() => {
                match cmd {
                    Some(SessionCommand::RawInput(bytes)) => {
                        tracing::debug!("ssh channel.data len={} bytes", bytes.len());
                        if let Err(err) = channel.data(&bytes[..]).await {
                            let _ = events.send(SessionEvent::Closed(format!("{}: {err}", t("写入失败", "write failed"))));
                            break;
                        }
                    }
                    Some(SessionCommand::Resize(cols, rows)) => {
                        let _ = channel.window_change(cols, rows, 0, 0).await;
                    }
                    Some(SessionCommand::Close) | None => {
                        let _ = channel.eof().await;
                        break;
                    }
                }
            }
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        let text = String::from_utf8_lossy(&data).into_owned();
                        if !prompt_injected && !text.trim().is_empty() {
                            prompt_injected = true;
                            echo_suppressor.arm();
                            let _ = channel.data(format!("{PROMPT_SETUP_TEXT}\r").as_bytes()).await;
                        }
                        let text = echo_suppressor.feed(text);
                        if let Some(cwd) = extract_osc7_path(&text) {
                            tracing::debug!("OSC7 cwd={:?}", cwd);
                            let _ = events.send(SessionEvent::CwdChanged(cwd));
                        }
                        if !text.is_empty() {
                            let _ = events.send(SessionEvent::Output(text));
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, ext: _ }) => {
                        let text = String::from_utf8_lossy(&data).into_owned();
                        let _ = events.send(SessionEvent::Output(text));
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        let _ = events.send(SessionEvent::Status(
                            format!("{} (code {exit_status})", t("远程进程退出", "remote process exited")),
                        ));
                    }
                    Some(ChannelMsg::Close) | None => break,
                    _ => {}
                }
            }
            mon = async {
                match mon_channel.as_mut() {
                    Some(ch) => ch.wait().await,
                    None => std::future::pending().await,
                }
            } => {
                match mon {
                    Some(ChannelMsg::Data { data }) => {
                        mon_buf.push_str(&String::from_utf8_lossy(&data));
                        while let Some(idx) = mon_buf.find("__MSTICK__") {
                            let block = mon_buf[..idx].to_string();
                            let rest = mon_buf[idx + "__MSTICK__".len()..]
                                .trim_start_matches(['\r', '\n'])
                                .to_string();
                            mon_buf = rest;
                            if let Some(stats) = parse_monitor_block(
                                &block,
                                &mut prev_cpu,
                                &mut prev_net,
                                &mut prev_net_at,
                            ) {
                                let _ = events.send(stats);
                            }
                        }
                        // 限制残留(未完成)尾部：只发数据、永不发 __MSTICK__ 标记的
                        // 服务器不得让缓冲无限增长(内存 DoS, #27)。真实样本仅几 KiB，
                        // 1 MiB 是宽松上限。
                        const MON_BUF_CAP: usize = 1 << 20;
                        if mon_buf.len() > MON_BUF_CAP {
                            mon_buf.clear();
                        }
                    }
                    Some(ChannelMsg::Close) | None => {
                        mon_channel = None;
                    }
                    _ => {}
                }
            }
            _ = probe_timer.tick() => {
                // 唤醒探测：进程被挂起（系统睡眠 / App Nap）时 tokio 定时器随之冻结。唤醒后
                // 用挂钟时间跨 tick 的真实流逝识别「刚恢复」——此刻连接可能已半开，而 keepalive
                // 最长还需 180s 才判定（期间 channel.wait() 永久挂起、UI 卡「已连接」、输入无回显
                // 也不重连，见 keepalive_client_config 注释）。于是主动开一条临时通道探测：半开则
                // 收不到 channel-open 确认、PROBE_TIMEOUT 超时即 break，走下方 Closed 流程触发自动
                // 重连，把最长 180s 的「假死」收敛到数秒。
                let now = std::time::SystemTime::now();
                let real_elapsed = now.duration_since(last_probe).unwrap_or_default();
                last_probe = now;
                if resumed_from_suspend(real_elapsed) {
                    match tokio::time::timeout(PROBE_TIMEOUT, handle.channel_open_session()).await {
                        Ok(Ok(_probe_ch)) => {} // 连接仍活；临时通道 drop 时自动关闭
                        _ => break, // 半开 / 超时 / 错误：判定断开，走下方 Closed → 自动重连
                    }
                }
            }
            maybe_fwd = fwd_rx.recv() => {
                if let Some(req) = maybe_fwd {
                    // handle 为 &self 调用，串行开通道；await 期间由 russh 会话任务驱动应答，不阻塞本循环。
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
        }
    }

    for h in tunnel_acceptors {
        h.abort();
    }
    for h in pump_tasks {
        h.abort();
    }

    let _ = handle
        .disconnect(Disconnect::ByApplication, "bye", "")
        .await;
    let _ = events.send(SessionEvent::Closed(
        t("连接已关闭", "connection closed").into(),
    ));
    Ok(())
}

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

fn parse_monitor_block(
    block: &str,
    prev: &mut Option<(u64, u64)>,
    prev_net: &mut std::collections::HashMap<String, (u64, u64)>,
    prev_net_at: &mut std::time::Instant,
) -> Option<SessionEvent> {
    let mut cpu_total = 0u64;
    let mut cpu_idle = 0u64;
    let mut have_cpu = false;
    let mut mem_total = 0u64;
    let mut mem_avail = 0u64;
    let mut swap_total = 0u64;
    let mut swap_free = 0u64;
    let mut net_now: Vec<(String, u64, u64)> = Vec::new();
    let mut disks: Vec<(String, u64, u64)> = Vec::new();
    let mut in_df = false;

    // 限制单次采样接受的网卡/文件系统行数，使恶意服务器无法用伪造行洪流拖垮
    // 解析与侧栏(#27)。真实机器远不及此数。
    const MAX_MON_ENTRIES: usize = 64;

    for line in block.lines() {
        if line == "__DF__" {
            in_df = true;
            continue;
        }
        if in_df {
            if disks.len() < MAX_MON_ENTRIES {
                if let Some(d) = parse_df_line(line) {
                    disks.push(d);
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("cpu ") {
            let nums: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect();
            if nums.len() >= 4 {
                // 饱和运算：服务器可发任意 jiffy 值，普通求和/加法在 debug 下会溢出 panic(#27)。
                cpu_total = nums.iter().copied().fold(0u64, u64::saturating_add);
                cpu_idle = nums[3].saturating_add(nums.get(4).copied().unwrap_or(0));
                have_cpu = true;
            }
        } else if let Some(v) = line.strip_prefix("MemTotal:") {
            mem_total = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            mem_avail = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("SwapTotal:") {
            swap_total = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("SwapFree:") {
            swap_free = parse_meminfo_kib(v);
        } else if net_now.len() < MAX_MON_ENTRIES {
            if let Some((iface, counters)) = parse_net_dev_line(line) {
                net_now.push((iface, counters.0, counters.1));
            }
        }
    }

    let now = std::time::Instant::now();
    let elapsed = now.duration_since(*prev_net_at).as_secs_f64().max(0.001);
    let mut net: Vec<(String, u64, u64)> = Vec::new();
    if !net_now.is_empty() {
        for (iface, rx, tx) in &net_now {
            if let Some((prx, ptx)) = prev_net.get(iface) {
                let rx_bps = (rx.saturating_sub(*prx) as f64 / elapsed) as u64;
                let tx_bps = (tx.saturating_sub(*ptx) as f64 / elapsed) as u64;
                net.push((iface.clone(), rx_bps, tx_bps));
            }
        }
        prev_net.clear();
        for (iface, rx, tx) in net_now {
            prev_net.insert(iface, (rx, tx));
        }
        *prev_net_at = now;
        net.sort_by_key(|e| std::cmp::Reverse(e.1 + e.2));
    }

    let cpu_percent = if have_cpu {
        let result = match *prev {
            Some((ptotal, pidle)) => {
                let dt = cpu_total.saturating_sub(ptotal);
                let di = cpu_idle.saturating_sub(pidle);
                if dt > 0 {
                    (1.0 - di as f32 / dt as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            }
            None => 0.0,
        };
        *prev = Some((cpu_total, cpu_idle));
        result
    } else {
        0.0
    };

    if mem_total == 0 {
        return None;
    }

    Some(SessionEvent::ResourceStats {
        cpu_percent,
        mem_used_kib: mem_total.saturating_sub(mem_avail),
        mem_total_kib: mem_total,
        swap_used_kib: swap_total.saturating_sub(swap_free),
        swap_total_kib: swap_total,
        net,
        disks,
    })
}

fn parse_df_line(line: &str) -> Option<(String, u64, u64)> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 6 || f[0] == "Filesystem" {
        return None;
    }
    let total_kb: u64 = f[1].parse().ok()?;
    let avail_kb: u64 = f[3].parse().ok()?;
    if total_kb == 0 {
        return None;
    }
    // 饱和：服务器可报任意块数，KiB→字节不得在 debug 下溢出 panic(#27)。
    Some((
        f[5..].join(" "),
        avail_kb.saturating_mul(1024),
        total_kb.saturating_mul(1024),
    ))
}

fn parse_meminfo_kib(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|x| x.parse().ok())
        .unwrap_or(0)
}

fn parse_net_dev_line(line: &str) -> Option<(String, (u64, u64))> {
    let (name, rest) = line.split_once(':')?;
    let iface = name.trim();
    if iface.is_empty() || iface == "lo" || iface.contains(' ') {
        return None;
    }
    let nums: Vec<u64> = rest
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    if nums.len() < 9 {
        return None;
    }
    Some((iface.to_string(), (nums[0], nums[8])))
}

/// 注入 cwd 上报：定义 `__lcwd` 发 OSC7（`file://HOST/PWD`），并同时挂到 bash 的
/// `PROMPT_COMMAND` 与 zsh 的 `precmd_functions` —— 两种 shell 都能在每次提示符
/// （含 `cd` 后）上报当前目录，末尾立即调用一次上报初始目录。bash 下
/// `precmd_functions+=` 只是建个无用数组（`2>/dev/null` 兜底），zsh 下
/// `PROMPT_COMMAND` 只是未使用的变量；互不干扰。fish 等非 POSIX shell 不支持。
/// 行首空格：远端启用 HISTCONTROL=ignorespace/ignoreboth 时不记入 history。
const PROMPT_SETUP_TEXT: &str = " __lcwd(){ printf \"\\033]7;file://%s%s\\007\" \"$HOSTNAME\" \"$PWD\"; }; PROMPT_COMMAND=\"__lcwd;$PROMPT_COMMAND\"; precmd_functions+=(__lcwd) 2>/dev/null; __lcwd";

/// 注入命令的回显最迟应出现在其后几个输出包内；超过该字节数仍未命中
/// 说明回显被远端改写（如 zsh 语法高亮插件），放弃过滤、降级为可见。
const ECHO_SCAN_BUDGET: usize = 8192;

#[derive(PartialEq)]
enum EchoSuppressorState {
    Idle,
    Scanning,
    // 命中命令回显后、还差吞掉那一个命令换行的 LF（CRLF 被切到下一包时停在这里等）。
    FoldNewline,
    Done,
}

/// 把注入命令在 PTY 中的回显从输出流里删掉（远端无感知）。
/// 回显可能跨多个 Data 包分片到达，因此流式匹配：尾部疑似命令
/// 前缀的字节先暂扣；超出预算未命中则放弃并原样放行，绝不丢字节。
struct EchoSuppressor {
    state: EchoSuppressorState,
    held: String,
    seen: usize,
}

impl EchoSuppressor {
    fn new() -> Self {
        Self {
            state: EchoSuppressorState::Idle,
            held: String::new(),
            seen: 0,
        }
    }

    /// 注入 PROMPT_SETUP 时调用，开始扫描后续输出中的回显。
    fn arm(&mut self) {
        if self.state == EchoSuppressorState::Idle {
            self.state = EchoSuppressorState::Scanning;
        }
    }

    fn feed(&mut self, chunk: String) -> String {
        match self.state {
            EchoSuppressorState::Idle | EchoSuppressorState::Done => return chunk,
            // 命中命令后，吞掉命令换行残留的 LF（CRLF 被切到下一包的情况）。
            EchoSuppressorState::FoldNewline => return self.fold_newline(chunk),
            EchoSuppressorState::Scanning => {}
        }
        self.seen += chunk.len();
        self.held.push_str(&chunk);

        if let Some(pos) = self.held.find(PROMPT_SETUP_TEXT) {
            let mut out = self.held[..pos].to_string();
            let after = self.held[pos + PROMPT_SETUP_TEXT.len()..].to_string();
            self.held.clear();
            // 删掉命令文本后，再吞掉命令回车换行里的 LF（保留 CR）：bash 执行注入
            // 命令后会重绘一个新提示符，保留 CR 让它回到命令行行首覆盖旧提示符，
            // 终端里就不会多出一行提示符（修复「连接后自动换行一次」）。
            out.push_str(&self.fold_newline(after));
            return out;
        }

        if self.seen > ECHO_SCAN_BUDGET {
            self.state = EchoSuppressorState::Done;
            return std::mem::take(&mut self.held);
        }

        // 暂扣尾部与命令文本头部的最长重叠（纯 ASCII，切点必在 char 边界）
        let keep = longest_overlap(self.held.as_bytes(), PROMPT_SETUP_TEXT.as_bytes());
        let rest = self.held.split_off(self.held.len() - keep);
        std::mem::replace(&mut self.held, rest)
    }

    /// 吞掉 chunk 中第一个 LF（注入命令的回车换行）并完成抑制；CRLF 被切到下一包
    /// （本包还没出现 LF）时停在 FoldNewline 等下一包。CR 不动——它把光标带回命令
    /// 行行首，让命令执行后重绘的新提示符覆盖旧提示符，终端里只留一行提示符。
    fn fold_newline(&mut self, chunk: String) -> String {
        match chunk.find('\n') {
            Some(i) => {
                self.state = EchoSuppressorState::Done;
                let mut out = chunk[..i].to_string();
                out.push_str(&chunk[i + 1..]);
                out
            }
            None => {
                self.state = EchoSuppressorState::FoldNewline;
                chunk
            }
        }
    }
}

/// haystack 尾部与 needle 头部的最长重叠字节数（不含完整匹配）。
fn longest_overlap(haystack: &[u8], needle: &[u8]) -> usize {
    let max = haystack.len().min(needle.len().saturating_sub(1));
    (1..=max)
        .rev()
        .find(|&k| haystack[haystack.len() - k..] == needle[..k])
        .unwrap_or(0)
}

struct ClientHandler {
    host: String,
    port: u16,
}

#[async_trait]
impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        use self::known_hosts::HostKeyStatus;
        match known_hosts::verify(&self.host, self.port, server_public_key) {
            HostKeyStatus::Match => Ok(true),
            HostKeyStatus::Unknown => {
                // 首次见到该主机：记住后放行（TOFU 静默信任首次）。
                let _ = known_hosts::remember(&self.host, self.port, server_public_key);
                Ok(true)
            }
            HostKeyStatus::Mismatch => Ok(false), // 密钥变化（可能 MITM）→ 拒绝握手
        }
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        _data: &[u8],
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[allow(dead_code)]
fn _assert_handle_send() {
    fn takes<T: Send>() {}
    takes::<Handle<ClientHandler>>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keepalive_client_config_keeps_idle_connections_alive() {
        // 终端 PTY 与 SFTP 是同一个 tab 下的两条独立 TCP 长连接，共用本配置。
        // keepalive 缺失会让空闲的那条（用户只用终端时即 SFTP）被
        // inactivity_timeout / 中间 NAT 静默断开，之后 read_dir / read 报
        // session closed——本测试锚定 keepalive 不变量，防止再次回归。
        let cfg = keepalive_client_config();
        assert_eq!(
            cfg.keepalive_interval,
            Some(std::time::Duration::from_secs(60)),
            "必须周期性发送 SSH keepalive 主动保活"
        );
        assert_eq!(cfg.keepalive_max, 3, "连续无响应判定断开的次数");
        assert_eq!(
            cfg.inactivity_timeout,
            Some(std::time::Duration::from_secs(60 * 10)),
            "兜底的无活动断开超时"
        );
    }

    #[test]
    fn connect_timeout_is_bounded() {
        // 连接建立总超时必须存在且在合理范围：太短会误杀慢握手的正常连接，太长则
        // "TCP 可达但 SSH 无响应"时用户要干等。run_session / run_sftp / test_connection
        // 三处共用本常量，锚定防止有人改连接逻辑时漏掉超时（曾致重连永久卡「连接中」）。
        assert!(
            CONNECT_TIMEOUT >= std::time::Duration::from_secs(5)
                && CONNECT_TIMEOUT <= std::time::Duration::from_secs(60),
            "连接超时应在 5~60 秒之间，实际 {CONNECT_TIMEOUT:?}"
        );
    }

    #[test]
    fn resumed_from_suspend_only_fires_after_real_gap() {
        // 唤醒探测的触发判定：正常运行时轮询间隔（含调度抖动）不应判为「刚恢复」，
        // 否则会在健康连接上无谓地反复开临时通道。只有跨轮询的真实流逝出现大跳变
        // （系统睡眠 / App Nap 唤醒）才判定恢复、触发主动探测。
        assert!(!resumed_from_suspend(std::time::Duration::ZERO));
        assert!(!resumed_from_suspend(PROBE_INTERVAL));
        assert!(!resumed_from_suspend(std::time::Duration::from_secs(30)));
        assert!(resumed_from_suspend(RESUME_GAP));
        assert!(resumed_from_suspend(std::time::Duration::from_secs(3600)));
    }

    #[test]
    fn probe_thresholds_are_sane() {
        // 阈值不变量：恢复判定阈值必须显著大于轮询间隔，留足调度抖动余量，避免前台
        // 正常运行被误判为「刚恢复」；主动探测超时必须远小于被动 keepalive 的收敛时间
        // （keepalive_interval 60s × keepalive_max 3 = 180s），否则这层主动探测形同虚设；
        // 探测超时也不应过短，给一次 channel-open 往返留出余量，免得误杀健康连接。
        assert!(
            RESUME_GAP > PROBE_INTERVAL,
            "恢复阈值需大于轮询间隔：RESUME_GAP={RESUME_GAP:?} PROBE_INTERVAL={PROBE_INTERVAL:?}"
        );
        assert!(
            PROBE_TIMEOUT < std::time::Duration::from_secs(180),
            "探测超时需远小于 keepalive 被动收敛的 180s，实际 {PROBE_TIMEOUT:?}"
        );
        assert!(
            PROBE_TIMEOUT >= std::time::Duration::from_secs(2),
            "探测超时需给一次握手往返留余量，实际 {PROBE_TIMEOUT:?}"
        );
    }

    #[tokio::test]
    async fn test_connection_fails_fast_on_closed_port() {
        // 绑定临时端口后立刻释放，对它发起连接必然失败（refused，
        // 或极小概率端口被他人复用导致握手/认证失败）——任何一种都应
        // 走错误路径快速返回，而不是挂到 20 秒超时。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut session = Session::new_empty();
        session.host = "127.0.0.1".into();
        session.port = port;
        session.user = "nobody".into();

        let started = std::time::Instant::now();
        let result = test_connection(session).await;
        assert!(result.is_err());
        assert!(started.elapsed() < CONNECT_TIMEOUT);
    }

    #[test]
    fn extracts_osc7_bel_and_st_paths() {
        assert_eq!(
            extract_osc7_path("\x1b]7;file://host/home/meat%20shell\x07"),
            Some("/home/meat shell".to_string())
        );
        assert_eq!(
            extract_osc7_path("\x1b]7;file:///tmp\x1b\\"),
            Some("/tmp".to_string())
        );
    }

    #[test]
    fn echo_suppressor_removes_injected_command_echo() {
        let mut s = EchoSuppressor::new();
        s.arm();
        let out = s.feed(format!("user@host:~$ {PROMPT_SETUP_TEXT}\r\nrest"));
        // 删命令文本 + 吞掉命令换行的 LF（保留 CR）→ 新提示符回行首覆盖旧提示符。
        assert_eq!(out, "user@host:~$ \rrest");
    }

    #[test]
    fn echo_suppressor_folds_prompt_redraw_into_single_line() {
        // 完整复现连接后的注入序列：提示符 P1 + 注入命令 + 回车(CRLF) +
        // OSC7（PROMPT_COMMAND 首次执行）+ 重绘提示符 P2。抑制后应删命令文本、
        // 吞掉命令换行的 LF、只留 CR —— P2 回行首覆盖 P1，终端只剩一行提示符。
        let mut s = EchoSuppressor::new();
        s.arm();
        let osc7 = "\x1b]7;file://host/home\x07";
        let out = s.feed(format!(
            "ubuntu@LD:~$ {PROMPT_SETUP_TEXT}\r\n{osc7}ubuntu@LD:~$ "
        ));
        assert_eq!(out, format!("ubuntu@LD:~$ \r{osc7}ubuntu@LD:~$ "));
    }

    #[test]
    fn echo_suppressor_folds_newline_split_after_command() {
        // 命令文本落在包尾、CRLF 在下一包：仍要吞掉 LF（保留 CR）。
        let mut s = EchoSuppressor::new();
        s.arm();
        let mut out = String::new();
        out.push_str(&s.feed(format!("ubuntu@LD:~$ {PROMPT_SETUP_TEXT}")));
        out.push_str(&s.feed("\r\nubuntu@LD:~$ ".to_string()));
        assert_eq!(out, "ubuntu@LD:~$ \rubuntu@LD:~$ ");
    }

    #[test]
    fn echo_suppressor_handles_echo_split_across_chunks() {
        let mut s = EchoSuppressor::new();
        s.arm();
        let full = format!("user@host:~$ {PROMPT_SETUP_TEXT}\r\n");
        let (a, rest) = full.split_at(30);
        let (b, c) = rest.split_at(40);
        let mut out = String::new();
        out.push_str(&s.feed(a.to_string()));
        out.push_str(&s.feed(b.to_string()));
        out.push_str(&s.feed(c.to_string()));
        assert_eq!(out, "user@host:~$ \r");
    }

    #[test]
    fn echo_suppressor_keeps_multibyte_output_intact() {
        let mut s = EchoSuppressor::new();
        s.arm();
        let out = s.feed(format!("欢迎使用{PROMPT_SETUP_TEXT}\r\n"));
        assert_eq!(out, "欢迎使用\r");
    }

    #[test]
    fn echo_suppressor_releases_held_prefix_on_mismatch() {
        let mut s = EchoSuppressor::new();
        s.arm();
        // 尾部恰好像命令开头 → 暂扣等下一包
        assert_eq!(s.feed("foo __lcwd".to_string()), "foo");
        // 下一包证明不是回显 → 原样补出
        assert_eq!(s.feed("Z bar".to_string()), " __lcwdZ bar");
    }

    #[test]
    fn echo_suppressor_passes_through_when_idle_or_after_budget() {
        // 未 arm：直通
        let mut s = EchoSuppressor::new();
        assert_eq!(s.feed("hello".to_string()), "hello");

        // 超预算未命中：放弃过滤，已扣数据吐回，之后直通
        let mut s = EchoSuppressor::new();
        s.arm();
        let big = "x".repeat(ECHO_SCAN_BUDGET + 1);
        assert_eq!(s.feed(big.clone()), big);
        assert_eq!(
            s.feed(PROMPT_SETUP_TEXT.to_string()),
            PROMPT_SETUP_TEXT,
            "放弃后即使出现命令文本也不再过滤"
        );
    }

    #[test]
    fn normalises_private_key_path_for_auth() {
        let path = private_key_path_for_auth(r"C:\Users\me\.ssh\id_rsa.pub").unwrap();
        assert_eq!(path, PathBuf::from("C:/Users/me/.ssh/id_rsa"));

        let home = directories::UserDirs::new().unwrap();
        let path = private_key_path_for_auth("~/.ssh/id_ed25519.pub").unwrap();
        assert_eq!(path, home.home_dir().join(".ssh/id_ed25519"));
    }

    #[test]
    fn rejects_empty_private_key_path_before_loading() {
        let err = private_key_path_for_auth("   ").unwrap_err().to_string();
        assert!(
            err.contains("私钥路径为空") || err.contains("private key path is empty"),
            "{err}"
        );
    }

    #[test]
    fn parses_monitor_block_baseline() {
        let mut prev = None;
        let mut prev_net = std::collections::HashMap::new();
        let mut prev_net_at = std::time::Instant::now();
        let block = "cpu  10 0 10 80 0\nMemTotal: 1000 kB\nMemAvailable: 250 kB\nSwapTotal: 100 kB\nSwapFree: 90 kB\n  eth0: 1000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n__DF__\nFilesystem 1024-blocks Used Available Capacity Mounted on\n/dev/x 100 20 80 20% /\n";
        let Some(SessionEvent::ResourceStats {
            cpu_percent,
            mem_used_kib,
            mem_total_kib,
            swap_used_kib,
            disks,
            ..
        }) = parse_monitor_block(block, &mut prev, &mut prev_net, &mut prev_net_at)
        else {
            panic!("expected stats");
        };
        assert_eq!(cpu_percent, 0.0);
        assert_eq!(mem_used_kib, 750);
        assert_eq!(mem_total_kib, 1000);
        assert_eq!(swap_used_kib, 10);
        assert_eq!(disks, vec![("/".to_string(), 80 * 1024, 100 * 1024)]);
    }

    #[test]
    fn parses_monitor_block_deltas_and_sorts_network() {
        let mut prev = Some((100, 80));
        let mut prev_net = std::collections::HashMap::from([
            ("eth0".to_string(), (1_000, 2_000)),
            ("wlan0".to_string(), (10_000, 20_000)),
        ]);
        let mut prev_net_at = std::time::Instant::now() - std::time::Duration::from_secs(2);
        let block = "cpu  30 0 20 90 0\nMemTotal: 2000 kB\nMemAvailable: 1000 kB\nSwapTotal: 100 kB\nSwapFree: 25 kB\n  lo: 999 0 0 0 0 0 0 0 999 0 0 0 0 0 0 0\n  eth0: 3000 0 0 0 0 0 0 0 6000 0 0 0 0 0 0 0\n  wlan0: 13000 0 0 0 0 0 0 0 21000 0 0 0 0 0 0 0\n__DF__\nFilesystem 1024-blocks Used Available Capacity Mounted on\n/dev/x 100 20 80 20% /\n";
        let Some(SessionEvent::ResourceStats {
            cpu_percent,
            mem_used_kib,
            swap_used_kib,
            net,
            ..
        }) = parse_monitor_block(block, &mut prev, &mut prev_net, &mut prev_net_at)
        else {
            panic!("expected stats");
        };
        assert!((cpu_percent - 0.75).abs() < 0.01);
        assert_eq!(mem_used_kib, 1000);
        assert_eq!(swap_used_kib, 75);
        assert_eq!(net.len(), 2);
        assert_eq!(net[0].0, "eth0");
        assert_eq!(net[1].0, "wlan0");
    }
}
