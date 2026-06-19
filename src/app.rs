slint::include_modules!();

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use tokio::runtime::Runtime;

use crate::config::{AuthMethod, ConfigStore, Secret, Session};
use crate::sftp::{spawn_sftp, SftpHandle};
use crate::ssh::{spawn_session, SessionCommand, SessionEvent, SessionHandle};
use crate::system::{format_bytes_per_sec, SystemSampler, SystemSnapshot};

const NET_HISTORY_LEN: usize = 60;
const MAX_HISTORY: usize = 100_000;

type TermBuffers = Arc<Mutex<HashMap<String, TermBuffer>>>;
type TabStatuses = Arc<Mutex<HashMap<String, TabStatus>>>;
type LocalSnap = Arc<Mutex<SystemSnapshot>>;
type NetHist = Arc<Mutex<Vec<f32>>>;
type SftpHandles = Arc<Mutex<HashMap<String, SftpHandle>>>;
type SftpManualNav = Arc<Mutex<HashMap<String, bool>>>;
type Line = (String, Vec<HistSpan>);

struct AppModels {
    sessions: Rc<VecModel<SessionInfo>>,
    tabs: Rc<VecModel<TabInfo>>,
    terminals: Rc<VecModel<TerminalState>>,
}

/// start_session_io 所需的共享容器集合。connect 与 reconnect 都从这里拿，
/// 保证两条路径装配出的连接在事件转发、状态簿记上完全一致。
#[derive(Clone)]
struct SessionIoCtx {
    runtime: Arc<Runtime>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    sftp_handles: SftpHandles,
    sftp_manual_nav: SftpManualNav,
    bufs: TermBuffers,
    tab_statuses: TabStatuses,
    user_closing: Arc<Mutex<HashSet<String>>>,
    local_snap: LocalSnap,
    local_net_hist: NetHist,
}

struct TermBuffer {
    parser: vt100::Parser,
    find_query: String,
    sel: Option<(u16, u16, u16, u16)>,
    history: Vec<Line>,
    prev: Vec<Line>,
    view_offset: usize,
    displayed_text: Vec<String>,
    csi_state: CsiState,
}

#[derive(Clone, Copy, PartialEq)]
enum CsiState {
    Normal,
    Esc,
    Csi,
}

#[derive(Clone, Default)]
struct TabStatus {
    host: String,
    state: u8,
    /// 本轮断线已发起的自动重连次数；连接成功后清零。
    reconnect_attempts: u8,
    cpu: f32,
    mem_used_kib: u64,
    mem_total_kib: u64,
    swap_used_kib: u64,
    swap_total_kib: u64,
    net: Vec<(String, u64, u64)>,
    selected_iface: String,
    net_hist: Vec<f32>,
    disks: Vec<(String, u64, u64)>,
}

pub fn run(log_buffer: crate::logbuf::LogBuffer) -> anyhow::Result<()> {
    let runtime = Arc::new(Runtime::new()?);
    let store = Rc::new(RefCell::new(ConfigStore::load()?));
    crate::i18n::set_language(store.borrow().language());

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

    let window = AppWindow::new()?;
    crate::i18n::apply_to_slint();
    window.set_lang_en(crate::i18n::is_en());
    window.set_app_version(env!("CARGO_PKG_VERSION").into());
    window.set_build_date(env!("LIBSSH_BUILD_DATE").into());

    // 初始化「全局 CLI」开关状态（仅 Unix；Windows 整行隐藏）。
    #[cfg(unix)]
    {
        window.set_cli_link_supported(true);
        window.set_cli_link_state(crate::system::cli_link_status() as i32);
    }
    #[cfg(not(unix))]
    window.set_cli_link_supported(false);

    let models = initialise_models(&window, &store.borrow());
    let handles: Rc<RefCell<HashMap<String, SessionHandle>>> =
        Rc::new(RefCell::new(HashMap::new()));
    // tab → 连接时的 Session 副本：断线重连用同一配置原地重建连接。
    let tab_sessions: Rc<RefCell<HashMap<String, Session>>> = Rc::new(RefCell::new(HashMap::new()));
    // 全局命令历史 + 每标签的输入行跟踪（粘贴在剪贴板线程 feed，故用 Arc<Mutex>）。
    let cmd_history: Rc<RefCell<crate::history::CommandHistory>> =
        Rc::new(RefCell::new(crate::history::CommandHistory::load_default()));
    let input_trackers: Arc<Mutex<HashMap<String, crate::history::InputTracker>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let sftp_handles: SftpHandles = Arc::new(Mutex::new(HashMap::new()));
    let sftp_manual_nav: SftpManualNav = Arc::new(Mutex::new(HashMap::new()));
    let bufs: TermBuffers = Arc::new(Mutex::new(HashMap::new()));
    let tab_statuses: TabStatuses = Arc::new(Mutex::new(HashMap::new()));
    // 用户主动关闭的标签集合：用于抑制因关闭"未连上/连接中"的标签而误弹"连接失败"框。
    let user_closing_tabs: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let local_snap: LocalSnap = Arc::new(Mutex::new(SystemSnapshot::default()));
    let local_net_hist: NetHist = Arc::new(Mutex::new(vec![0.0; NET_HISTORY_LEN]));
    let last_term_size: Arc<Mutex<(u32, u32)>> = Arc::new(Mutex::new((80, 24)));

    wire_callbacks(
        &window,
        store,
        models,
        runtime,
        handles,
        tab_sessions,
        cmd_history,
        input_trackers,
        sftp_handles.clone(),
        sftp_manual_nav.clone(),
        bufs,
        tab_statuses.clone(),
        user_closing_tabs,
        local_snap.clone(),
        local_net_hist.clone(),
        last_term_size,
    );
    register_file_drop(&window, sftp_handles);
    start_local_sampler(&window, tab_statuses, local_snap, local_net_hist);

    // 「运行日志」浮层：清空 / 复制回调，以及打开时定时把缓冲快照刷到 UI。
    {
        let weak = window.as_weak();
        let lb = log_buffer.clone();
        window.on_log_clear(move || {
            crate::logbuf::clear(&lb);
            if let Some(w) = weak.upgrade() {
                w.set_log_lines(ModelRc::from(Rc::new(VecModel::<SharedString>::default())));
            }
        });
    }
    {
        let lb = log_buffer.clone();
        window.on_log_copy(move || {
            let text = crate::logbuf::snapshot(&lb).join("\n");
            let _ = arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text));
        });
    }
    // 定时刷新：仅在浮层打开时把缓冲快照同步到 UI（开销极小，关闭时只读一个 bool）。
    let log_timer = slint::Timer::default();
    {
        let weak = window.as_weak();
        let lb = log_buffer.clone();
        log_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(800),
            move || {
                if let Some(w) = weak.upgrade() {
                    if w.get_log_open() {
                        let rows: Vec<SharedString> = crate::logbuf::snapshot(&lb)
                            .into_iter()
                            .map(SharedString::from)
                            .collect();
                        w.set_log_lines(ModelRc::from(Rc::new(VecModel::from(rows))));
                    }
                }
            },
        );
    }

    window.run()?;
    Ok(())
}

fn initialise_models(window: &AppWindow, store: &ConfigStore) -> AppModels {
    let sessions_model: Rc<VecModel<SessionInfo>> = Rc::new(VecModel::default());
    sync_sessions_to_model(window, store, &sessions_model, "");
    window.set_sessions(ModelRc::from(sessions_model.clone()));

    let tabs_model: Rc<VecModel<TabInfo>> = Rc::new(VecModel::default());
    tabs_model.push(TabInfo {
        id: "welcome".into(),
        title: crate::i18n::t("连接管理", "Connection manager").into(),
        kind: "welcome".into(),
        connected: false,
    });
    window.set_tabs(ModelRc::from(tabs_model.clone()));
    window.set_active_tab_id("welcome".into());

    let terminals_model: Rc<VecModel<TerminalState>> = Rc::new(VecModel::default());
    window.set_terminals(ModelRc::from(terminals_model.clone()));
    window.set_transfers(empty_model::<TransferInfo>());

    window.set_command_suggestions(empty_model::<SharedString>());
    sync_quick_commands_to_model(store, window);
    window.set_net_top_history(ModelRc::from(Rc::new(VecModel::from(vec![0.0; 60]))));
    window.set_net_bot_history(ModelRc::from(Rc::new(VecModel::from(vec![0.0; 60]))));
    window.set_net_ifaces(empty_model::<SharedString>());
    window.set_disks(empty_model::<DiskInfo>());

    window.set_connection_state(crate::i18n::t("未连接", "Not connected").into());
    window.set_resource_title(crate::i18n::t("本机资源", "Local resources").into());
    window.set_conn_state(0);
    window.set_cpu_percent(0.0);
    window.set_mem_percent(0.0);
    window.set_swap_percent(0.0);
    window.set_mem_detail("0M/0M".into());
    window.set_swap_detail("0M/0M".into());
    window.set_net_top_up("0 B/s".into());
    window.set_net_top_down("0 B/s".into());
    window.set_net_bot_up("0 B/s".into());
    window.set_net_bot_down("0 B/s".into());
    window.set_net_selected("".into());
    window.set_net_show_selector(false);
    window.set_download_dir(store.download_dir().into());
    AppModels {
        sessions: sessions_model,
        tabs: tabs_model,
        terminals: terminals_model,
    }
}

fn start_local_sampler(
    window: &AppWindow,
    statuses: TabStatuses,
    local_snap: LocalSnap,
    local_net_hist: NetHist,
) {
    let sampler = Rc::new(RefCell::new(SystemSampler::new()));

    {
        let snap = sampler.borrow_mut().sample();
        if let Ok(mut local) = local_snap.lock() {
            *local = snap;
        }
        if let Ok(mut hist) = local_net_hist.lock() {
            push_ring(
                &mut hist,
                local_snap.lock().unwrap().net_bytes_per_sec as f32,
            );
        }
        refresh_sidebar(window, &statuses, &local_snap, &local_net_hist);
    }

    let weak = window.as_weak();
    let tick_sampler = sampler.clone();
    let tick_statuses = statuses.clone();
    let tick_local = local_snap.clone();
    let tick_hist = local_net_hist.clone();
    let timer = Timer::default();
    timer.start(
        TimerMode::Repeated,
        SystemSampler::recommended_interval(),
        move || {
            let snap = tick_sampler.borrow_mut().sample();
            if let Ok(mut hist) = tick_hist.lock() {
                push_ring(&mut hist, snap.net_bytes_per_sec as f32);
            }
            if let Ok(mut local) = tick_local.lock() {
                *local = snap;
            }
            if let Some(w) = weak.upgrade() {
                refresh_sidebar(&w, &tick_statuses, &tick_local, &tick_hist);
            }
        },
    );
    Box::leak(Box::new(timer));
}

fn refresh_sidebar(
    window: &AppWindow,
    statuses: &TabStatuses,
    local_snap: &LocalSnap,
    local_net_hist: &NetHist,
) {
    let snap = local_snap.lock().map(|s| s.clone()).unwrap_or_default();
    let hist = local_net_hist
        .lock()
        .map(|h| h.clone())
        .unwrap_or_else(|_| vec![0.0; NET_HISTORY_LEN]);

    window.set_net_bot_up(format_bytes_per_sec(snap.net_tx_per_sec).into());
    window.set_net_bot_down(format_bytes_per_sec(snap.net_rx_per_sec).into());
    window.set_net_bot_history(normalized_model(&hist));

    let active = window.get_active_tab_id().to_string();
    let status = if active == "welcome" {
        None
    } else {
        statuses.lock().ok().and_then(|s| s.get(&active).cloned())
    };

    match status {
        Some(st) if st.state == 1 => {
            window.set_connection_state(
                format!("{} {}", crate::i18n::t("已连接", "Connected"), st.host).into(),
            );
            window.set_resource_title(crate::i18n::t("服务器资源", "Server resources").into());
            window.set_conn_state(1);
            window.set_cpu_percent(st.cpu);
            let mem_percent = if st.mem_total_kib > 0 {
                st.mem_used_kib as f32 / st.mem_total_kib as f32
            } else {
                0.0
            };
            let swap_percent = if st.swap_total_kib > 0 {
                st.swap_used_kib as f32 / st.swap_total_kib as f32
            } else {
                0.0
            };
            window.set_mem_percent(mem_percent);
            window.set_swap_percent(swap_percent);
            window.set_mem_detail(
                format!("{}M/{}M", st.mem_used_kib / 1024, st.mem_total_kib / 1024).into(),
            );
            window.set_swap_detail(
                format!("{}M/{}M", st.swap_used_kib / 1024, st.swap_total_kib / 1024).into(),
            );
            let (iface, rx, tx) = selected_iface(&st);
            window.set_net_top_up(format_bytes_per_sec(tx).into());
            window.set_net_top_down(format_bytes_per_sec(rx).into());
            window.set_net_top_history(normalized_model(&st.net_hist));
            window.set_net_ifaces(ModelRc::from(Rc::new(VecModel::from(
                st.net
                    .iter()
                    .map(|n| SharedString::from(n.0.as_str()))
                    .collect::<Vec<_>>(),
            ))));
            window.set_net_selected(iface.into());
            window.set_net_show_selector(!st.net.is_empty());
            window.set_disks(disk_model(&st.disks));
        }
        Some(st) => {
            let state = if st.state == 2 {
                crate::i18n::t("已断开", "Disconnected")
            } else {
                crate::i18n::t("连接中", "Connecting")
            };
            window.set_connection_state(format!("{state} {}", st.host).into());
            window.set_resource_title(crate::i18n::t("服务器资源", "Server resources").into());
            window.set_conn_state(st.state as i32);
            clear_resource_stats(window);
            set_top_local(window, &snap, &hist);
        }
        None => {
            window.set_connection_state(crate::i18n::t("未连接", "Not connected").into());
            window.set_resource_title(crate::i18n::t("本机资源", "Local resources").into());
            window.set_conn_state(0);
            window.set_cpu_percent(snap.cpu_percent);
            window.set_mem_percent(snap.mem_percent);
            window.set_swap_percent(snap.swap_percent);
            window.set_mem_detail(format!("{}M/{}M", snap.mem_used_mib, snap.mem_total_mib).into());
            window.set_swap_detail(
                format!("{}M/{}M", snap.swap_used_mib, snap.swap_total_mib).into(),
            );
            set_top_local(window, &snap, &hist);
        }
    }
}

fn schedule_sidebar_refresh(
    weak: slint::Weak<AppWindow>,
    statuses: TabStatuses,
    local_snap: LocalSnap,
    local_net_hist: NetHist,
) {
    Timer::single_shot(std::time::Duration::from_millis(1), move || {
        if let Some(w) = weak.upgrade() {
            refresh_sidebar(&w, &statuses, &local_snap, &local_net_hist);
        }
    });
}

fn should_show_connection_failed_alert(previous_state: Option<u8>) -> bool {
    matches!(previous_state, None | Some(0))
}

/// 是否应在会话断开时弹"连接失败"框。
/// `was_user_close` 为 true 表示用户主动关闭了该标签（点 × 关标签），
/// 这种断开是预期内的，永不弹窗；否则沿用"仅未成功连接前才弹"的逻辑。
fn should_alert_on_close(was_user_close: bool, previous_state: Option<u8>) -> bool {
    !was_user_close && should_show_connection_failed_alert(previous_state)
}

/// 第 `attempt` 次自动重连前的退避等待（2s/4s/8s）；attempt 从 1 起，>3 不再重试。
fn auto_reconnect_delay(attempt: u8) -> Option<std::time::Duration> {
    (1..=3)
        .contains(&attempt)
        .then(|| std::time::Duration::from_secs(1u64 << attempt))
}

/// 意外断开是否安排自动重连：仅限「曾成功连接（state==1）、非用户主动关闭、
/// 重试次数未用尽」。首次连接失败（配置/网络错误）重试无意义，交给失败弹窗。
fn should_auto_reconnect(was_user_close: bool, previous_state: Option<u8>, attempts: u8) -> bool {
    !was_user_close && previous_state == Some(1) && attempts < 3
}

fn show_connection_failed_alert(win: &AppWindow, reason: &str) {
    let title = crate::i18n::t("连接失败", "Connection failed");
    let message = reason.trim();
    let message = if message.is_empty() {
        crate::i18n::t("无法连接到服务器，请检查主机、端口、认证方式或网络。", "Unable to connect to the server. Check the host, port, authentication method, or network.").to_string()
    } else {
        message.to_string()
    };

    win.set_alert_title(title.into());
    win.set_alert_message(message.clone().into());
    win.set_alert_open(true);

    let weak = win.as_weak();
    Timer::single_shot(std::time::Duration::from_secs(8), move || {
        if let Some(w) = weak.upgrade() {
            if w.get_alert_message().as_str() == message {
                w.set_alert_open(false);
            }
        }
    });
}

fn set_top_local(window: &AppWindow, snap: &SystemSnapshot, net_hist: &[f32]) {
    window.set_net_top_up(format_bytes_per_sec(snap.net_tx_per_sec).into());
    window.set_net_top_down(format_bytes_per_sec(snap.net_rx_per_sec).into());
    window.set_net_top_history(normalized_model(net_hist));
    window.set_net_ifaces(empty_model::<SharedString>());
    window.set_net_selected("".into());
    window.set_net_show_selector(false);
    window.set_disks(disk_model(&snap.disks));
}

fn clear_resource_stats(window: &AppWindow) {
    window.set_cpu_percent(0.0);
    window.set_mem_percent(0.0);
    window.set_swap_percent(0.0);
    window.set_mem_detail("0M/0M".into());
    window.set_swap_detail("0M/0M".into());
}

fn push_ring(values: &mut Vec<f32>, value: f32) {
    values.push(value);
    if values.len() > NET_HISTORY_LEN {
        values.remove(0);
    }
}

fn normalized_model(values: &[f32]) -> ModelRc<f32> {
    let peak = values.iter().copied().fold(0.0_f32, f32::max).max(1.0);
    let rows: Vec<f32> = values.iter().map(|v| (v / peak).clamp(0.0, 1.0)).collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

fn disk_model(disks: &[(String, u64, u64)]) -> ModelRc<DiskInfo> {
    let rows: Vec<DiskInfo> = disks
        .iter()
        .map(|(mount, avail, total)| {
            let used = total.saturating_sub(*avail);
            let percent = if *total > 0 {
                used as f32 / *total as f32
            } else {
                0.0
            };
            DiskInfo {
                path: mount.clone().into(),
                detail: format!("{}/{}", format_size(*avail), format_size(*total)).into(),
                percent,
            }
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

fn format_size(bytes: u64) -> String {
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

fn empty_model<T: 'static + Clone + Default>() -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::<T>::default()))
}

/// 把 markdown 文本解析成 NoteBlock 模型，供更新弹窗的说明区渲染。
fn notes_blocks_model(md: &str) -> ModelRc<NoteBlock> {
    let rows: Vec<NoteBlock> = crate::markdown::notes_to_blocks(md)
        .into_iter()
        .map(|b| NoteBlock {
            kind: b.kind.into(),
            text: b.text.into(),
            level: b.level,
            marker: b.marker.into(),
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

/// Rebuild the quick-connect model: filter by `filter` (name/host/user), order
/// by group (first-appearance), annotate each row with its group header label,
/// colour index and size, and refresh the header subtitle. Reordering is safe
/// for latency probing because `set_session_latency` writes back by id; already
/// measured latencies are carried forward by id so search doesn't blank them.
fn sync_sessions_to_model(
    win: &AppWindow,
    store: &ConfigStore,
    model: &VecModel<SessionInfo>,
    filter: &str,
) {
    let sessions = store.sessions();

    // Preserve already-measured latencies (keyed by id) across rebuilds.
    let mut prev_lat: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    for i in 0..model.row_count() {
        if let Some(row) = model.row_data(i) {
            prev_lat.insert(row.id.to_string(), row.latency);
        }
    }

    let filter_lc = filter.trim().to_lowercase();
    let matches = |s: &Session| -> bool {
        filter_lc.is_empty()
            || s.name.to_lowercase().contains(&filter_lc)
            || s.host.to_lowercase().contains(&filter_lc)
            || s.user.to_lowercase().contains(&filter_lc)
    };

    // Group order = first appearance over the full set.
    let mut group_order: Vec<String> = Vec::new();
    for s in sessions.iter() {
        if !group_order.iter().any(|g| g == &s.group) {
            group_order.push(s.group.clone());
        }
    }
    let has_named = sessions.iter().any(|s| !s.group.is_empty());

    let mut rows: Vec<SessionInfo> = Vec::new();
    for (gi, g) in group_order.iter().enumerate() {
        let members: Vec<&Session> = sessions
            .iter()
            .filter(|s| &s.group == g && matches(s))
            .collect();
        if members.is_empty() {
            continue;
        }
        // Empty group → "Ungrouped" header, unless every session is ungrouped
        // (then show a flat list with no header at all).
        let label = if g.is_empty() {
            if has_named {
                crate::i18n::t("未分组", "Ungrouped").to_string()
            } else {
                String::new()
            }
        } else {
            g.clone()
        };
        let size = members.len() as i32;
        for s in members {
            rows.push(SessionInfo {
                id: s.id.clone().into(),
                name: s.name.clone().into(),
                host: s.host.clone().into(),
                port: s.port as i32,
                user: s.user.clone().into(),
                auth: s.auth.as_str().into(),
                last_used: s
                    .last_used
                    .clone()
                    .unwrap_or_else(|| "never".to_string())
                    .into(),
                latency: prev_lat.get(s.id.as_str()).copied().unwrap_or(-1),
                group_label: label.clone().into(),
                group_index: gi as i32,
                group_size: size,
            });
        }
    }
    model.set_vec(rows);

    // Header subtitle — counts over the FULL set, independent of the filter.
    let total = sessions.len();
    let click_hint = crate::i18n::t(
        "点击「连接」建立 SSH 会话",
        "click Connect to start an SSH session",
    );
    let subtitle = if has_named {
        format!(
            "{} {} · {} {} · {}",
            total,
            crate::i18n::t("个会话", "sessions"),
            group_order.len(),
            crate::i18n::t("个分组", "groups"),
            click_hint,
        )
    } else {
        format!(
            "{} {} · {}",
            total,
            crate::i18n::t("个会话", "sessions"),
            click_hint
        )
    };
    win.set_session_subtitle(subtitle.into());
}

/// Measure TCP connect time to `host:port`, in milliseconds.
/// `-2` signals unreachable / timed out (caller renders it red).
async fn measure_latency(host: &str, port: u16) -> i32 {
    let start = std::time::Instant::now();
    let connect = tokio::net::TcpStream::connect((host, port));
    match tokio::time::timeout(std::time::Duration::from_secs(3), connect).await {
        Ok(Ok(_stream)) => start.elapsed().as_millis().min(i32::MAX as u128) as i32,
        _ => -2,
    }
}

/// Spawn one async TCP-latency probe per session and write each result back into
/// the quick-connect model on the UI thread as it lands.
fn spawn_latency_probes(
    runtime: &Arc<Runtime>,
    weak: slint::Weak<AppWindow>,
    targets: Vec<(String, String, u16)>,
) {
    for (id, host, port) in targets {
        if host.is_empty() {
            continue;
        }
        let weak = weak.clone();
        runtime.spawn(async move {
            let ms = measure_latency(&host, port).await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    set_session_latency(&w, &id, ms);
                }
            });
        });
    }
}

/// 把 config 里的快捷命令同步到 UI 模型（增删改后整体重建，量小无所谓）。
fn sync_quick_commands_to_model(store: &ConfigStore, window: &AppWindow) {
    let rows: Vec<QuickCmdInfo> = store
        .quick_commands()
        .iter()
        .map(|q| QuickCmdInfo {
            id: q.id.clone().into(),
            name: q.name.clone().into(),
            command: q.command.clone().into(),
        })
        .collect();
    window.set_quick_commands(ModelRc::from(Rc::new(VecModel::from(rows))));
}

/// Update a single quick-connect row's latency by session id.
fn set_session_latency(win: &AppWindow, id: &str, ms: i32) {
    let sessions = win.get_sessions();
    let Some(model) = sessions.as_any().downcast_ref::<VecModel<SessionInfo>>() else {
        return;
    };
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            if row.id.as_str() == id {
                row.latency = ms;
                model.set_row_data(i, row);
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn wire_callbacks(
    window: &AppWindow,
    store: Rc<RefCell<ConfigStore>>,
    models: AppModels,
    runtime: Arc<Runtime>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    tab_sessions: Rc<RefCell<HashMap<String, Session>>>,
    cmd_history: Rc<RefCell<crate::history::CommandHistory>>,
    input_trackers: Arc<Mutex<HashMap<String, crate::history::InputTracker>>>,
    sftp_handles: SftpHandles,
    sftp_manual_nav: SftpManualNav,
    bufs: TermBuffers,
    tab_statuses: TabStatuses,
    user_closing_tabs: Arc<Mutex<HashSet<String>>>,
    local_snap: LocalSnap,
    local_net_hist: NetHist,
    last_term_size: Arc<Mutex<(u32, u32)>>,
) {
    let sessions_model = models.sessions.clone();
    let search_filter: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    {
        let weak = window.as_weak();
        let search_store = store.clone();
        let search_sessions = sessions_model.clone();
        let search_filter = search_filter.clone();
        window.on_search_changed(move |text: SharedString| {
            *search_filter.borrow_mut() = text.to_string();
            if let Some(w) = weak.upgrade() {
                sync_sessions_to_model(
                    &w,
                    &search_store.borrow(),
                    &search_sessions,
                    &search_filter.borrow(),
                );
            }
        });
    }
    let tabs_model = models.tabs.clone();
    let terminals_model = models.terminals.clone();
    let io_ctx = SessionIoCtx {
        runtime: runtime.clone(),
        handles: handles.clone(),
        sftp_handles: sftp_handles.clone(),
        sftp_manual_nav: sftp_manual_nav.clone(),
        bufs: bufs.clone(),
        tab_statuses: tab_statuses.clone(),
        user_closing: user_closing_tabs.clone(),
        local_snap: local_snap.clone(),
        local_net_hist: local_net_hist.clone(),
    };

    // --- Theme: follow the OS appearance (issue #2) -----------------------
    // `Palette.color-scheme` can't be trusted for *reading* the system theme
    // (writing it breaks detection; left unforced it reads back `unknown`).
    // Detect the real setting in Rust off the UI thread and push it into the
    // Theme global every few seconds so "follow system" tracks live changes.
    {
        let weak = window.as_weak();
        runtime.spawn(async move {
            loop {
                let dark = tokio::task::spawn_blocking(crate::system::detect_dark_mode)
                    .await
                    .ok()
                    .flatten();
                if let Some(d) = dark {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.global::<Theme>().set_system_is_dark(d);
                        }
                    });
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        });
    }

    // --- Quick-connect latency probes (issue #3) --------------------------
    let latency_runtime = runtime.clone();
    let latency_store = store.clone();
    let latency_weak = window.as_weak();
    window.on_probe_latencies(move || {
        let targets: Vec<(String, String, u16)> = latency_store
            .borrow()
            .sessions()
            .iter()
            .map(|s| (s.id.clone(), s.host.clone(), s.port))
            .collect();
        spawn_latency_probes(&latency_runtime, latency_weak.clone(), targets);
    });
    window.invoke_probe_latencies();

    let weak = window.as_weak();
    let new_tab_statuses = tab_statuses.clone();
    let new_tab_local = local_snap.clone();
    let new_tab_hist = local_net_hist.clone();
    window.on_new_tab_clicked(move || {
        if let Some(w) = weak.upgrade() {
            w.set_active_tab_id("welcome".into());
        }
        schedule_sidebar_refresh(
            weak.clone(),
            new_tab_statuses.clone(),
            new_tab_local.clone(),
            new_tab_hist.clone(),
        );
    });

    let weak = window.as_weak();
    window.on_session_dialog_cancel(move || {
        if let Some(w) = weak.upgrade() {
            w.set_dialog_open(false);
        }
    });

    // 「测试连接」防串扰代际号：打开对话框或发起新测试都 +1，
    // 在途任务写回前比对，避免旧结果落到新打开的草稿上。
    let test_epoch = Arc::new(AtomicU64::new(0));

    let weak = window.as_weak();
    let new_test_epoch = test_epoch.clone();
    window.on_new_session_clicked(move || {
        if let Some(w) = weak.upgrade() {
            let empty = Session::new_empty();
            w.set_dialog_editing(false);
            w.set_dialog_id(empty.id.into());
            w.set_dialog_name("".into());
            w.set_dialog_host("".into());
            w.set_dialog_port("22".into());
            w.set_dialog_user("root".into());
            w.set_dialog_auth("password".into());
            w.set_dialog_group("".into());
            w.set_dialog_password("".into());
            w.set_dialog_key_path("".into());
            new_test_epoch.fetch_add(1, Ordering::SeqCst);
            w.set_dialog_test_status("idle".into());
            w.set_dialog_test_message("".into());
            w.set_dialog_open(true);
        }
    });

    let weak = window.as_weak();
    let import_store = store.clone();
    let import_sessions = sessions_model.clone();
    let import_filter = search_filter.clone();
    window.on_import_ssh_config(move || {
        let hosts = crate::ssh_config::parse_default();
        let mut added = 0usize;
        if hosts.is_empty() {
            if let Some(w) = weak.upgrade() {
                w.set_ssh_import_hint(
                    crate::i18n::t("未找到 ~/.ssh/config", "no ~/.ssh/config found").into(),
                );
            }
            return;
        }

        {
            let mut s = import_store.borrow_mut();
            for h in hosts {
                let user = if h.user.is_empty() {
                    "root".to_string()
                } else {
                    h.user
                };
                let dup = s
                    .sessions()
                    .iter()
                    .any(|x| x.name == h.alias || (x.host == h.hostname && x.user == user));
                if dup {
                    continue;
                }
                let auth = if h.identity_file.is_empty() {
                    AuthMethod::Password
                } else {
                    AuthMethod::Key
                };
                s.upsert(Session {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: h.alias,
                    host: h.hostname,
                    port: h.port,
                    user,
                    auth,
                    password: Secret::default(),
                    private_key_path: h.identity_file,
                    proxy: String::new(),
                    last_used: None,
                    group: String::new(),
                });
                added += 1;
            }
            if added > 0 {
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
        }

        if let Some(w) = weak.upgrade() {
            sync_sessions_to_model(
                &w,
                &import_store.borrow(),
                &import_sessions,
                &import_filter.borrow(),
            );
            w.invoke_probe_latencies();
            let hint = if added > 0 {
                format!("{} {}", crate::i18n::t("已导入", "imported"), added)
            } else {
                crate::i18n::t("没有新主机可导入", "no new hosts to import").to_string()
            };
            w.set_ssh_import_hint(hint.into());
        }
    });

    let weak = window.as_weak();
    let edit_store = store.clone();
    let edit_test_epoch = test_epoch.clone();
    window.on_edit_session(move |id: SharedString| {
        let id = id.to_string();
        let store = edit_store.borrow();
        let Some(session) = store.get(&id) else {
            return;
        };
        if let Some(w) = weak.upgrade() {
            w.set_dialog_id(session.id.clone().into());
            w.set_dialog_name(session.name.clone().into());
            w.set_dialog_host(session.host.clone().into());
            w.set_dialog_port(session.port.to_string().into());
            w.set_dialog_user(session.user.clone().into());
            w.set_dialog_auth(session.auth.as_str().into());
            w.set_dialog_group(session.group.clone().into());
            w.set_dialog_password("".into());
            w.set_dialog_key_path(session.private_key_path.clone().into());
            edit_test_epoch.fetch_add(1, Ordering::SeqCst);
            w.set_dialog_test_status("idle".into());
            w.set_dialog_test_message("".into());
            w.set_dialog_editing(true);
            w.set_dialog_open(true);
        }
    });

    let weak = window.as_weak();
    let remove_store = store.clone();
    let remove_sessions = sessions_model.clone();
    let remove_filter = search_filter.clone();
    window.on_remove_session(move |id: SharedString| {
        {
            let mut s = remove_store.borrow_mut();
            s.remove(id.as_ref());
            if let Err(err) = s.save() {
                tracing::warn!("failed to save config: {err:#}");
            }
        }
        crate::secrets::keyring_delete(id.as_ref());
        if let Some(w) = weak.upgrade() {
            sync_sessions_to_model(
                &w,
                &remove_store.borrow(),
                &remove_sessions,
                &remove_filter.borrow(),
            );
            let _ = w.get_sessions();
            w.invoke_probe_latencies();
        }
    });

    let weak = window.as_weak();
    let submit_store = store.clone();
    let submit_sessions = sessions_model.clone();
    let submit_filter = search_filter.clone();
    window.on_session_dialog_submit(move |draft: SessionDraft| {
        let id = draft.id.to_string();
        let password = if draft.password.is_empty() {
            submit_store
                .borrow()
                .get(&id)
                .map(|s| s.password.clone())
                .unwrap_or_default()
        } else {
            Secret::new(draft.password.to_string())
        };
        let mut new_session = Session {
            id: if id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                id
            },
            name: if draft.name.is_empty() {
                format!("{}@{}", draft.user, draft.host)
            } else {
                draft.name.to_string()
            },
            host: draft.host.to_string(),
            port: if draft.port <= 0 {
                22
            } else {
                draft.port as u16
            },
            user: draft.user.to_string(),
            auth: AuthMethod::from_str(draft.auth.as_ref()),
            password,
            private_key_path: draft.private_key_path.to_string().replace('\\', "/"),
            proxy: String::new(),
            last_used: None,
            group: draft.group.to_string(),
        };
        // 「记住」开关决定是否持久化：不记住→清空；记住+新输入明文→加密成
        // enc:v1: 密文（加密不可用则不持久化，绝不明文落盘）；记住+未改密码
        //（draft 为空）→沿用上面取出的旧密文，不动。
        if !draft.remember {
            new_session.password = Secret::default();
        } else if !draft.password.is_empty() {
            new_session.password = crate::secrets::encrypt_password(new_session.password.as_str())
                .map(Secret::new)
                .unwrap_or_default();
        }
        {
            let mut s = submit_store.borrow_mut();
            s.upsert(new_session);
            if let Err(err) = s.save() {
                tracing::warn!("failed to save config: {err:#}");
            }
        }
        if let Some(w) = weak.upgrade() {
            sync_sessions_to_model(
                &w,
                &submit_store.borrow(),
                &submit_sessions,
                &submit_filter.borrow(),
            );
            w.set_dialog_open(false);
            w.invoke_probe_latencies();
        }
    });

    let weak = window.as_weak();
    window.on_session_dialog_pick_key(move || {
        let mut dialog = rfd::FileDialog::new()
            .set_title(crate::i18n::t("选择私钥文件", "Choose private key file"));
        if let Some(home) = directories::UserDirs::new().map(|u| u.home_dir().join(".ssh")) {
            if home.is_dir() {
                dialog = dialog.set_directory(home);
            }
        }
        if let Some(file) = dialog.pick_file() {
            let path = file.to_string_lossy().replace('\\', "/");
            if let Some(w) = weak.upgrade() {
                w.set_dialog_key_path(path.into());
            }
        }
    });

    let weak = window.as_weak();
    let test_store = store.clone();
    let test_runtime = runtime.clone();
    let test_epoch_cb = test_epoch.clone();
    window.on_session_dialog_test(move |draft: SessionDraft| {
        let Some(w) = weak.upgrade() else {
            return;
        };
        let my_epoch = test_epoch_cb.fetch_add(1, Ordering::SeqCst) + 1;

        let host = draft.host.to_string().trim().to_string();
        if host.is_empty() {
            w.set_dialog_test_status("fail".into());
            w.set_dialog_test_message(
                crate::i18n::t("请先填写主机地址", "Fill in the host address first").into(),
            );
            return;
        }

        let id = draft.id.to_string();
        let mut session = Session {
            id: id.clone(),
            name: String::new(),
            host,
            port: if draft.port <= 0 {
                22
            } else {
                draft.port as u16
            },
            user: draft.user.to_string(),
            auth: AuthMethod::from_str(draft.auth.as_ref()),
            password: Secret::new(draft.password.to_string()),
            private_key_path: draft.private_key_path.to_string().replace('\\', "/"),
            proxy: String::new(),
            last_used: None,
            group: draft.group.to_string(),
        };
        // 与正式连接保持一致：编辑时密码留空沿用已存密码、proxy 沿用已存配置；
        // json 密码为空再回查 keyring。
        if let Some(saved) = test_store.borrow().get(&id) {
            session.proxy = saved.proxy.clone();
            if session.auth == AuthMethod::Password && session.password.as_str().is_empty() {
                session.password = saved.password.clone();
            }
        }
        crate::secrets::resolve_session_password(&mut session);

        w.set_dialog_test_status("testing".into());
        w.set_dialog_test_message(crate::i18n::t("正在连接…", "Connecting…").into());

        let weak = weak.clone();
        let epoch = test_epoch_cb.clone();
        test_runtime.spawn(async move {
            let outcome = crate::ssh::test_connection(session).await;
            let (status, message) = match outcome {
                Ok(()) => (
                    "ok",
                    crate::i18n::t("连接成功", "Connection successful").to_string(),
                ),
                Err(e) => ("fail", format!("{e:#}")),
            };
            let _ = weak.upgrade_in_event_loop(move |w| {
                // 对话框已重开或发起了新测试 → 此结果过期，不写回。
                if epoch.load(Ordering::SeqCst) == my_epoch {
                    w.set_dialog_test_status(status.into());
                    w.set_dialog_test_message(message.as_str().into());
                }
            });
        });
    });

    let weak = window.as_weak();
    let connect_store = store.clone();
    let connect_tabs = tabs_model.clone();
    let connect_terminals = terminals_model.clone();
    let connect_ctx = io_ctx.clone();
    let connect_tab_sessions = tab_sessions.clone();
    let connect_trackers = input_trackers.clone();
    let connect_last_size = last_term_size.clone();
    window.on_connect_session(move |id: SharedString| {
        let id = id.to_string();
        let weak = weak.clone();
        let connect_store = connect_store.clone();
        let connect_tabs = connect_tabs.clone();
        let connect_terminals = connect_terminals.clone();
        let connect_ctx = connect_ctx.clone();
        let connect_tab_sessions = connect_tab_sessions.clone();
        let connect_trackers = connect_trackers.clone();
        let connect_last_size = connect_last_size.clone();

        Timer::single_shot(std::time::Duration::from_millis(1), move || {
            let mut session = match connect_store.borrow().get(&id).cloned() {
                Some(s) => s,
                None => return,
            };
            // 密码在 json 中为空时回查系统凭据库；之后 spawn_session /
            // spawn_sftp / 重连映射拿到的都是已解析副本。
            crate::secrets::resolve_session_password(&mut session);
            let session = session;
            let tab_id = format!("term-{}", uuid::Uuid::new_v4());
            connect_tab_sessions
                .borrow_mut()
                .insert(tab_id.clone(), session.clone());
            connect_trackers
                .lock()
                .unwrap()
                .insert(tab_id.clone(), Default::default());
            connect_ctx.tab_statuses.lock().unwrap().insert(
                tab_id.clone(),
                TabStatus {
                    host: format!("{}@{}", session.user, session.host),
                    state: 0,
                    ..Default::default()
                },
            );
            connect_tabs.push(TabInfo {
                id: tab_id.clone().into(),
                title: session.name.clone().into(),
                kind: "terminal".into(),
                connected: false,
            });
            connect_terminals.push(TerminalState {
                id: tab_id.clone().into(),
                status: crate::i18n::t("连接中...", "Connecting...").into(),
                spans: empty_model::<TermSpan>(),
                cursor_row: 0,
                cursor_col: 0,
                rows_used: 0,
                is_alt_screen: false,
                conn_lost: false,
                find_matches: empty_model::<TermMatch>(),
                selection: empty_model::<TermMatch>(),
                sftp_path: "/".into(),
                sftp_entries: empty_model::<SftpEntry>(),
                sftp_status: crate::i18n::t("SFTP 连接中...", "SFTP connecting...").into(),
                sftp_loading: true,
                sftp_tree_nodes: empty_model::<SftpTreeNode>(),
            });
            connect_ctx.bufs.lock().unwrap().insert(
                tab_id.clone(),
                TermBuffer {
                    parser: vt100::Parser::new(24, 80, 5000),
                    find_query: String::new(),
                    sel: None,
                    history: Vec::new(),
                    prev: Vec::new(),
                    view_offset: 0,
                    displayed_text: Vec::new(),
                    csi_state: CsiState::Normal,
                },
            );
            connect_ctx
                .sftp_manual_nav
                .lock()
                .unwrap()
                .insert(tab_id.clone(), false);
            if let Some(w) = weak.upgrade() {
                w.set_active_tab_id(tab_id.clone().into());
            }
            schedule_sidebar_refresh(
                weak.clone(),
                connect_ctx.tab_statuses.clone(),
                connect_ctx.local_snap.clone(),
                connect_ctx.local_net_hist.clone(),
            );

            let (initial_cols, initial_rows) = *connect_last_size.lock().unwrap();
            start_session_io(
                weak.clone(),
                &connect_ctx,
                tab_id,
                session,
                initial_cols,
                initial_rows,
            );
        });
    });

    // 底部命令栏：输入变化 → 历史前缀建议；回车/点建议/点快捷命令 → 发送。
    let bar_history = cmd_history.clone();
    let weak = window.as_weak();
    window.on_command_bar_input(move |text: SharedString| {
        let suggestions: Vec<SharedString> = bar_history
            .borrow()
            .suggest(text.as_str(), 8)
            .into_iter()
            .map(SharedString::from)
            .collect();
        if let Some(w) = weak.upgrade() {
            w.set_command_suggestions(ModelRc::from(Rc::new(VecModel::from(suggestions))));
        }
    });

    let bar_handles = handles.clone();
    let bar_history = cmd_history.clone();
    let bar_trackers = input_trackers.clone();
    window.on_command_bar_send(move |tab_id: SharedString, text: SharedString| {
        let cmd = text.trim().to_string();
        if cmd.is_empty() {
            return;
        }
        if let Some(handle) = bar_handles.borrow().get(tab_id.as_str()) {
            // 0x15 = Ctrl+U：先清掉远端行上可能存在的半截输入，再注入完整命令。
            let mut bytes = vec![0x15];
            bytes.extend_from_slice(cmd.as_bytes());
            bytes.push(b'\n');
            handle.send_raw(bytes);
            bar_history.borrow_mut().add(&cmd);
            if let Some(t) = bar_trackers.lock().unwrap().get_mut(tab_id.as_str()) {
                t.reset();
            }
        }
    });

    // 快捷命令：管理对话框预填、保存（新建/编辑）、删除。
    let qcm_store = store.clone();
    let weak = window.as_weak();
    window.on_quick_cmd_manage(move |id: SharedString| {
        if let Some(w) = weak.upgrade() {
            let s = qcm_store.borrow();
            let existing = s.quick_commands().iter().find(|q| q.id == id.as_str());
            w.set_qc_dialog_id(id.clone());
            w.set_qc_dialog_name(existing.map(|q| q.name.clone()).unwrap_or_default().into());
            w.set_qc_dialog_command(
                existing
                    .map(|q| q.command.clone())
                    .unwrap_or_default()
                    .into(),
            );
            w.set_qc_dialog_open(true);
        }
    });

    let qcs_store = store.clone();
    let weak = window.as_weak();
    window.on_quick_cmd_submit(
        move |id: SharedString, name: SharedString, command: SharedString| {
            let name = name.trim().to_string();
            let command = command.trim().to_string();
            if command.is_empty() {
                return;
            }
            {
                let mut s = qcs_store.borrow_mut();
                s.upsert_quick_command(crate::config::QuickCommand {
                    id: if id.is_empty() {
                        uuid::Uuid::new_v4().to_string()
                    } else {
                        id.to_string()
                    },
                    name: if name.is_empty() {
                        command.clone()
                    } else {
                        name
                    },
                    command,
                });
                if let Err(e) = s.save() {
                    tracing::warn!("save quick command failed: {e:#}");
                }
            }
            if let Some(w) = weak.upgrade() {
                sync_quick_commands_to_model(&qcs_store.borrow(), &w);
            }
        },
    );

    let qcd_store = store.clone();
    let weak = window.as_weak();
    window.on_quick_cmd_delete(move |id: SharedString| {
        {
            let mut s = qcd_store.borrow_mut();
            s.remove_quick_command(id.as_str());
            if let Err(e) = s.save() {
                tracing::warn!("save quick command failed: {e:#}");
            }
        }
        if let Some(w) = weak.upgrade() {
            sync_quick_commands_to_model(&qcd_store.borrow(), &w);
        }
    });

    // 就地重连：手动按钮与自动重连定时器共用入口。终端缓冲 / 回看历史 /
    // SFTP 面板状态全部保留，只重建 SSH + SFTP 两条连接。
    let rc_ctx = io_ctx.clone();
    let rc_sessions = tab_sessions.clone();
    let rc_last_size = last_term_size.clone();
    let weak = window.as_weak();
    window.on_reconnect_tab(move |tab_id: SharedString| {
        let tab_id = tab_id.to_string();
        // 仅在「已断开」状态下执行：吞掉手动点击与自动定时器的双重触发。
        {
            let st = rc_ctx.tab_statuses.lock().unwrap();
            if st.get(&tab_id).map(|s| s.state) != Some(2) {
                return;
            }
        }
        let Some(session) = rc_sessions.borrow().get(&tab_id).cloned() else {
            return; // 标签已被关闭
        };
        rc_ctx.user_closing.lock().unwrap().remove(&tab_id);
        if let Some(h) = rc_ctx.handles.borrow_mut().remove(&tab_id) {
            h.close();
        }
        if let Some(h) = rc_ctx.sftp_handles.lock().unwrap().remove(&tab_id) {
            h.close();
        }
        if let Some(st) = rc_ctx.tab_statuses.lock().unwrap().get_mut(&tab_id) {
            st.state = 0;
        }
        if let Some(w) = weak.upgrade() {
            set_terminal_row(&w, &tab_id, |row| {
                row.conn_lost = false;
                row.status = crate::i18n::t("重连中...", "Reconnecting...").into();
            });
        }
        let (cols, rows) = *rc_last_size.lock().unwrap();
        start_session_io(weak.clone(), &rc_ctx, tab_id, session, cols, rows);
    });

    let weak = window.as_weak();
    let close_tabs = tabs_model.clone();
    let close_terminals = terminals_model.clone();
    let close_tab_sessions = tab_sessions.clone();
    let close_trackers = input_trackers.clone();
    let close_handles = handles.clone();
    let close_sftp_handles = sftp_handles.clone();
    let close_sftp_manual_nav = sftp_manual_nav.clone();
    let close_bufs = bufs.clone();
    let close_statuses = tab_statuses.clone();
    let close_local = local_snap.clone();
    let close_hist = local_net_hist.clone();
    let close_user_closing = user_closing_tabs.clone();
    window.on_tab_closed(move |id: SharedString| {
        let id = id.to_string();
        if id == "welcome" {
            return;
        }
        // 标记为"用户主动关闭"，让随后异步到达的 Closed 事件不要误弹"连接失败"框。
        close_user_closing.lock().unwrap().insert(id.clone());
        close_tab_sessions.borrow_mut().remove(&id);
        close_trackers.lock().unwrap().remove(&id);
        if let Some(handle) = close_handles.borrow_mut().remove(&id) {
            handle.close();
        }
        if let Some(handle) = close_sftp_handles.lock().unwrap().remove(&id) {
            handle.close();
        }
        close_sftp_manual_nav.lock().unwrap().remove(&id);
        close_bufs.lock().unwrap().remove(&id);
        close_statuses.lock().unwrap().remove(&id);
        remove_model_row(&close_tabs, &id, |row| row.id.as_str().to_string());
        remove_model_row(&close_terminals, &id, |row| row.id.as_str().to_string());
        if let Some(w) = weak.upgrade() {
            if w.get_active_tab_id().as_str() == id {
                w.set_active_tab_id("welcome".into());
                schedule_sidebar_refresh(
                    weak.clone(),
                    close_statuses.clone(),
                    close_local.clone(),
                    close_hist.clone(),
                );
            }
        }
    });

    let select_tab_statuses = tab_statuses.clone();
    let select_tab_local = local_snap.clone();
    let select_tab_hist = local_net_hist.clone();
    let weak = window.as_weak();
    window.on_tab_selected(move |id: SharedString| {
        if let Some(w) = weak.upgrade() {
            w.set_active_tab_id(id);
        }
        schedule_sidebar_refresh(
            weak.clone(),
            select_tab_statuses.clone(),
            select_tab_local.clone(),
            select_tab_hist.clone(),
        );
    });

    let select_statuses = tab_statuses.clone();
    let select_local = local_snap.clone();
    let select_hist = local_net_hist.clone();
    let weak = window.as_weak();
    window.on_refresh_sidebar(move || {
        schedule_sidebar_refresh(
            weak.clone(),
            select_statuses.clone(),
            select_local.clone(),
            select_hist.clone(),
        );
    });

    let iface_statuses = tab_statuses.clone();
    let iface_local = local_snap.clone();
    let iface_hist = local_net_hist.clone();
    let weak = window.as_weak();
    window.on_select_net_iface(move |iface: SharedString| {
        if let Some(w) = weak.upgrade() {
            let active = w.get_active_tab_id().to_string();
            if let Some(st) = iface_statuses.lock().unwrap().get_mut(&active) {
                st.selected_iface = iface.to_string();
                st.net_hist = vec![0.0; NET_HISTORY_LEN];
            }
            refresh_sidebar(&w, &iface_statuses, &iface_local, &iface_hist);
        }
    });

    let resize_handles = handles.clone();
    let resize_bufs = bufs.clone();
    let resize_last_size = last_term_size.clone();
    let resize_weak = window.as_weak();
    // Debounce PTY resizes. Dragging the SFTP divider (or any rapid relayout)
    // fires a burst of size changes; applying each one floods the remote with
    // SIGWINCH and makes the shell reprint its prompt over and over — the
    // "multiple blank prompt lines" bug. Collapse a burst into one resize ~90ms
    // after the last change; resize_terminal_buffer then no-ops if the grid is
    // unchanged.
    let resize_debounce = Timer::default();
    window.on_terminal_resize(move |tab_id: SharedString, cols_f: f32, rows_f: f32| {
        let tid = tab_id.to_string();
        let handles = resize_handles.clone();
        let bufs = resize_bufs.clone();
        let last_size = resize_last_size.clone();
        let weak = resize_weak.clone();
        resize_debounce.start(
            TimerMode::SingleShot,
            std::time::Duration::from_millis(90),
            move || {
                let (cols, rows, applied) =
                    resize_terminal_buffer(&tid, cols_f, rows_f, &bufs, &last_size);
                if applied {
                    if let Some(handle) = handles.borrow().get(tid.as_str()) {
                        handle.resize(cols, rows);
                    }
                    schedule_terminal_display_rebuild(weak.clone(), bufs.clone(), tid.clone());
                }
            },
        );
    });

    let send_handles = handles.clone();
    let send_bufs = bufs.clone();
    let send_history = cmd_history.clone();
    let send_trackers = input_trackers.clone();
    let last_shift_time: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
    window.on_send_key(
        move |tab_id: SharedString, key: SharedString, ctrl, alt, shift| {
            let key = key.to_string();
            // 输入行跟踪：干净提交的一行进入全局命令历史。
            if !key.is_empty() {
                if let Some(tracker) = send_trackers.lock().unwrap().get_mut(tab_id.as_str()) {
                    if let Some(line) = tracker.feed_key(&key, ctrl, alt) {
                        send_history.borrow_mut().add(&line);
                    }
                }
            }
            let app_cursor = {
                let mut map = send_bufs.lock().unwrap();
                match map.get_mut(tab_id.as_str()) {
                    Some(buf) => {
                        buf.view_offset = 0;
                        buf.parser.screen().application_cursor()
                    }
                    None => false,
                }
            };

            if key.is_empty() && shift && !ctrl && !alt {
                *last_shift_time.lock().unwrap() = Some(std::time::Instant::now());
                return;
            }

            if !ctrl && !alt {
                if let Some(c) = key.chars().next() {
                    let cp = c as u32;
                    let standalone = matches!(cp, 0x08 | 0x09 | 0x0A | 0x0D | 0x1B);
                    if key.chars().count() == 1 && (0x01..=0x1f).contains(&cp) && !standalone {
                        *last_shift_time.lock().unwrap() = Some(std::time::Instant::now());
                        return;
                    }
                }
            }

            #[cfg(windows)]
            if ctrl {
                if let Some(ch) = key.chars().next() {
                    let cp = ch as u32;
                    let always_pass = matches!(cp, 0x09 | 0x0a | 0x0d);
                    if !always_pass
                        && key.chars().count() == 1
                        && (0x01..=0x1a).contains(&cp)
                        && !c0_letter_key_down(cp)
                    {
                        return;
                    }
                }
            }

            if key == "\u{0008}" && !ctrl && !alt {
                if shift {
                    return;
                }
                let shift_recent = last_shift_time
                    .lock()
                    .unwrap()
                    .map(|t| t.elapsed().as_millis() < 1500)
                    .unwrap_or(false);
                if shift_recent {
                    return;
                }
                #[cfg(windows)]
                if !is_vk_back_down() {
                    return;
                }
            }

            let bytes = key_to_pty_bytes(&key, ctrl, alt, app_cursor);
            if !bytes.is_empty() {
                if let Some(handle) = send_handles.borrow().get(tab_id.as_str()) {
                    handle.send_raw(bytes);
                }
            }
        },
    );

    let clear_bufs = bufs.clone();
    let clear_handles = handles.clone();
    let weak = window.as_weak();
    window.on_clear_terminal(move |tab_id: SharedString| {
        let tid = tab_id.to_string();
        if let Some(buf) = clear_bufs.lock().unwrap().get_mut(tab_id.as_str()) {
            let (rows, cols) = buf.parser.screen().size();
            buf.parser = vt100::Parser::new(rows, cols, 5000);
            buf.history.clear();
            buf.prev.clear();
            buf.view_offset = 0;
            buf.displayed_text.clear();
            buf.find_query.clear();
            buf.sel = None;
        }
        if let Some(w) = weak.upgrade() {
            set_terminal_row(&w, &tid, |row| {
                row.spans = empty_model::<TermSpan>();
                row.find_matches = empty_model::<TermMatch>();
                row.selection = empty_model::<TermMatch>();
                row.cursor_row = 0;
                row.cursor_col = 0;
                row.rows_used = 0;
                row.is_alt_screen = false;
            });
        }
        if let Some(handle) = clear_handles.borrow().get(&tid) {
            handle.send_raw(vec![0x0c]);
        }
    });

    let find_bufs = bufs.clone();
    let weak = window.as_weak();
    window.on_find_query_changed(move |tab_id: SharedString, query: SharedString| {
        if let Some(buf) = find_bufs.lock().unwrap().get_mut(tab_id.as_str()) {
            buf.find_query = query.to_string();
        }
        if let Some(w) = weak.upgrade() {
            rebuild_tab_display(&w, &find_bufs, tab_id.as_str());
        }
    });

    let scroll_bufs = bufs.clone();
    // 每个 tab 的滚轮余数累积器（分数行）。满一整行才移动 view_offset，详见
    // accumulate_scroll_lines。
    let scroll_accum: Arc<Mutex<HashMap<String, f32>>> = Arc::new(Mutex::new(HashMap::new()));
    let weak = window.as_weak();
    window.on_terminal_scroll(move |tab_id: SharedString, delta: f32| {
        // delta = 分数行 = slint 端「实际滚动像素 / 行高」。累积去抖：不足一整行
        // (尤其触控板惯性末尾、到顶后边界回弹的亚行反向抖动)只更新余数、不移动
        // 视图，避免被放大成整行回退；满一行才翻整数行。
        let lines = {
            let mut acc = scroll_accum.lock().unwrap();
            accumulate_scroll_lines(acc.entry(tab_id.to_string()).or_insert(0.0), delta)
        };
        if lines == 0 {
            return;
        }
        if let Some(buf) = scroll_bufs.lock().unwrap().get_mut(tab_id.as_str()) {
            let max_off = buf.history.len() as i64;
            let cur = buf.view_offset as i64;
            buf.view_offset = (cur + lines).clamp(0, max_off) as usize;
        }
        if let Some(w) = weak.upgrade() {
            rebuild_tab_display(&w, &scroll_bufs, tab_id.as_str());
        }
    });

    let copy_bufs = bufs.clone();
    window.on_copy_terminal_text(move |tab_id: SharedString| {
        let text = {
            let map = copy_bufs.lock().unwrap();
            match map.get(tab_id.as_str()) {
                Some(buf) => match buf.sel {
                    Some((sr, sc, er, ec)) if (sr, sc) != (er, ec) => {
                        extract_selection(&buf.displayed_text, sr, sc, er, ec)
                    }
                    _ => buf.displayed_text.join("\n"),
                },
                None => String::new(),
            }
        };
        std::thread::spawn(move || {
            if let Err(err) = arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
                tracing::warn!("copy_terminal_text failed: {err}");
            }
        });
    });

    let paste_handles = handles.clone();
    let paste_trackers = input_trackers.clone();
    window.on_paste_from_clipboard(move |tab_id: SharedString| {
        let sender = paste_handles
            .borrow()
            .get(tab_id.as_str())
            .map(|h| h.commands.clone());
        let Some(sender) = sender else {
            return;
        };
        let trackers = paste_trackers.clone();
        let tab_id = tab_id.to_string();
        std::thread::spawn(move || {
            match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
                Ok(text) => {
                    if let Some(t) = trackers.lock().unwrap().get_mut(&tab_id) {
                        t.feed_paste(&text);
                    }
                    let _ = sender.send(SessionCommand::RawInput(
                        normalize_pasted_newlines(&text).into_bytes(),
                    ));
                }
                Err(err) => tracing::warn!("paste_from_clipboard failed: {err}"),
            }
        });
    });

    let select_bufs = bufs.clone();
    let weak = window.as_weak();
    window.on_term_select_start(move |tab_id: SharedString, row, col| {
        let tid = tab_id.to_string();
        {
            let mut map = select_bufs.lock().unwrap();
            let Some(buf) = map.get_mut(&tid) else {
                return;
            };
            let (rows, cols) = buf.parser.screen().size();
            let r = row.clamp(0, rows.saturating_sub(1) as i32) as u16;
            let c = col.clamp(0, cols.saturating_sub(1) as i32) as u16;
            buf.sel = Some((r, c, r, c));
        }
        if let Some(w) = weak.upgrade() {
            rebuild_tab_display(&w, &select_bufs, &tid);
        }
    });

    let select_bufs = bufs.clone();
    let weak = window.as_weak();
    window.on_term_select_update(move |tab_id: SharedString, row, col| {
        let tid = tab_id.to_string();
        {
            let mut map = select_bufs.lock().unwrap();
            let Some(buf) = map.get_mut(&tid) else {
                return;
            };
            let (rows, cols) = buf.parser.screen().size();
            let r = row.clamp(0, rows.saturating_sub(1) as i32) as u16;
            let c = col.clamp(0, cols.saturating_sub(1) as i32) as u16;
            if let Some((sr, sc, _, _)) = buf.sel {
                buf.sel = Some((sr, sc, r, c));
            }
        }
        if let Some(w) = weak.upgrade() {
            rebuild_tab_display(&w, &select_bufs, &tid);
        }
    });

    let select_bufs = bufs.clone();
    let weak = window.as_weak();
    window.on_term_select_end(move |tab_id: SharedString| {
        let tid = tab_id.to_string();
        let selected = {
            let mut map = select_bufs.lock().unwrap();
            let Some(buf) = map.get_mut(&tid) else {
                return;
            };
            match buf.sel {
                Some((sr, sc, er, ec)) if (sr, sc) != (er, ec) => {
                    Some(extract_selection(&buf.displayed_text, sr, sc, er, ec))
                }
                _ => {
                    buf.sel = None;
                    None
                }
            }
        };
        if let Some(text) = selected.filter(|s| !s.is_empty()) {
            std::thread::spawn(move || {
                let _ = arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text));
            });
        }
        if let Some(w) = weak.upgrade() {
            rebuild_tab_display(&w, &select_bufs, &tid);
        }
    });

    let select_bufs = bufs.clone();
    let weak = window.as_weak();
    window.on_term_select_autoscroll(move |tab_id: SharedString, dir| {
        let tid = tab_id.to_string();
        {
            let mut map = select_bufs.lock().unwrap();
            let Some(buf) = map.get_mut(&tid) else {
                return;
            };
            if buf.parser.screen().alternate_screen() {
                return;
            }
            let rows = buf.parser.screen().size().0;
            let last = rows.saturating_sub(1);
            let max_off = buf.history.len();
            let Some((sr, sc, _, ec)) = buf.sel else {
                return;
            };
            if dir < 0 {
                let new_off = (buf.view_offset + 2).min(max_off);
                let delta = new_off - buf.view_offset;
                if delta == 0 {
                    return;
                }
                buf.view_offset = new_off;
                let nsr = ((sr as usize) + delta).min(last as usize) as u16;
                buf.sel = Some((nsr, sc, 0, ec));
            } else if dir > 0 {
                let new_off = buf.view_offset.saturating_sub(2);
                let delta = buf.view_offset - new_off;
                if delta == 0 {
                    return;
                }
                buf.view_offset = new_off;
                let nsr = (sr as i32 - delta as i32).max(0) as u16;
                buf.sel = Some((nsr, sc, last, ec));
            }
        }
        if let Some(w) = weak.upgrade() {
            rebuild_tab_display(&w, &select_bufs, &tid);
        }
    });

    let weak = window.as_weak();
    let lang_store = store.clone();
    window.on_set_language(move |code| {
        crate::i18n::set_language(code.as_str());
        {
            let mut s = lang_store.borrow_mut();
            s.set_language(crate::i18n::current_code().to_string());
            if let Err(err) = s.save() {
                tracing::warn!("failed to save config: {err:#}");
            }
        }
        if let Some(w) = weak.upgrade() {
            w.set_lang_en(crate::i18n::is_en());
            w.set_connection_state(crate::i18n::t("未连接", "Not connected").into());
            w.set_resource_title(crate::i18n::t("本机资源", "Local resources").into());
        }
    });

    // 「全局 CLI」开关：建链/重链/移除 ~/.local/bin/LibSSH（仅 Unix）。
    #[cfg(unix)]
    {
        let weak = window.as_weak();
        window.on_toggle_cli_link(move || {
            let Some(w) = weak.upgrade() else {
                return;
            };
            // 双语「失败」反馈（disable / enable 两个 Err 分支共用）。
            let fmt_err = |err: &anyhow::Error| -> String {
                if crate::i18n::is_en() {
                    format!("Failed: {err}")
                } else {
                    format!("失败：{err}")
                }
            };
            // 1=Linked → 移除；0=未链接 / 2=Stale（过期链接）→ 重建。
            let linked = w.get_cli_link_state() == 1;
            let feedback = if linked {
                match crate::system::disable_cli_link() {
                    Ok(()) => {
                        crate::i18n::t("已移除全局 CLI 链接", "Global CLI link removed").to_string()
                    }
                    Err(err) => fmt_err(&err),
                }
            } else {
                match crate::system::enable_cli_link() {
                    Ok(outcome) => {
                        let path = outcome.link_path.display();
                        if outcome.in_path {
                            if crate::i18n::is_en() {
                                format!("Linked at {path}")
                            } else {
                                format!("已链接到 {path}")
                            }
                        } else if crate::i18n::is_en() {
                            format!(
                                "Linked at {path}\n~/.local/bin is not on PATH. Add to your shell profile (e.g. ~/.zshrc):\nexport PATH=\"$HOME/.local/bin:$PATH\""
                            )
                        } else {
                            format!(
                                "已链接到 {path}\n~/.local/bin 不在 PATH，请加入你的 shell 配置（如 ~/.zshrc）：\nexport PATH=\"$HOME/.local/bin:$PATH\""
                            )
                        }
                    }
                    Err(err) => fmt_err(&err),
                }
            };
            // 重新读取文件系统状态作为真相源，并写回反馈。
            w.set_cli_link_state(crate::system::cli_link_status() as i32);
            w.set_cli_link_feedback(feedback.into());
        });
    }

    let weak = window.as_weak();
    let download_store = store.clone();
    window.on_pick_download_dir(move || {
        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
            let dir = folder.to_string_lossy().to_string();
            {
                let mut s = download_store.borrow_mut();
                s.set_download_dir(dir.clone());
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_download_dir(dir.into());
            }
        }
    });

    let weak = window.as_weak();
    window.on_open_download_dir(move || {
        let Some(w) = weak.upgrade() else {
            return;
        };
        let dir = w.get_download_dir().to_string();
        if dir.is_empty() {
            return;
        }
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("explorer").arg(&dir).spawn();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
        }
    });

    let weak = window.as_weak();
    window.on_clear_transfers(move || {
        if let Some(w) = weak.upgrade() {
            if let Some(model) = w
                .get_transfers()
                .as_any()
                .downcast_ref::<VecModel<TransferInfo>>()
            {
                model.set_vec(Vec::new());
            }
        }
    });

    let nav_sftp = sftp_handles.clone();
    let nav_manual = sftp_manual_nav.clone();
    let weak = window.as_weak();
    window.on_sftp_navigate(move |tab_id: SharedString, path: SharedString| {
        let tab_id = tab_id.to_string();
        let target = if path.as_str() == ".." {
            let current = weak
                .upgrade()
                .map(|w| terminal_sftp_path(&w, &tab_id))
                .unwrap_or_else(|| "/".to_string());
            parent_path(&current)
        } else {
            path.to_string()
        };
        nav_manual.lock().unwrap().insert(tab_id.clone(), true);
        if let Ok(handles) = nav_sftp.lock() {
            if let Some(handle) = handles.get(&tab_id) {
                // 手动导航也亮起加载态（行内目录 spinner / 顶部 Loading 行靠它
                // 触发）。只在确实发出 list_dir 时设置，加载终结事件
                // （SftpEntries / SftpLoadFailed）会统一把它复位。
                if let Some(w) = weak.upgrade() {
                    set_terminal_row(&w, &tab_id, |row| row.sftp_loading = true);
                }
                handle.list_dir(target);
            }
        }
    });

    let dl_sftp = sftp_handles.clone();
    let weak = window.as_weak();
    window.on_sftp_download(move |tab_id: SharedString, remote_path: SharedString| {
        let tab_id = tab_id.to_string();
        let remote_path = remote_path.to_string();
        let preset = weak
            .upgrade()
            .map(|w| w.get_download_dir().to_string())
            .unwrap_or_default();
        if !preset.is_empty() {
            if let Ok(handles) = dl_sftp.lock() {
                if let Some(handle) = handles.get(&tab_id) {
                    handle.download(remote_path, preset);
                }
            }
            return;
        }
        let dl_sftp = dl_sftp.clone();
        std::thread::spawn(move || {
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                if let Ok(handles) = dl_sftp.lock() {
                    if let Some(handle) = handles.get(&tab_id) {
                        handle.download(remote_path, folder.to_string_lossy().to_string());
                    }
                }
            }
        });
    });

    let up_sftp = sftp_handles.clone();
    window.on_sftp_upload_clicked(move |tab_id: SharedString, remote_dir: SharedString| {
        let tab_id = tab_id.to_string();
        let remote_dir = remote_dir.to_string();
        let up_sftp = up_sftp.clone();
        std::thread::spawn(move || {
            if let Some(file) = rfd::FileDialog::new().pick_file() {
                if let Ok(handles) = up_sftp.lock() {
                    if let Some(handle) = handles.get(&tab_id) {
                        handle.upload(file.to_string_lossy().to_string(), remote_dir);
                    }
                }
            }
        });
    });

    let refresh_sftp = sftp_handles.clone();
    window.on_sftp_refresh(move |tab_id: SharedString, path: SharedString| {
        if let Ok(handles) = refresh_sftp.lock() {
            if let Some(handle) = handles.get(tab_id.as_str()) {
                handle.list_dir(path.to_string());
            }
        }
    });

    let tree_sftp = sftp_handles.clone();
    let tree_manual = sftp_manual_nav.clone();
    window.on_sftp_tree_expand(move |tab_id: SharedString, path: SharedString| {
        let tab_id = tab_id.to_string();
        let path = path.to_string();
        tree_manual.lock().unwrap().insert(tab_id.clone(), true);
        if let Ok(handles) = tree_sftp.lock() {
            if let Some(handle) = handles.get(&tab_id) {
                handle.toggle_tree_node(path.clone());
                handle.list_dir(path);
            }
        }
    });

    let upfolder_sftp = sftp_handles.clone();
    window.on_sftp_upload_folder_clicked(move |tab_id: SharedString, remote_dir: SharedString| {
        let tab_id = tab_id.to_string();
        let remote_dir = remote_dir.to_string();
        let upfolder_sftp = upfolder_sftp.clone();
        std::thread::spawn(move || {
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                if let Ok(handles) = upfolder_sftp.lock() {
                    if let Some(handle) = handles.get(&tab_id) {
                        handle.upload_dir(folder.to_string_lossy().to_string(), remote_dir);
                    }
                }
            }
        });
    });

    let copy_path_weak = window.as_weak();
    window.on_sftp_copy_path(move |tab_id: SharedString, path: SharedString| {
        let ok = arboard::Clipboard::new()
            .and_then(|mut cb| cb.set_text(path.to_string()))
            .is_ok();
        if let Some(w) = copy_path_weak.upgrade() {
            let msg: SharedString = if ok {
                format!("{}: {}", crate::i18n::t("已复制路径", "Path copied"), path).into()
            } else {
                crate::i18n::t("复制路径失败", "Copy path failed").into()
            };
            set_terminal_row(&w, tab_id.as_str(), |row| {
                row.sftp_status = msg.clone();
            });
        }
    });

    let rename_sftp = sftp_handles.clone();
    window.on_sftp_rename(
        move |tab_id: SharedString, path: SharedString, new_name: SharedString| {
            if let Ok(handles) = rename_sftp.lock() {
                if let Some(handle) = handles.get(tab_id.as_str()) {
                    handle.rename(path.to_string(), new_name.to_string());
                }
            }
        },
    );

    let mkfile_sftp = sftp_handles.clone();
    window.on_sftp_create_file(
        move |tab_id: SharedString, dir: SharedString, name: SharedString| {
            if let Ok(handles) = mkfile_sftp.lock() {
                if let Some(handle) = handles.get(tab_id.as_str()) {
                    handle.create_file(dir.to_string(), name.to_string());
                }
            }
        },
    );

    let mkdir_sftp = sftp_handles.clone();
    window.on_sftp_create_dir(
        move |tab_id: SharedString, dir: SharedString, name: SharedString| {
            if let Ok(handles) = mkdir_sftp.lock() {
                if let Some(handle) = handles.get(tab_id.as_str()) {
                    handle.create_dir(dir.to_string(), name.to_string());
                }
            }
        },
    );

    let delete_sftp = sftp_handles.clone();
    window.on_sftp_delete(move |tab_id: SharedString, path: SharedString| {
        if let Ok(handles) = delete_sftp.lock() {
            if let Some(handle) = handles.get(tab_id.as_str()) {
                handle.delete(path.to_string());
            }
        }
    });

    let edit_sftp = sftp_handles.clone();
    window.on_sftp_edit(move |tab_id: SharedString, path: SharedString| {
        if let Ok(handles) = edit_sftp.lock() {
            if let Some(handle) = handles.get(tab_id.as_str()) {
                handle.read_file(path.to_string());
            }
        }
    });

    let save_sftp = sftp_handles.clone();
    window.on_editor_save(
        move |tab_id: SharedString, remote: SharedString, content: SharedString| {
            if let Ok(handles) = save_sftp.lock() {
                if let Some(handle) = handles.get(tab_id.as_str()) {
                    handle.write_file(remote.to_string(), content.to_string());
                }
            }
        },
    );

    let editor_close_weak = window.as_weak();
    window.on_editor_close(move || {
        if let Some(w) = editor_close_weak.upgrade() {
            w.set_editor_open(false);
            w.set_editor_content("".into());
            w.set_editor_dirty(false);
            w.set_editor_confirm_discard(false);
        }
    });

    // ===== 自动更新接线 =====
    let pending_release: Arc<Mutex<Option<crate::updater::ReleaseInfo>>> =
        Arc::new(Mutex::new(None));
    let pending_helper: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::new(Mutex::new(None));
    // 当前下载的取消开关（点"取消"时置位，download_and_verify 轮询它中止）。
    let pending_cancel: Arc<Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>> =
        Arc::new(Mutex::new(None));

    fn show_release(w: &AppWindow, rel: &crate::updater::ReleaseInfo) {
        w.set_update_current(env!("CARGO_PKG_VERSION").into());
        w.set_update_version(rel.version.to_string().into());
        w.set_update_note_blocks(notes_blocks_model(&rel.notes));
        w.set_update_phase("prompt".into());
        w.set_update_progress(0.0);
        w.set_update_guided(false);
        w.set_update_error("".into());
        w.set_update_open(true);
    }

    fn updates_dir() -> std::path::PathBuf {
        directories::ProjectDirs::from("dev", "LibSSH", "LibSSH")
            .map(|d| d.cache_dir().join("updates"))
            .unwrap_or_else(|| std::env::temp_dir().join("LibSSH-updates"))
    }

    // --- 启动自动检查（节流 24h）---
    {
        let do_check = {
            let s = store.borrow();
            s.auto_check_update()
                && match s.last_update_check() {
                    Some(last) => chrono::Utc::now().timestamp() - last >= 24 * 3600,
                    None => true,
                }
        };
        if do_check {
            {
                let mut s = store.borrow_mut();
                s.set_last_update_check(Some(chrono::Utc::now().timestamp()));
                let _ = s.save();
            }
            let skipped = store.borrow().skipped_version().map(|s| s.to_string());
            let weak = window.as_weak();
            let pending = pending_release.clone();
            runtime.spawn(async move {
                match crate::updater::check_for_update(env!("CARGO_PKG_VERSION"), skipped, false)
                    .await
                {
                    Ok(Some(rel)) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                *pending.lock().unwrap() = Some(rel.clone());
                                show_release(&w, &rel);
                            }
                        });
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("auto update check failed: {e:#}"),
                }
            });
        }
    }

    // --- 手动检查（关于页按钮）---
    {
        let weak = window.as_weak();
        let store = store.clone();
        let runtime = runtime.clone();
        let pending = pending_release.clone();
        window.on_check_update_manual(move || {
            let skipped = store.borrow().skipped_version().map(|s| s.to_string());
            let weak = weak.clone();
            let pending = pending.clone();
            runtime.spawn(async move {
                let res =
                    crate::updater::check_for_update(env!("CARGO_PKG_VERSION"), skipped, true)
                        .await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        match res {
                            Ok(Some(rel)) => {
                                *pending.lock().unwrap() = Some(rel.clone());
                                show_release(&w, &rel);
                            }
                            Ok(None) => {
                                w.set_alert_title(
                                    crate::i18n::t("检查更新", "Check for updates").into(),
                                );
                                w.set_alert_message(
                                    crate::i18n::t(
                                        "已是最新版本。",
                                        "You are on the latest version.",
                                    )
                                    .into(),
                                );
                                w.set_alert_open(true);
                            }
                            Err(_) => {
                                w.set_alert_title(
                                    crate::i18n::t("检查更新", "Check for updates").into(),
                                );
                                w.set_alert_message(
                                    crate::i18n::t("检查更新失败。", "Update check failed.").into(),
                                );
                                w.set_alert_open(true);
                            }
                        }
                    }
                });
            });
        });
    }

    // --- 立即更新：下载 → 校验 → 安装 ---
    {
        let weak = window.as_weak();
        let runtime = runtime.clone();
        let pending = pending_release.clone();
        let helper = pending_helper.clone();
        let cancel_slot = pending_cancel.clone();
        window.on_update_confirm(move || {
            let Some(rel) = pending.lock().unwrap().clone() else { return; };
            // 新建本次下载的取消开关，登记到共享槽供"取消"按钮置位。
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            *cancel_slot.lock().unwrap() = Some(cancel.clone());
            if let Some(w) = weak.upgrade() {
                w.set_update_phase("downloading".into());
                w.set_update_progress(0.0);
            }
            let weak = weak.clone();
            let helper = helper.clone();
            let cancel_dl = cancel.clone();
            runtime.spawn(async move {
                let prog_weak = weak.clone();
                let last_pct = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
                let on_progress = move |done: u64, total: u64| {
                    let pct = (done * 100).checked_div(total).unwrap_or(0);
                    if last_pct.swap(pct, std::sync::atomic::Ordering::Relaxed) != pct {
                        let prog_weak = prog_weak.clone();
                        let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = prog_weak.upgrade() {
                                w.set_update_progress(frac);
                            }
                        });
                    }
                };

                let dl =
                    crate::updater::download_and_verify(&rel, &updates_dir(), cancel_dl.clone(), on_progress)
                        .await;

                match dl {
                    Ok(dmg) => {
                        let vweak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = vweak.upgrade() { w.set_update_phase("installing".into()); }
                        });
                        let install = tokio::task::spawn_blocking(move || crate::updater::install(&dmg)).await;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                match install {
                                    Ok(Ok(crate::updater::InstallOutcome::ReadyToRestart { helper_script })) => {
                                        *helper.lock().unwrap() = Some(helper_script);
                                        w.set_update_guided(false);
                                        w.set_update_phase("ready".into());
                                    }
                                    Ok(Ok(crate::updater::InstallOutcome::GuidedManual)) => {
                                        *helper.lock().unwrap() = None;
                                        w.set_update_guided(true);
                                        w.set_update_note_blocks(notes_blocks_model(crate::i18n::t(
                                            "请将 LibSSH 拖到「应用程序」文件夹以完成更新。",
                                            "Drag LibSSH into the Applications folder to finish updating.",
                                        )));
                                        w.set_update_phase("ready".into());
                                    }
                                    _ => {
                                        w.set_update_error(crate::i18n::t("安装失败。", "Install failed.").into());
                                        w.set_update_phase("error".into());
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        // 区分用户取消与真实失败：取消时对话框已由"取消"按钮关闭，静默即可。
                        if cancel_dl.load(std::sync::atomic::Ordering::Relaxed) {
                            tracing::info!("update download cancelled by user");
                        } else {
                            tracing::warn!("download failed: {e:#}");
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_update_error(crate::i18n::t("下载或校验失败。", "Download or verification failed.").into());
                                    w.set_update_phase("error".into());
                                }
                            });
                        }
                    }
                }
            });
        });
    }

    // --- 稍后 ---
    {
        let weak = window.as_weak();
        window.on_update_later(move || {
            if let Some(w) = weak.upgrade() {
                w.set_update_open(false);
            }
        });
    }

    // --- 取消下载（downloading 阶段）---
    {
        let weak = window.as_weak();
        let cancel_slot = pending_cancel.clone();
        window.on_update_cancel(move || {
            if let Some(c) = cancel_slot.lock().unwrap().as_ref() {
                c.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(w) = weak.upgrade() {
                w.set_update_progress(0.0);
                w.set_update_phase("prompt".into()); // 回到 prompt，可重新发起
                w.set_update_open(false); // 关闭对话框
            }
        });
    }

    // --- 跳过此版本 ---
    {
        let weak = window.as_weak();
        let store = store.clone();
        let pending = pending_release.clone();
        window.on_update_skip(move || {
            if let Some(rel) = pending.lock().unwrap().clone() {
                let mut s = store.borrow_mut();
                s.set_skipped_version(Some(rel.tag.clone()));
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_update_open(false);
            }
        });
    }

    // --- 重启 / 完成 ---
    {
        let weak = window.as_weak();
        let helper = pending_helper.clone();
        window.on_update_restart(move || {
            if let Some(_script) = helper.lock().unwrap().clone() {
                #[cfg(target_os = "macos")]
                crate::updater::run_helper_and_exit(&_script); // 不返回，进程被替换
            }
            if let Some(w) = weak.upgrade() {
                w.set_update_open(false);
            }
        });
    }

    // --- 重试 ---
    {
        let weak = window.as_weak();
        window.on_update_retry(move || {
            if let Some(w) = weak.upgrade() {
                w.set_update_error("".into());
                w.set_update_phase("prompt".into());
            }
        });
    }

    // --- 去发布页 ---
    {
        window.on_update_open_release(move || {
            let url = "https://github.com/qdlibra/LibSSH/releases/latest";
            #[cfg(target_os = "macos")]
            let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
            #[cfg(target_os = "windows")]
            let _ = std::process::Command::new("cmd")
                .args(["/C", "start", url])
                .spawn();
            #[cfg(all(unix, not(target_os = "macos")))]
            let _ = std::process::Command::new("xdg-open").arg(url).spawn();
        });
    }

    // --- 关于对话框：打开 github 仓库主页 ---
    {
        window.on_open_github(move || {
            let url = "https://github.com/qdlibra/LibSSH";
            #[cfg(target_os = "macos")]
            let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
            #[cfg(target_os = "windows")]
            let _ = std::process::Command::new("cmd")
                .args(["/C", "start", url])
                .spawn();
            #[cfg(all(unix, not(target_os = "macos")))]
            let _ = std::process::Command::new("xdg-open").arg(url).spawn();
        });
    }
}

fn remove_model_row<T: Clone + 'static>(
    model: &VecModel<T>,
    id: &str,
    get_id: impl Fn(T) -> String,
) {
    let mut idx = None;
    for i in 0..model.row_count() {
        if model
            .row_data(i)
            .map(|row| get_id(row) == id)
            .unwrap_or(false)
        {
            idx = Some(i);
            break;
        }
    }
    if let Some(i) = idx {
        model.remove(i);
    }
}

fn terminal_sftp_path(win: &AppWindow, tab_id: &str) -> String {
    let terminals = win.get_terminals();
    let Some(model) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
        return "/".to_string();
    };
    for i in 0..model.row_count() {
        if let Some(row) = model.row_data(i) {
            if row.id.as_str() == tab_id {
                return row.sftp_path.to_string();
            }
        }
    }
    "/".to_string()
}

fn parent_path(path: &str) -> String {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

fn register_file_drop(window: &AppWindow, sftp_handles: SftpHandles) {
    use i_slint_backend_winit::winit::event::WindowEvent as WinitEvent;
    use i_slint_backend_winit::EventResult;
    use i_slint_backend_winit::WinitWindowAccessor;

    let weak = window.as_weak();
    window
        .window()
        .on_winit_window_event(move |_window, event| {
            if let WinitEvent::DroppedFile(path) = event {
                if let Some(win) = weak.upgrade() {
                    handle_file_drop(&win, &sftp_handles, path.to_string_lossy().to_string());
                }
            }
            EventResult::Propagate
        });
}

#[cfg(windows)]
fn cursor_pos() -> Option<(i32, i32)> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    extern "system" {
        fn GetCursorPos(point: *mut Point) -> i32;
    }
    let mut p = Point { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        Some((p.x, p.y))
    } else {
        None
    }
}

#[cfg(windows)]
fn handle_file_drop(win: &AppWindow, sftp_handles: &SftpHandles, path: String) {
    // with_winit_window 是 WinitWindowAccessor 扩展 trait 的方法，需在本函数作用域内
    // 引入该 trait（register_file_drop 里的 use 不跨函数）。否则 Windows 编译报
    // E0599: no method named `with_winit_window`。
    use i_slint_backend_winit::WinitWindowAccessor;
    let active = win.get_active_tab_id().to_string();
    if active == "welcome" {
        return;
    }
    let window = win.window();
    let scale = window.scale_factor().max(0.01);
    let size = window.size();
    let Some(inner) = window
        .with_winit_window(|w| w.inner_position().ok())
        .flatten()
    else {
        return;
    };
    let Some((cx, cy)) = cursor_pos() else {
        return;
    };
    let client_x = (cx - inner.x) as f32 / scale;
    let client_y = (cy - inner.y) as f32 / scale;
    let width = size.width as f32 / scale;
    let height = size.height as f32 / scale;
    let sftp_height = win.get_sftp_panel_height();

    let zone_left = 381.0_f32;
    let zone_top = height - sftp_height + 51.0;
    let zone_bottom = height - 18.0;
    if client_x < zone_left || client_x > width || client_y < zone_top || client_y > zone_bottom {
        return;
    }

    let dir = terminal_sftp_path(win, &active);
    if dir.is_empty() {
        return;
    }
    if let Ok(handles) = sftp_handles.lock() {
        if let Some(handle) = handles.get(&active) {
            handle.upload(path, dir);
        }
    }
}

#[cfg(not(windows))]
fn handle_file_drop(_win: &AppWindow, _sftp_handles: &SftpHandles, _path: String) {}

fn selected_iface(st: &TabStatus) -> (String, u64, u64) {
    if !st.selected_iface.is_empty() {
        if let Some(e) = st.net.iter().find(|e| e.0 == st.selected_iface) {
            return e.clone();
        }
    }
    st.net.first().cloned().unwrap_or_default()
}

/// 终端报告的工作目录变化是否应驱动 SFTP 面板「自动跟随」（重新列目录）。
///
/// 手动模式（用户已在面板里双击进目录 / 展开过目录树）下返回 `false`：此时
/// 终端 `cd` 不得触碰 SFTP 面板 —— 既不重新列目录，也不把 `CwdChanged` 透传
/// 给 UI。后者尤其关键：UI 的 `CwdChanged` 处理会无条件把 `sftp_loading` 置
/// `true`，若这里放行透传却又跳过了 `list_dir`，就再没有 `SftpEntries` /
/// `SftpLoadFailed` 来复位它，面板会永久停在「加载中…」。
fn sftp_should_follow_cwd(is_manual_nav: bool) -> bool {
    !is_manual_nav
}

/// 该事件是否标志「一次目录加载的终结」（成功或失败），因而必须把
/// `sftp_loading` 复位为 `false`。这是守护「loading 不是只进不出的陷阱状态」
/// 这一不变量的单一判定点：凡是会把 `sftp_loading` 置 `true` 的加载，最终都
/// 应收到一个 settled 事件让它落回 `false`。新增加载结束类事件时，必须在此登记。
fn settles_sftp_loading(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::SftpEntries { .. } | SessionEvent::SftpLoadFailed(_)
    )
}

/// 为 tab 建立 SSH + SFTP 连接并接好事件转发线程。
/// connect（新开标签）与 reconnect（原地重连）共用：调用方负责保证
/// TerminalState / TermBuffer / TabStatus 行已存在；这里只装配 IO。
fn start_session_io(
    weak: slint::Weak<AppWindow>,
    ctx: &SessionIoCtx,
    tab_id: String,
    session: Session,
    initial_cols: u32,
    initial_rows: u32,
) {
    let sftp_session = session.clone();
    let (handle, mut rx) = spawn_session(
        ctx.runtime.handle(),
        tab_id.clone(),
        session,
        initial_cols,
        initial_rows,
    );
    ctx.handles.borrow_mut().insert(tab_id.clone(), handle);

    let (sftp_tx, mut sftp_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
    let sftp_handle = spawn_sftp(ctx.runtime.handle(), sftp_session, sftp_tx);
    ctx.sftp_handles
        .lock()
        .unwrap()
        .insert(tab_id.clone(), sftp_handle);

    let weak_events = weak.clone();
    let bufs_events = ctx.bufs.clone();
    let statuses_events = ctx.tab_statuses.clone();
    let local_events = ctx.local_snap.clone();
    let hist_events = ctx.local_net_hist.clone();
    let user_closing_events = ctx.user_closing.clone();
    let sftp_handles_events = ctx.sftp_handles.clone();
    let sftp_manual_events = ctx.sftp_manual_nav.clone();
    let runtime_events = ctx.runtime.clone();
    let shell_tab_id = tab_id.clone();
    std::thread::spawn(move || {
        let mut cwd_debounce: Option<tokio::task::JoinHandle<()>> = None;
        while let Some(event) = rx.blocking_recv() {
            if let SessionEvent::CwdChanged(ref cwd) = event {
                let is_manual = sftp_manual_events
                    .lock()
                    .ok()
                    .and_then(|m| m.get(&shell_tab_id).copied())
                    .unwrap_or(false);
                if sftp_should_follow_cwd(is_manual) {
                    if let Some(prev) = cwd_debounce.take() {
                        prev.abort();
                    }
                    let cwd = cwd.clone();
                    let tid = shell_tab_id.clone();
                    let sftp_handles = sftp_handles_events.clone();
                    cwd_debounce = Some(runtime_events.spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        if let Ok(handles) = sftp_handles.lock() {
                            if let Some(handle) = handles.get(&tid) {
                                handle.list_dir(cwd);
                            }
                        }
                    }));
                } else {
                    // 手动导航模式：不自动跟随终端 cwd。直接丢弃此 CwdChanged，
                    // 不透传给 UI —— 否则 apply 会把 sftp_loading 置 true 却无人
                    // 复位（上面已跳过 list_dir），面板永久停在「加载中…」。
                    continue;
                }
            }
            let weak_evt = weak_events.clone();
            let tab_evt = shell_tab_id.clone();
            let bufs_evt = bufs_events.clone();
            let statuses_evt = statuses_events.clone();
            let local_evt = local_events.clone();
            let hist_evt = hist_events.clone();
            let user_closing_evt = user_closing_events.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_evt.upgrade() {
                    apply_session_event_to_window(
                        &w,
                        &tab_evt,
                        event,
                        &bufs_evt,
                        &statuses_evt,
                        &local_evt,
                        &hist_evt,
                        &user_closing_evt,
                    );
                }
            });
        }
    });

    let weak_events = weak.clone();
    let bufs_events = ctx.bufs.clone();
    let statuses_events = ctx.tab_statuses.clone();
    let local_events = ctx.local_snap.clone();
    let hist_events = ctx.local_net_hist.clone();
    let user_closing_events = ctx.user_closing.clone();
    let tab_events = tab_id;
    std::thread::spawn(move || {
        while let Some(event) = sftp_rx.blocking_recv() {
            let weak_evt = weak_events.clone();
            let tab_evt = tab_events.clone();
            let bufs_evt = bufs_events.clone();
            let statuses_evt = statuses_events.clone();
            let local_evt = local_events.clone();
            let hist_evt = hist_events.clone();
            let user_closing_evt = user_closing_events.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_evt.upgrade() {
                    apply_session_event_to_window(
                        &w,
                        &tab_evt,
                        event,
                        &bufs_evt,
                        &statuses_evt,
                        &local_evt,
                        &hist_evt,
                        &user_closing_evt,
                    );
                }
            });
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn apply_session_event_to_window(
    win: &AppWindow,
    tab_id: &str,
    event: SessionEvent,
    bufs: &TermBuffers,
    statuses: &TabStatuses,
    local: &LocalSnap,
    local_net_hist: &NetHist,
    user_closing: &Arc<Mutex<HashSet<String>>>,
) {
    let tabs_rc = win.get_tabs();
    let terminals_rc = win.get_terminals();
    let Some(tabs) = tabs_rc.as_any().downcast_ref::<VecModel<TabInfo>>() else {
        return;
    };
    let Some(terminals) = terminals_rc
        .as_any()
        .downcast_ref::<VecModel<TerminalState>>()
    else {
        return;
    };

    let update_terminal = |mutator: &dyn Fn(&mut TerminalState)| {
        for i in 0..terminals.row_count() {
            if let Some(mut row) = terminals.row_data(i) {
                if row.id.as_str() == tab_id {
                    mutator(&mut row);
                    terminals.set_row_data(i, row);
                    break;
                }
            }
        }
    };
    let update_tab = |mutator: &dyn Fn(&mut TabInfo)| {
        for i in 0..tabs.row_count() {
            if let Some(mut row) = tabs.row_data(i) {
                if row.id.as_str() == tab_id {
                    mutator(&mut row);
                    tabs.set_row_data(i, row);
                    break;
                }
            }
        }
    };

    // 在 match 消费 event 前算好：本事件是否「终结」一次目录加载。终结事件
    // （成功 SftpEntries / 失败 SftpLoadFailed）统一在 match 之后复位
    // sftp_loading，确保失败路径也能解除「加载中…」，不再只进不出。
    let settles_loading = settles_sftp_loading(&event);

    match event {
        SessionEvent::Status(status) => {
            update_terminal(&|t| t.status = status.clone().into());
        }
        SessionEvent::Output(chunk) => {
            let built = {
                let mut map = bufs.lock().unwrap();
                if let Some(buf) = map.get_mut(tab_id) {
                    buf.ingest(chunk.as_bytes());
                    let cols = buf.parser.screen().size().1;
                    let b = buf.render();
                    let matches = compute_find_matches(&buf.displayed_text, &buf.find_query);
                    let sel = match buf.sel {
                        Some((sr, sc, er, ec)) => selection_rects(sr, sc, er, ec, cols),
                        None => Vec::new(),
                    };
                    Some((b, matches, sel))
                } else {
                    None
                }
            };
            if let Some((b, matches, sel)) = built {
                let spans_model: ModelRc<TermSpan> =
                    ModelRc::from(Rc::new(VecModel::from(b.spans)));
                let matches_model: ModelRc<TermMatch> =
                    ModelRc::from(Rc::new(VecModel::from(matches)));
                let selection_model: ModelRc<TermMatch> =
                    ModelRc::from(Rc::new(VecModel::from(sel)));
                let (cur_row, cur_col, rows_used, is_alt) =
                    (b.cursor_row, b.cursor_col, b.rows_used, b.is_alt);
                update_terminal(&|t| {
                    t.spans = spans_model.clone();
                    t.cursor_row = cur_row;
                    t.cursor_col = cur_col;
                    t.rows_used = rows_used;
                    t.is_alt_screen = is_alt;
                    t.find_matches = matches_model.clone();
                    t.selection = selection_model.clone();
                });
            }
        }
        SessionEvent::Connected => {
            update_tab(&|t| t.connected = true);
            update_terminal(&|t| {
                t.status = crate::i18n::t("已连接", "Connected").into();
                t.conn_lost = false;
            });
            if let Some(st) = statuses.lock().unwrap().get_mut(tab_id) {
                st.state = 1;
                st.reconnect_attempts = 0;
            }
            if win.get_active_tab_id().as_str() == tab_id {
                refresh_sidebar(win, statuses, local, local_net_hist);
            }
        }
        SessionEvent::Closed(reason) => {
            update_tab(&|t| t.connected = false);
            // 判定本次断开的去向：自动重连（曾连上 + 非用户关闭 + 次数未尽）
            // 还是仅亮重连按钮；首次连接失败仍走原有的失败弹窗。
            let (schedule, show_failure_alert) = {
                // 取出并清除"用户主动关闭"标记（一次性）。
                let was_user_close = user_closing.lock().unwrap().remove(tab_id);
                let mut statuses = statuses.lock().unwrap();
                let previous_state = statuses.get(tab_id).map(|st| st.state);
                let attempts = statuses
                    .get(tab_id)
                    .map(|st| st.reconnect_attempts)
                    .unwrap_or(0);
                let auto = should_auto_reconnect(was_user_close, previous_state, attempts);
                if let Some(st) = statuses.get_mut(tab_id) {
                    st.state = 2;
                    if auto {
                        st.reconnect_attempts += 1;
                    }
                }
                let schedule = if auto {
                    auto_reconnect_delay(attempts + 1).map(|d| (d, attempts + 1))
                } else {
                    None
                };
                (
                    schedule,
                    schedule.is_none() && should_alert_on_close(was_user_close, previous_state),
                )
            };
            match schedule {
                Some((delay, attempt)) => {
                    update_terminal(&|t| {
                        t.conn_lost = true;
                        t.status = format!(
                            "{} - {} ({}/3, {}s)",
                            crate::i18n::t("已断开", "Disconnected"),
                            crate::i18n::t("自动重连", "auto-reconnect"),
                            attempt,
                            delay.as_secs(),
                        )
                        .into();
                    });
                    let weak = win.as_weak();
                    let tid = tab_id.to_string();
                    Timer::single_shot(delay, move || {
                        if let Some(w) = weak.upgrade() {
                            w.invoke_reconnect_tab(tid.clone().into());
                        }
                    });
                }
                None => {
                    update_terminal(&|t| {
                        t.conn_lost = true;
                        t.status =
                            format!("{} - {reason}", crate::i18n::t("已断开", "Disconnected"))
                                .into();
                    });
                }
            }
            if show_failure_alert {
                show_connection_failed_alert(win, &reason);
            }
            if win.get_active_tab_id().as_str() == tab_id {
                refresh_sidebar(win, statuses, local, local_net_hist);
            }
        }
        SessionEvent::ResourceStats {
            cpu_percent,
            mem_used_kib,
            mem_total_kib,
            swap_used_kib,
            swap_total_kib,
            net,
            disks,
        } => {
            if let Some(st) = statuses.lock().unwrap().get_mut(tab_id) {
                st.cpu = cpu_percent;
                st.mem_used_kib = mem_used_kib;
                st.mem_total_kib = mem_total_kib;
                st.swap_used_kib = swap_used_kib;
                st.swap_total_kib = swap_total_kib;
                st.net = net;
                st.disks = disks;
                if st.state != 1 {
                    st.state = 1;
                }
                let (_, rx, tx) = selected_iface(st);
                push_ring(&mut st.net_hist, (rx + tx) as f32);
            }
            if win.get_active_tab_id().as_str() == tab_id {
                refresh_sidebar(win, statuses, local, local_net_hist);
            }
        }
        SessionEvent::CwdChanged(path) => {
            update_terminal(&|t| {
                t.sftp_path = path.clone().into();
                t.sftp_loading = true;
            });
        }
        SessionEvent::SftpEntries { path, entries } => {
            let rows: Vec<SftpEntry> = entries
                .iter()
                .map(|e| SftpEntry {
                    name: e.name.clone().into(),
                    full_path: e.full_path.clone().into(),
                    is_dir: e.is_dir,
                    size: if e.is_dir {
                        "".into()
                    } else {
                        crate::ssh::format_size(e.size).into()
                    },
                    modified: crate::ssh::format_mtime(e.modified).into(),
                })
                .collect();
            let model = ModelRc::from(Rc::new(VecModel::from(rows)));
            update_terminal(&|t| {
                t.sftp_path = path.clone().into();
                t.sftp_entries = model.clone();
            });
            // sftp_loading 的复位交由 match 之后的统一 settled 处理。
        }
        SessionEvent::SftpLoadFailed(msg) => {
            // 目录加载失败：回显错误原因，保留用户当前正在看的列表不动。
            // loading 的复位同样交由统一 settled 处理 —— 这正是「刷新不了、
            // 一直加载中」的修复点：失败不再把面板永久卡在加载态。
            update_terminal(&|t| t.sftp_status = msg.clone().into());
        }
        SessionEvent::SftpStatus(msg) => {
            update_terminal(&|t| t.sftp_status = msg.clone().into());
            // 编辑器保存结果：成功（"已保存/Saved"开头）→ 关闭编辑器，回到文件管理器；
            // 底部状态行已回显"已保存: <文件名>"作为成功提示。失败（"保存失败/Save
            // failed"开头）→ 留在编辑器并把错误回显到状态行，便于用户重试。
            if win.get_editor_open() {
                if msg.starts_with(crate::i18n::t("已保存", "Saved")) {
                    win.set_editor_dirty(false);
                    win.set_editor_confirm_discard(false);
                    win.set_editor_content("".into());
                    win.set_editor_status("".into());
                    win.set_editor_open(false);
                } else if msg.starts_with(crate::i18n::t("保存失败", "Save failed")) {
                    win.set_editor_status(msg.clone().into());
                }
            }
        }
        SessionEvent::SftpFileContent {
            remote,
            filename,
            content,
        } => {
            win.set_editor_tab_id(tab_id.into());
            win.set_editor_path(remote.into());
            win.set_editor_filename(filename.into());
            win.set_editor_content(content.into());
            win.set_editor_dirty(false);
            win.set_editor_confirm_discard(false);
            win.set_editor_status("".into());
            win.set_editor_open(true);
        }
        SessionEvent::SftpTreeUpdate(nodes) => {
            let rows: Vec<SftpTreeNode> = nodes
                .iter()
                .map(|n| SftpTreeNode {
                    path: n.path.clone().into(),
                    name: n.name.clone().into(),
                    depth: n.depth as i32,
                    expanded: n.expanded,
                    has_children: n.has_children,
                })
                .collect();
            let model = ModelRc::from(Rc::new(VecModel::from(rows)));
            update_terminal(&|t| t.sftp_tree_nodes = model.clone());
        }
        SessionEvent::SftpTransfer {
            id,
            name,
            is_upload,
            transferred,
            total,
            state,
            msg: _,
        } => {
            let detail = match state {
                2 => crate::i18n::t("失败", "Failed").to_string(),
                1 => crate::i18n::t("已完成", "Done").to_string(),
                _ if total > 0 => format!(
                    "{}/{}",
                    crate::ssh::format_size(transferred),
                    crate::ssh::format_size(total)
                ),
                _ => crate::ssh::format_size(transferred),
            };
            let percent = if state == 1 {
                1.0
            } else if total > 0 {
                (transferred as f32 / total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let rec = TransferInfo {
                id: id.clone().into(),
                name: name.into(),
                detail: detail.into(),
                percent,
                state: state as i32,
                is_upload,
            };
            if let Some(model) = win
                .get_transfers()
                .as_any()
                .downcast_ref::<VecModel<TransferInfo>>()
            {
                let mut found = None;
                for i in 0..model.row_count() {
                    if let Some(row) = model.row_data(i) {
                        if row.id.as_str() == id.as_str() {
                            found = Some(i);
                            break;
                        }
                    }
                }
                match found {
                    Some(i) => model.set_row_data(i, rec),
                    None => model.insert(0, rec),
                }
            }
        }
    }

    // 统一复位：凡「终结一次目录加载」的事件（成功 SftpEntries 或失败
    // SftpLoadFailed）都在这唯一出口把 sftp_loading 落回 false。单一出口确保
    // 不会再出现「置了 true 却没人复位」的陷阱状态 —— 这是「文件管理器一直
    // 加载中、刷新不了」的根因防线。
    if settles_loading {
        update_terminal(&|t| t.sftp_loading = false);
    }
}

fn compute_find_matches(rows: &[String], query: &str) -> Vec<TermMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let q: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    for (r, line) in rows.iter().enumerate() {
        let lower: Vec<char> = line.chars().map(|c| c.to_ascii_lowercase()).collect();
        let mut i = 0usize;
        while i + q.len() <= lower.len() {
            if lower[i..i + q.len()] == q[..] {
                out.push(TermMatch {
                    row: r as i32,
                    col: i as i32,
                    len: q.len() as i32,
                });
                i += q.len();
            } else {
                i += 1;
            }
        }
    }
    out
}

fn norm_sel(sr: u16, sc: u16, er: u16, ec: u16) -> (u16, u16, u16, u16) {
    if (sr, sc) <= (er, ec) {
        (sr, sc, er, ec)
    } else {
        (er, ec, sr, sc)
    }
}

fn selection_rects(sr: u16, sc: u16, er: u16, ec: u16, cols: u16) -> Vec<TermMatch> {
    let (sr, sc, er, ec) = norm_sel(sr, sc, er, ec);
    let mut out = Vec::new();
    if sr == er {
        let lo = sc.min(ec);
        let hi = sc.max(ec);
        out.push(TermMatch {
            row: sr as i32,
            col: lo as i32,
            len: (hi - lo + 1) as i32,
        });
    } else {
        out.push(TermMatch {
            row: sr as i32,
            col: sc as i32,
            len: (cols - sc) as i32,
        });
        for r in (sr + 1)..er {
            out.push(TermMatch {
                row: r as i32,
                col: 0,
                len: cols as i32,
            });
        }
        out.push(TermMatch {
            row: er as i32,
            col: 0,
            len: (ec + 1) as i32,
        });
    }
    out
}

fn extract_selection(rows: &[String], sr: u16, sc: u16, er: u16, ec: u16) -> String {
    let (sr, sc, er, ec) = norm_sel(sr, sc, er, ec);
    let mut out = String::new();
    for r in sr..=er {
        let chars: Vec<char> = rows
            .get(r as usize)
            .map(|line| line.chars().collect())
            .unwrap_or_default();
        let (lo, hi) = if sr == er {
            (sc.min(ec), sc.max(ec))
        } else if r == sr {
            (sc, u16::MAX)
        } else if r == er {
            (0, ec)
        } else {
            (0, u16::MAX)
        };
        let lo = (lo as usize).min(chars.len());
        let hi = ((hi as usize).saturating_add(1)).min(chars.len());
        if lo < hi {
            let segment: String = chars[lo..hi].iter().collect();
            out.push_str(segment.trim_end());
        }
        if r != er {
            out.push('\n');
        }
    }
    out
}

fn rebuild_tab_display(win: &AppWindow, bufs: &TermBuffers, tab_id: &str) {
    let data = {
        let mut map = bufs.lock().unwrap();
        let Some(buf) = map.get_mut(tab_id) else {
            return;
        };
        let cols = buf.parser.screen().size().1;
        let b = buf.render();
        let matches = compute_find_matches(&buf.displayed_text, &buf.find_query);
        let sel = match buf.sel {
            Some((sr, sc, er, ec)) => selection_rects(sr, sc, er, ec, cols),
            None => Vec::new(),
        };
        (b, matches, sel)
    };
    let (b, matches, sel) = data;
    let spans = ModelRc::from(Rc::new(VecModel::from(b.spans)));
    let fm = ModelRc::from(Rc::new(VecModel::from(matches)));
    let sm = ModelRc::from(Rc::new(VecModel::from(sel)));
    set_terminal_row(win, tab_id, move |row| {
        row.spans = spans.clone();
        row.cursor_row = b.cursor_row;
        row.cursor_col = b.cursor_col;
        row.rows_used = b.rows_used;
        row.is_alt_screen = b.is_alt;
        row.find_matches = fm.clone();
        row.selection = sm.clone();
    });
}

fn resize_terminal_buffer(
    tab_id: &str,
    cols_f: f32,
    rows_f: f32,
    bufs: &TermBuffers,
    last_size: &Arc<Mutex<(u32, u32)>>,
) -> (u32, u32, bool) {
    let cols = (cols_f as u32).max(10);
    let rows = (rows_f as u32).max(5);
    *last_size.lock().unwrap() = (cols, rows);

    let mut applied = false;
    if let Some(buf) = bufs.lock().unwrap().get_mut(tab_id) {
        let (old_rows, old_cols) = buf.parser.screen().size();
        // Already at the requested grid: do nothing. Skipping the redundant
        // window_change + reflow stops the remote shell from reprinting its
        // prompt (the "multiple blank lines while dragging the file manager" bug).
        if old_rows == rows as u16 && old_cols == cols as u16 {
            return (cols, rows, false);
        }
        if (rows as u16) < old_rows && !buf.parser.screen().alternate_screen() {
            let s = buf.parser.screen();
            let cols_now = s.size().1;
            // The last row that must remain on the shrunken screen: the cursor
            // row, or the lowest row with visible content below it.
            let cursor_row = s.cursor_position().0;
            let mut keep_last = cursor_row;
            for r in (cursor_row + 1..old_rows).rev() {
                if line_has_visible_content(&build_row(s, r, cols_now)) {
                    keep_last = r;
                    break;
                }
            }
            // Scroll up only what is needed to keep `keep_last` inside the new
            // grid. A fixed old-new delta here shoves a mostly-empty screen's
            // content into history while the cursor stays at its old row, which
            // paints a tall blank gap above the prompt after mouse-up (the
            // "extra newlines / huge line spacing" drag bug).
            let need = (keep_last + 1).saturating_sub(rows as u16);
            if need > 0 {
                for r in 0..need {
                    let line = build_row(s, r, cols_now);
                    if line_has_visible_content(&line) {
                        buf.history.push(line);
                    }
                }
                if buf.history.len() > MAX_HISTORY {
                    let drop = buf.history.len() - MAX_HISTORY;
                    buf.history.drain(0..drop);
                }
                let scroll_up = format!("\x1b[{need}S");
                buf.parser.process(scroll_up.as_bytes());
            }
        }
        buf.parser.set_size(rows as u16, cols as u16);
        buf.prev.clear();
        applied = true;
    }

    (cols, rows, applied)
}

fn schedule_terminal_display_rebuild(
    weak: slint::Weak<AppWindow>,
    bufs: TermBuffers,
    tab_id: String,
) {
    Timer::single_shot(std::time::Duration::from_millis(1), move || {
        if let Some(w) = weak.upgrade() {
            rebuild_tab_display(&w, &bufs, &tab_id);
        }
    });
}

fn set_terminal_row(win: &AppWindow, tab_id: &str, mutator: impl Fn(&mut TerminalState)) {
    let terminals = win.get_terminals();
    let Some(model) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
        return;
    };
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            if row.id.as_str() == tab_id {
                mutator(&mut row);
                model.set_row_data(i, row);
                break;
            }
        }
    }
}

fn key_to_pty_bytes(key: &str, ctrl: bool, alt: bool, app_cursor: bool) -> Vec<u8> {
    let special: Option<&[u8]> = match key {
        "\u{F700}" => Some(if app_cursor {
            b"\x1bOA" as &[u8]
        } else {
            b"\x1b[A"
        }),
        "\u{F701}" => Some(if app_cursor {
            b"\x1bOB" as &[u8]
        } else {
            b"\x1b[B"
        }),
        "\u{F702}" => Some(if app_cursor {
            b"\x1bOD" as &[u8]
        } else {
            b"\x1b[D"
        }),
        "\u{F703}" => Some(if app_cursor {
            b"\x1bOC" as &[u8]
        } else {
            b"\x1b[C"
        }),
        "\u{F729}" => Some(b"\x1b[H" as &[u8]),
        "\u{F72B}" => Some(b"\x1b[F" as &[u8]),
        "\u{F72C}" => Some(b"\x1b[5~" as &[u8]),
        "\u{F72D}" => Some(b"\x1b[6~" as &[u8]),
        "\u{F728}" => Some(b"\x1b[3~" as &[u8]),
        "\u{F704}" => Some(b"\x1bOP" as &[u8]),
        "\u{F705}" => Some(b"\x1bOQ" as &[u8]),
        "\u{F706}" => Some(b"\x1bOR" as &[u8]),
        "\u{F707}" => Some(b"\x1bOS" as &[u8]),
        "\u{F708}" => Some(b"\x1b[15~" as &[u8]),
        "\u{F709}" => Some(b"\x1b[17~" as &[u8]),
        "\u{F70A}" => Some(b"\x1b[18~" as &[u8]),
        "\u{F70B}" => Some(b"\x1b[19~" as &[u8]),
        "\u{F70C}" => Some(b"\x1b[20~" as &[u8]),
        "\u{F70D}" => Some(b"\x1b[21~" as &[u8]),
        "\u{F70E}" => Some(b"\x1b[23~" as &[u8]),
        "\u{F70F}" => Some(b"\x1b[24~" as &[u8]),
        _ => None,
    };
    if let Some(seq) = special {
        return seq.to_vec();
    }
    if key == "\u{0008}" {
        return vec![0x7f];
    }
    if key == "\n" && !ctrl && !alt {
        return vec![0x0d];
    }
    if key.is_empty() {
        return vec![];
    }
    if ctrl {
        if let Some(c) = key.chars().next() {
            let cp = c as u32;
            if key.chars().count() == 1 && (0x01..=0x1f).contains(&cp) {
                return vec![cp as u8];
            }
            if key.chars().count() == 1 {
                let upper = c.to_ascii_uppercase() as u8;
                let ctrl_char = match upper {
                    b'A'..=b'Z' => Some(upper - b'A' + 1),
                    b'[' => Some(0x1b),
                    b'\\' => Some(0x1c),
                    b']' => Some(0x1d),
                    b'^' => Some(0x1e),
                    b'_' => Some(0x1f),
                    b'@' => Some(0x00),
                    _ => None,
                };
                if let Some(byte) = ctrl_char {
                    return vec![byte];
                }
            }
        }
    }
    if key.chars().any(|c| (0xE000..=0xF8FF).contains(&(c as u32))) {
        return vec![];
    }
    if alt && !ctrl {
        let mut bytes = vec![0x1b];
        bytes.extend_from_slice(key.as_bytes());
        return bytes;
    }
    key.as_bytes().to_vec()
}

#[cfg(windows)]
fn is_vk_back_down() -> bool {
    #[allow(non_snake_case)]
    extern "system" {
        fn GetKeyState(nVirtKey: i32) -> i16;
    }
    const VK_BACK: i32 = 0x08;
    unsafe { (GetKeyState(VK_BACK) as u16) & 0x8000 != 0 }
}

#[cfg(windows)]
fn c0_letter_key_down(cp: u32) -> bool {
    if !(0x01..=0x1a).contains(&cp) {
        return true;
    }
    #[allow(non_snake_case)]
    extern "system" {
        fn GetKeyState(nVirtKey: i32) -> i16;
    }
    let vk = (cp + 0x40) as i32;
    unsafe { (GetKeyState(vk) as u16) & 0x8000 != 0 }
}

struct BuiltScreen {
    spans: Vec<TermSpan>,
    cursor_row: i32,
    cursor_col: i32,
    rows_used: i32,
    is_alt: bool,
}

#[derive(Clone)]
struct HistSpan {
    text: String,
    fg: slint::Color,
    bg: slint::Color,
    bold: bool,
    col: i32,
    cells: i32,
}

fn cell_attrs(
    screen: &vt100::Screen,
    r: u16,
    c: u16,
) -> (String, vt100::Color, vt100::Color, bool) {
    match screen.cell(r, c) {
        Some(cell) => {
            let (mut fg, mut bg) = (cell.fgcolor(), cell.bgcolor());
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut bg);
            }
            let s = cell.contents();
            // 宽字符（CJK 全角字符）在 vt100 网格里占两格：第 1 格存字符本身，
            // 第 2 格是「延续格」，其 contents() 为空字符串。延续格不是真正的空白
            // 单元格——前一格的全角字形已经覆盖了这两格的宽度。若把它补成半角
            // 空格，每个汉字后就会多出一个空格，终端里中文字间距被撑大、复制出来
            // 的文本也夹带空格。只有真正的空白格才补空格，以维持等宽网格对齐。
            let s = if cell.is_wide_continuation() {
                String::new()
            } else if s.is_empty() {
                " ".to_string()
            } else {
                s
            };
            (s, fg, bg, cell.bold())
        }
        None => (
            " ".to_string(),
            vt100::Color::Default,
            vt100::Color::Default,
            false,
        ),
    }
}

fn build_row(screen: &vt100::Screen, r: u16, cols: u16) -> Line {
    let mut plain = String::with_capacity(cols as usize);
    let mut runs = Vec::new();
    let mut c = 0u16;
    while c < cols {
        let (s, fg, bg, bold) = cell_attrs(screen, r, c);
        let start_col = c;
        let mut text = s.clone();
        plain.push_str(&s);
        c += 1;
        while c < cols {
            let (cs, cfg, cbg, cbold) = cell_attrs(screen, r, c);
            if cfg != fg || cbg != bg || cbold != bold {
                break;
            }
            plain.push_str(&cs);
            text.push_str(&cs);
            c += 1;
        }
        let cells = (c - start_col) as i32;
        let is_blank = text.chars().all(|ch| ch == ' ');
        let bg_default = matches!(bg, vt100::Color::Default);
        if is_blank && bg_default {
            continue;
        }
        runs.push(HistSpan {
            text,
            fg: vt_color_to_slint(fg, bold),
            bg: vt_bg_to_slint(bg),
            bold,
            col: start_col as i32,
            cells,
        });
    }
    (plain, runs)
}

fn line_has_visible_content(line: &Line) -> bool {
    !line.0.trim_end().is_empty() || !line.1.is_empty()
}

/// 滚轮去抖累积：把「分数行」增量 `delta_lines` 并入余数 `acc`，返回应滚动的整
/// 行数（向零截断），余数留在 `acc` 供后续累积。
///
/// 目的：消除「上滑到顶后又莫名下跳几行」。触控板惯性滚动末尾、以及到达边界后的
/// 回弹，会产生方向相反、幅度不足一行的微小 scroll 事件；若每个事件都按符号固定
/// 翻 ±N 行，这些亚行抖动就被放大成整行回退。改为按真实幅度累积后，亚行反向抖动
/// 只是抵消余数、不触发滚动。
fn accumulate_scroll_lines(acc: &mut f32, delta_lines: f32) -> i64 {
    *acc += delta_lines;
    let lines = acc.trunc() as i64;
    *acc -= lines as f32;
    lines
}

fn detect_scroll(prev: &[Line], curr: &[Line]) -> usize {
    let mut best_k = 0usize;
    let mut best_len = 0usize;
    for k in 0..prev.len() {
        let mut p = 0usize;
        while k + p < prev.len() && p < curr.len() && prev[k + p].0 == curr[p].0 {
            p += 1;
        }
        if p > best_len {
            best_len = p;
            best_k = k;
        }
    }
    best_k
}

impl TermBuffer {
    fn ingest(&mut self, raw: &[u8]) {
        let bytes = self.rewrite_hvp(raw);
        let rows = self.parser.screen().size().0 as usize;
        let batch_lines = (rows / 2).max(1);
        let mut start = 0usize;
        let mut nl = 0usize;
        for i in 0..bytes.len() {
            if bytes[i] == b'\n' {
                nl += 1;
                if nl >= batch_lines {
                    self.ingest_chunk(&bytes[start..=i]);
                    start = i + 1;
                    nl = 0;
                }
            }
        }
        if start < bytes.len() {
            self.ingest_chunk(&bytes[start..]);
        }
    }

    fn rewrite_hvp(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            match self.csi_state {
                CsiState::Normal => {
                    if b == 0x1b {
                        self.csi_state = CsiState::Esc;
                    }
                    out.push(b);
                }
                CsiState::Esc => {
                    if b == b'[' {
                        self.csi_state = CsiState::Csi;
                    } else {
                        self.csi_state = if b == 0x1b {
                            CsiState::Esc
                        } else {
                            CsiState::Normal
                        };
                    }
                    out.push(b);
                }
                CsiState::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        out.push(if b == b'f' { b'H' } else { b });
                        self.csi_state = CsiState::Normal;
                    } else {
                        out.push(b);
                    }
                }
            }
        }
        out
    }

    fn ingest_chunk(&mut self, bytes: &[u8]) {
        let has_cursor_home = bytes.windows(3).any(|w| w == b"\x1b[H");
        let has_erase_display =
            bytes.windows(4).any(|w| w == b"\x1b[2J") || bytes.windows(3).any(|w| w == b"\x1b[J");
        let is_fullscreen_refresh = has_cursor_home && has_erase_display;

        self.parser.process(bytes);
        let (is_alt, rows, cols) = {
            let s = self.parser.screen();
            let (r, c) = s.size();
            (s.alternate_screen(), r, c)
        };
        if is_alt || is_fullscreen_refresh {
            self.view_offset = 0;
            self.prev.clear();
            return;
        }
        let curr: Vec<Line> = {
            let s = self.parser.screen();
            (0..rows).map(|r| build_row(s, r, cols)).collect()
        };
        if !self.prev.is_empty() {
            let k = detect_scroll(&self.prev, &curr);
            for line in self.prev.iter().take(k) {
                self.history.push(line.clone());
            }
            if self.history.len() > MAX_HISTORY {
                let drop = self.history.len() - MAX_HISTORY;
                self.history.drain(0..drop);
            }
            // 回看历史时(view_offset>0)，新滚出的 k 行进入 history 底部，可视窗口
            // 必须同步下移 k 行、锚定到同一批历史内容；否则渲染窗口 start 增大、
            // 整屏向下漂移，最早的命令被挤出视野(滚到顶却看不到顶部命令)。贴底
            // 实时(view_offset==0)保持不动以跟随最新输出。clamp 到 history.len()
            // 避免越过最顶(并兼顾 MAX_HISTORY 裁剪后的边界)。
            if self.view_offset > 0 {
                self.view_offset = (self.view_offset + k).min(self.history.len());
            }
        }
        self.prev = curr;
    }

    fn render(&mut self) -> BuiltScreen {
        let (is_alt, rows, cols, cur_row, cur_col) = {
            let s = self.parser.screen();
            let (r, c) = s.size();
            let (cr, cc) = s.cursor_position();
            (s.alternate_screen(), r, c, cr, cc)
        };

        if is_alt || self.view_offset == 0 {
            let mut spans = Vec::new();
            let mut displayed = Vec::with_capacity(rows as usize);
            let mut last_content = 0i32;
            let s = self.parser.screen();
            for r in 0..rows {
                let (plain, runs) = build_row(s, r, cols);
                if !runs.is_empty() {
                    last_content = r as i32;
                }
                for hs in runs {
                    spans.push(TermSpan {
                        text: hs.text.into(),
                        fg: hs.fg,
                        bg: hs.bg,
                        bold: hs.bold,
                        row: r as i32,
                        col: hs.col,
                        cells: hs.cells,
                    });
                }
                displayed.push(plain.trim_end().to_string());
            }
            self.displayed_text = displayed;
            return BuiltScreen {
                spans,
                cursor_row: cur_row as i32,
                cursor_col: cur_col as i32,
                rows_used: if is_alt {
                    rows as i32
                } else {
                    last_content + 1
                },
                is_alt,
            };
        }

        let live: Vec<Line> = {
            let s = self.parser.screen();
            (0..rows).map(|r| build_row(s, r, cols)).collect()
        };
        let live_used = live
            .iter()
            .rposition(|(_, r)| !r.is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);
        let hist_len = self.history.len();
        let combined_len = hist_len + live_used;
        let win = rows as usize;
        let start = combined_len.saturating_sub(win + self.view_offset);
        let end = (start + win).min(combined_len);
        let mut spans = Vec::new();
        let mut displayed = Vec::with_capacity(win);
        for (d, idx) in (start..end).enumerate() {
            let line = if idx < hist_len {
                &self.history[idx]
            } else {
                &live[idx - hist_len]
            };
            for hs in &line.1 {
                spans.push(TermSpan {
                    text: hs.text.clone().into(),
                    fg: hs.fg,
                    bg: hs.bg,
                    bold: hs.bold,
                    row: d as i32,
                    col: hs.col,
                    cells: hs.cells,
                });
            }
            displayed.push(line.0.trim_end().to_string());
        }
        while displayed.len() < win {
            displayed.push(String::new());
        }
        self.displayed_text = displayed;
        BuiltScreen {
            spans,
            cursor_row: -1,
            cursor_col: 0,
            rows_used: win as i32,
            is_alt: false,
        }
    }
}

// GitHub-Dark terminal palette (matched to ui/theme.slint term-bg/term-fg).
// 0-7 normal, 8-15 bright. Normal green/blue/yellow line up with the design
// mockup's prompt (#3fb950), links (#58a6ff) and `apt list` hint (#d29922).
const ANSI16: [(u8, u8, u8); 16] = [
    (0x48, 0x4f, 0x58), // black
    (0xff, 0x7b, 0x72), // red
    (0x3f, 0xb9, 0x50), // green
    (0xd2, 0x99, 0x22), // yellow
    (0x58, 0xa6, 0xff), // blue
    (0xbc, 0x8c, 0xff), // magenta
    (0x39, 0xc5, 0xcf), // cyan
    (0xb1, 0xba, 0xc4), // white
    (0x6e, 0x76, 0x81), // bright black
    (0xff, 0xa1, 0x98), // bright red
    (0x56, 0xd3, 0x64), // bright green
    (0xe3, 0xb3, 0x41), // bright yellow
    (0x79, 0xc0, 0xff), // bright blue
    (0xd2, 0xa8, 0xff), // bright magenta
    (0x56, 0xd4, 0xdd), // bright cyan
    (0xf0, 0xf6, 0xfc), // bright white
];

fn vt_color_to_slint(color: vt100::Color, bold: bool) -> slint::Color {
    let (r, g, b) = match color {
        vt100::Color::Default => (0xc9, 0xd1, 0xd9),
        vt100::Color::Idx(i) => idx_to_rgb(i, bold),
        vt100::Color::Rgb(r, g, b) => (r, g, b),
    };
    slint::Color::from_rgb_u8(r, g, b)
}

fn vt_bg_to_slint(color: vt100::Color) -> slint::Color {
    match color {
        vt100::Color::Default => slint::Color::from_argb_u8(0, 0, 0, 0),
        vt100::Color::Idx(i) => {
            let (r, g, b) = idx_to_rgb(i, false);
            slint::Color::from_rgb_u8(r, g, b)
        }
        vt100::Color::Rgb(r, g, b) => slint::Color::from_rgb_u8(r, g, b),
    }
}

fn idx_to_rgb(i: u8, bold: bool) -> (u8, u8, u8) {
    let i = if bold && i < 8 { i + 8 } else { i };
    match i {
        0..=15 => ANSI16[i as usize],
        16..=231 => {
            let n = i - 16;
            let to = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (to(n / 36), to((n % 36) / 6), to(n % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

/// 把粘贴文本里的换行统一折叠成单个回车（CR，`\r`）。
///
/// 剪贴板换行可能是 CRLF（Windows）、LF（Unix）或已是 CR。远端 shell 以 CR 作为
/// 行提交信号；若把 LF 或 CRLF 原样发去，多行命令每行会触发两个换行，反斜杠续行
/// （`\<newline>`）被提前结束，导致后续行丢失或被当成独立命令执行。统一折叠为单个
/// CR 后，每个换行只提交一次，续行语义保持完整。
fn normalize_pasted_newlines(text: &str) -> String {
    // 先折 CRLF→CR，再把残留的独立 LF→CR；已是单个 CR 的保持不变。
    text.replace("\r\n", "\r").replace('\n', "\r")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_pasted_newlines_folds_crlf_and_lf_to_cr() {
        // 多行/续行命令粘贴：剪贴板的 CRLF(Windows)或 LF 必须折叠成单个 CR，
        // 否则远端 shell 每行看到两个换行，反斜杠续行被提前结束、后续行丢失。
        assert_eq!(normalize_pasted_newlines("a\nb\nc"), "a\rb\rc");
        assert_eq!(
            normalize_pasted_newlines("sudo apt install \\\r\n  docker-ce"),
            "sudo apt install \\\r  docker-ce"
        );
        // 已是单个 CR 的原样保留，不重复处理。
        assert_eq!(normalize_pasted_newlines("a\rb"), "a\rb");
        // 无换行原样返回。
        assert_eq!(normalize_pasted_newlines("echo hi"), "echo hi");
    }

    #[test]
    fn wide_chars_render_without_padding_spaces() {
        // 中文（CJK 全角字符）在 vt100 网格里占两格：第 1 格存字符本身，第 2 格
        // 是「宽字符延续格」，其 contents() 为空字符串。该延续格不能被补成半角
        // 空格，否则每个汉字后会多出一个空格——终端里中文字间距被撑大，复制出来
        // 的文本也夹带空格。
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process("中文ab".as_bytes());
        let screen = parser.screen();
        let (plain, runs) = build_row(screen, 0, 80);
        assert_eq!(plain.trim_end(), "中文ab", "宽字符之间不应插入空格");
        assert_eq!(
            runs[0].text.trim_end(),
            "中文ab",
            "合并后的文字段不应夹带空格"
        );
    }

    #[test]
    fn auto_reconnect_backoff_caps_at_three_attempts() {
        use std::time::Duration;
        assert_eq!(auto_reconnect_delay(1), Some(Duration::from_secs(2)));
        assert_eq!(auto_reconnect_delay(2), Some(Duration::from_secs(4)));
        assert_eq!(auto_reconnect_delay(3), Some(Duration::from_secs(8)));
        assert_eq!(auto_reconnect_delay(4), None);
        assert_eq!(auto_reconnect_delay(0), None);
    }

    #[test]
    fn auto_reconnect_only_after_established_non_user_close() {
        // (was_user_close, previous_state, attempts_so_far) → 是否安排自动重连
        assert!(should_auto_reconnect(false, Some(1), 0));
        assert!(should_auto_reconnect(false, Some(1), 2));
        assert!(!should_auto_reconnect(true, Some(1), 0)); // 用户主动关
        assert!(!should_auto_reconnect(false, Some(0), 0)); // 从未连上（配置错）
        assert!(!should_auto_reconnect(false, None, 0)); // 状态未知
        assert!(!should_auto_reconnect(false, Some(1), 3)); // 次数用尽
    }

    #[test]
    fn maps_terminal_keys_to_pty_bytes() {
        assert_eq!(key_to_pty_bytes("\n", false, false, false), vec![0x0d]);
        assert_eq!(
            key_to_pty_bytes("\u{0008}", false, false, false),
            vec![0x7f]
        );
        assert_eq!(key_to_pty_bytes("\u{F700}", false, false, false), b"\x1b[A");
        assert_eq!(key_to_pty_bytes("\u{F700}", false, false, true), b"\x1bOA");
        assert_eq!(
            key_to_pty_bytes("\u{F70F}", false, false, false),
            b"\x1b[24~"
        );
        assert_eq!(key_to_pty_bytes("c", true, false, false), vec![0x03]);
        assert_eq!(key_to_pty_bytes("x", false, true, false), b"\x1bx");
    }

    #[test]
    fn extracts_multiline_selection() {
        let rows = vec![
            "hello world".to_string(),
            "middle   ".to_string(),
            "tail".to_string(),
        ];
        assert_eq!(extract_selection(&rows, 0, 6, 2, 1), "world\nmiddle\nta");
    }

    #[test]
    fn only_shows_connection_failed_alert_before_successful_connect() {
        assert!(should_show_connection_failed_alert(None));
        assert!(should_show_connection_failed_alert(Some(0)));
        assert!(!should_show_connection_failed_alert(Some(1)));
        assert!(!should_show_connection_failed_alert(Some(2)));
    }

    #[test]
    fn user_close_suppresses_failure_alert() {
        // 用户主动关闭标签：永不弹窗，即使从未连上。
        assert!(!should_alert_on_close(true, None));
        assert!(!should_alert_on_close(true, Some(0)));
        // 非用户关闭：沿用原"仅未成功连接前弹窗"的逻辑。
        assert!(should_alert_on_close(false, None));
        assert!(should_alert_on_close(false, Some(0)));
        assert!(!should_alert_on_close(false, Some(1)));
        assert!(!should_alert_on_close(false, Some(2)));
    }

    #[test]
    fn terminal_resize_requests_deferred_display_rebuild() {
        let tab_id = "term-test";
        let bufs: TermBuffers = Arc::new(Mutex::new(HashMap::new()));
        let mut buf = TermBuffer {
            parser: vt100::Parser::new(24, 80, 5000),
            find_query: String::new(),
            sel: None,
            history: Vec::new(),
            prev: Vec::new(),
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
        };
        buf.ingest(b"hello");
        bufs.lock().unwrap().insert(tab_id.to_string(), buf);
        let last_size = Arc::new(Mutex::new((80, 24)));

        let (cols, rows, should_rebuild) =
            resize_terminal_buffer(tab_id, 120.0, 40.0, &bufs, &last_size);

        assert_eq!((cols, rows), (120, 40));
        assert_eq!(*last_size.lock().unwrap(), (120, 40));
        assert!(should_rebuild);
        assert_eq!(
            bufs.lock().unwrap()[tab_id].parser.screen().size(),
            (40, 120)
        );
    }

    #[test]
    fn terminal_resize_noops_when_grid_unchanged() {
        // 拖拽 SFTP 分隔器松手时，若网格尺寸未变，resize_terminal_buffer 必须返回
        // applied=false 短路，避免重复向远端发 window_change/SIGWINCH —— 否则远端
        // shell 会反复重打印提示符，正是「拖动文件管理器后命令行多次换行/间隔」的根因。
        let tab_id = "term-noop";
        let bufs: TermBuffers = Arc::new(Mutex::new(HashMap::new()));
        let buf = TermBuffer {
            parser: vt100::Parser::new(24, 80, 5000),
            find_query: String::new(),
            sel: None,
            history: Vec::new(),
            prev: Vec::new(),
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
        };
        bufs.lock().unwrap().insert(tab_id.to_string(), buf);
        let last_size = Arc::new(Mutex::new((80, 24)));

        // 请求与当前完全相同的网格（80 列 × 24 行）。
        let (cols, rows, applied) = resize_terminal_buffer(tab_id, 80.0, 24.0, &bufs, &last_size);

        assert_eq!((cols, rows), (80, 24));
        assert!(!applied, "网格未变时必须短路，不得重复 resize");
    }

    #[test]
    fn terminal_resize_shrink_keeps_sparse_content_anchored() {
        // 「上下拖动文件管理器松手后，提示符上方出现大段空行/行距过大」回归：
        // 旧实现缩小 rows 时固定滚动 old-new 行；屏幕内容不足该差值时内容被
        // 整体滚出屏幕、光标却留在原行号，提示符上方留下大片空白。
        let tab_id = "term-shrink-sparse";
        let bufs: TermBuffers = Arc::new(Mutex::new(HashMap::new()));
        let mut buf = TermBuffer {
            parser: vt100::Parser::new(40, 80, 5000),
            find_query: String::new(),
            sel: None,
            history: Vec::new(),
            prev: Vec::new(),
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
        };
        // 模拟 banner + 提示符：两行内容，光标停在第 2 行（row 1）行尾。
        buf.ingest(b"welcome to server\r\nuser@host:~$ ");
        bufs.lock().unwrap().insert(tab_id.to_string(), buf);
        let last_size = Arc::new(Mutex::new((80u32, 40u32)));

        // 终端区域被压矮：40 行 → 24 行。
        let (_, _, applied) = resize_terminal_buffer(tab_id, 80.0, 24.0, &bufs, &last_size);
        assert!(applied);

        let m = bufs.lock().unwrap();
        let b = &m[tab_id];
        let s = b.parser.screen();
        assert_eq!(s.size(), (24, 80));
        // 内容原位保留、光标仍跟随提示符行、history 不得吞掉屏幕内容。
        assert!(b.history.is_empty(), "稀疏屏幕缩小时不得把内容滚入 history");
        assert_eq!(build_row(s, 0, 80).0.trim_end(), "welcome to server");
        assert!(build_row(s, 1, 80).0.starts_with("user@host:~$"));
        assert_eq!(s.cursor_position().0, 1);
    }

    #[test]
    fn terminal_resize_shrink_scrolls_full_screen_into_history() {
        // 光标深于新屏幕时仍需滚动：只滚「光标行落入新屏」所需的最小行数。
        let tab_id = "term-shrink-full";
        let bufs: TermBuffers = Arc::new(Mutex::new(HashMap::new()));
        let mut buf = TermBuffer {
            parser: vt100::Parser::new(40, 80, 5000),
            find_query: String::new(),
            sel: None,
            history: Vec::new(),
            prev: Vec::new(),
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
        };
        let mut feed = Vec::new();
        for i in 0..30 {
            feed.extend_from_slice(format!("line{i}\r\n").as_bytes());
        }
        buf.ingest(&feed); // 光标落在 row 30（空行）
        bufs.lock().unwrap().insert(tab_id.to_string(), buf);
        let last_size = Arc::new(Mutex::new((80u32, 40u32)));

        let (_, _, applied) = resize_terminal_buffer(tab_id, 80.0, 24.0, &bufs, &last_size);
        assert!(applied);

        let m = bufs.lock().unwrap();
        let b = &m[tab_id];
        let s = b.parser.screen();
        // need = (30+1) - 24 = 7：行 0..6 进 history，新屏顶行是 line7。
        assert_eq!(b.history.len(), 7);
        assert_eq!(b.history[0].0.trim_end(), "line0");
        assert_eq!(build_row(s, 0, 80).0.trim_end(), "line7");
        assert_eq!(s.cursor_position().0, 23);
    }

    #[test]
    fn ingest_large_scrolling_output_accumulates_history() {
        // 100 行输出到 40 行屏幕：约 60 行应滚入 history（复现「只能往上滚几行」）。
        let mut buf = TermBuffer {
            parser: vt100::Parser::new(40, 80, 5000),
            find_query: String::new(),
            sel: None,
            history: Vec::new(),
            prev: Vec::new(),
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
        };
        let mut feed = Vec::new();
        for i in 0..100 {
            feed.extend_from_slice(format!("line{i}\r\n").as_bytes());
        }
        buf.ingest(&feed);
        assert!(
            buf.history.len() >= 55,
            "history only accumulated {} lines (expected ~60)",
            buf.history.len()
        );
    }

    #[test]
    fn sftp_follows_terminal_cwd_only_when_not_manually_navigated() {
        // 自动模式：终端 cd 应驱动 SFTP 面板跟随（置 loading 并重新列目录）。
        assert!(sftp_should_follow_cwd(false));
        // 手动模式：用户已在面板里自行导航，终端 cd 不得触碰面板 —— 否则会把
        // sftp_loading 置 true 却无人复位（list_dir 被跳过），永久停在「加载中…」。
        assert!(!sftp_should_follow_cwd(true));
    }

    #[test]
    fn sftp_loading_settles_on_success_and_failure_but_not_on_progress() {
        // 不变量：能复位 sftp_loading 的，只有「加载终结」事件 —— 成功与失败都算。
        // 失败也必须复位，正是「刷新不了、一直加载中」的修复核心。
        assert!(settles_sftp_loading(&SessionEvent::SftpEntries {
            path: "/home".into(),
            entries: Vec::new(),
        }));
        assert!(settles_sftp_loading(&SessionEvent::SftpLoadFailed(
            "list directory failed: permission denied".into()
        )));
        // 发起加载 / 中途进度 / 无关状态都不得复位，否则会过早消除「加载中…」。
        assert!(!settles_sftp_loading(&SessionEvent::CwdChanged(
            "/root".into()
        )));
        assert!(!settles_sftp_loading(&SessionEvent::SftpStatus(
            "Loading /home...".into()
        )));
    }

    /// 取渲染结果中最顶行(row==0)的可见文本，按列拼接。
    fn top_line_text(b: &BuiltScreen) -> String {
        let mut cells: Vec<(i32, String)> = b
            .spans
            .iter()
            .filter(|sp| sp.row == 0)
            .map(|sp| (sp.col, sp.text.to_string()))
            .collect();
        cells.sort_by_key(|(c, _)| *c);
        cells
            .into_iter()
            .map(|(_, t)| t)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn scrollback_view_anchors_when_new_output_arrives_while_scrolled_up() {
        // 回归(bug: 滚到顶仍看不到顶部命令、画面自动下移)：view_offset 是"距底部
        // 行数"。滚轮拉到顶查看历史时，远端又来新输出会把若干行推入 history 底部；
        // 若 view_offset 不随之补偿，渲染窗口 start 增大、整屏向下漂移，最早的命令
        // 被挤出视野。不变量：回看历史时新输出不得移动已锚定的历史视图。
        let mut buf = TermBuffer {
            parser: vt100::Parser::new(40, 80, 5000),
            find_query: String::new(),
            sel: None,
            history: Vec::new(),
            prev: Vec::new(),
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
        };
        // 100 行内容(无尾随换行)灌满 40 行屏幕并把 line0.. 推入 history。
        let mut feed = String::new();
        for i in 0..100 {
            if i > 0 {
                feed.push_str("\r\n");
            }
            feed.push_str(&format!("line{i}"));
        }
        buf.ingest(feed.as_bytes());
        assert!(!buf.history.is_empty(), "应已累积 history");

        // 模拟滚轮拉到顶：view_offset = clamp 上界 history.len()。
        buf.view_offset = buf.history.len();
        let top_before = top_line_text(&buf.render());
        assert_eq!(top_before, "line0", "拉到顶应显示最早的一行 line0");

        // 看历史期间远端又产生输出，至少滚动一行。
        buf.ingest(b"\r\nline100");
        let top_after = top_line_text(&buf.render());

        assert_eq!(
            top_after, top_before,
            "看历史时新输出不得让视图向下漂移：顶部应仍锚定 line0"
        );
    }

    #[test]
    fn scroll_accumulator_ignores_subline_jitter_but_sums_to_whole_lines() {
        // 回归(bug: 上滑到顶后又莫名下跳三行)：触控板惯性末尾、到顶后边界回弹的
        // 亚行反向抖动不得移动视图，必须累积满一整行才翻行。
        let mut acc = 0.0f32;
        // 亚行正向：不足一行不滚，余数累积。
        assert_eq!(accumulate_scroll_lines(&mut acc, 0.3), 0);
        assert_eq!(accumulate_scroll_lines(&mut acc, 0.3), 0);
        assert_eq!(accumulate_scroll_lines(&mut acc, 0.3), 0); // 累计 ~0.9
        assert_eq!(accumulate_scroll_lines(&mut acc, 0.3), 1); // ~1.2 → 翻 1 行，余 ~0.2

        // 到顶后(余数≈0)的连串亚行反向抖动：累计仍不足一行 → 绝不回退。
        let mut top = 0.0f32;
        assert_eq!(accumulate_scroll_lines(&mut top, -0.2), 0);
        assert_eq!(accumulate_scroll_lines(&mut top, -0.2), 0);
        assert_eq!(accumulate_scroll_lines(&mut top, -0.4), 0); // 累计 ~-0.8

        // 鼠标滚轮一格(若干行)一次翻到位。
        let mut wheel = 0.0f32;
        assert_eq!(accumulate_scroll_lines(&mut wheel, 3.0), 3);
    }
}
