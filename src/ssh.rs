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
            if bytes[end] == 0x07 {
                break;
            } else if bytes[end] == 0x1b && end + 1 < bytes.len() && bytes[end + 1] == b'\\' {
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

pub async fn run_exec(session: Session, command: &str) -> Result<ExecResult> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(60 * 10)),
        ..<_>::default()
    });
    let handler = ClientHandler {};
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
    initial_cols: u32,
    initial_rows: u32,
) -> (SessionHandle, UnboundedReceiver<SessionEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<SessionEvent>();

    let evt_tx_for_task = evt_tx.clone();
    let join = runtime.spawn(async move {
        if let Err(err) = run_session(
            session,
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

async fn run_session(
    session: Session,
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

    let config = Arc::new(client::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(60 * 10)),
        ..<_>::default()
    });
    let handler = ClientHandler {};
    let addr = format!("{}:{}", session.host, session.port);
    let mut handle = match crate::proxy::resolve(&session.proxy) {
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
        let _ = events.send(SessionEvent::Closed(
            t("认证失败", "authentication failed").into(),
        ));
        let _ = handle
            .disconnect(Disconnect::ByApplication, "auth failed", "")
            .await;
        return Ok(());
    }

    let mut channel = handle
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

    let _ = events.send(SessionEvent::Connected);
    let _ = events.send(SessionEvent::Status(format!(
        "{} {}@{}",
        t("已连接", "Connected"),
        session.user,
        session.host
    )));

    let mut prompt_injected = false;
    let mut echo_suppressor = EchoSuppressor::new();

    const MON_CMD: &[u8] = b"while :; do awk '/^cpu /{print}' /proc/stat; awk '/^(MemTotal|MemAvailable|SwapTotal|SwapFree):/{print}' /proc/meminfo; cat /proc/net/dev; echo __DF__; df -kP 2>/dev/null; echo __MSTICK__; sleep 2; done\n";
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
                    }
                    Some(ChannelMsg::Close) | None => {
                        mon_channel = None;
                    }
                    _ => {}
                }
            }
        }
    }

    let _ = handle
        .disconnect(Disconnect::ByApplication, "bye", "")
        .await;
    let _ = events.send(SessionEvent::Closed(
        t("连接已关闭", "connection closed").into(),
    ));
    Ok(())
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

    for line in block.lines() {
        if line == "__DF__" {
            in_df = true;
            continue;
        }
        if in_df {
            if let Some(d) = parse_df_line(line) {
                disks.push(d);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("cpu ") {
            let nums: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect();
            if nums.len() >= 4 {
                cpu_total = nums.iter().sum();
                cpu_idle = nums[3] + nums.get(4).copied().unwrap_or(0);
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
        } else if let Some((iface, counters)) = parse_net_dev_line(line) {
            net_now.push((iface, counters.0, counters.1));
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
        net.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));
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
    Some((f[5..].join(" "), avail_kb * 1024, total_kb * 1024))
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

/// 行首空格：远端 shell 启用 HISTCONTROL=ignorespace/ignoreboth 时不记入 history。
const PROMPT_SETUP_TEXT: &str = " export PROMPT_COMMAND='printf \"\\033]7;file://${HOSTNAME}${PWD}\\007\"' && eval \"$PROMPT_COMMAND\"";

/// 注入命令的回显最迟应出现在其后几个输出包内；超过该字节数仍未命中
/// 说明回显被远端改写（如 zsh 语法高亮插件），放弃过滤、降级为可见。
const ECHO_SCAN_BUDGET: usize = 8192;

#[derive(PartialEq)]
enum EchoSuppressorState {
    Idle,
    Scanning,
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
        if self.state != EchoSuppressorState::Scanning {
            return chunk;
        }
        self.seen += chunk.len();
        self.held.push_str(&chunk);

        if let Some(pos) = self.held.find(PROMPT_SETUP_TEXT) {
            self.state = EchoSuppressorState::Done;
            let mut out = String::with_capacity(self.held.len() - PROMPT_SETUP_TEXT.len());
            out.push_str(&self.held[..pos]);
            out.push_str(&self.held[pos + PROMPT_SETUP_TEXT.len()..]);
            self.held.clear();
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
}

/// haystack 尾部与 needle 头部的最长重叠字节数（不含完整匹配）。
fn longest_overlap(haystack: &[u8], needle: &[u8]) -> usize {
    let max = haystack.len().min(needle.len().saturating_sub(1));
    (1..=max)
        .rev()
        .find(|&k| haystack[haystack.len() - k..] == needle[..k])
        .unwrap_or(0)
}

struct ClientHandler;

#[async_trait]
impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
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
        assert_eq!(out, "user@host:~$ \r\nrest");
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
        assert_eq!(out, "user@host:~$ \r\n");
    }

    #[test]
    fn echo_suppressor_keeps_multibyte_output_intact() {
        let mut s = EchoSuppressor::new();
        s.arm();
        let out = s.feed(format!("欢迎使用{PROMPT_SETUP_TEXT}\r\n"));
        assert_eq!(out, "欢迎使用\r\n");
    }

    #[test]
    fn echo_suppressor_releases_held_prefix_on_mismatch() {
        let mut s = EchoSuppressor::new();
        s.arm();
        // 尾部恰好像命令开头 → 暂扣等下一包
        assert_eq!(s.feed("foo export PROMPT".to_string()), "foo");
        // 下一包证明不是回显 → 原样补出
        assert_eq!(s.feed("_X bar".to_string()), " export PROMPT_X bar");
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
