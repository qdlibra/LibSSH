slint::include_modules!();

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
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

pub fn run() -> anyhow::Result<()> {
    let runtime = Arc::new(Runtime::new()?);
    let store = Rc::new(RefCell::new(ConfigStore::load()?));
    crate::i18n::set_language(store.borrow().language());

    let window = AppWindow::new()?;
    crate::i18n::apply_to_slint();
    window.set_lang_en(crate::i18n::is_en());

    let models = initialise_models(&window, &store.borrow());
    let handles: Rc<RefCell<HashMap<String, SessionHandle>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let sftp_handles: SftpHandles = Arc::new(Mutex::new(HashMap::new()));
    let sftp_manual_nav: SftpManualNav = Arc::new(Mutex::new(HashMap::new()));
    let bufs: TermBuffers = Arc::new(Mutex::new(HashMap::new()));
    let tab_statuses: TabStatuses = Arc::new(Mutex::new(HashMap::new()));
    let local_snap: LocalSnap = Arc::new(Mutex::new(SystemSnapshot::default()));
    let local_net_hist: NetHist = Arc::new(Mutex::new(vec![0.0; NET_HISTORY_LEN]));
    let last_term_size: Arc<Mutex<(u32, u32)>> = Arc::new(Mutex::new((80, 24)));

    wire_callbacks(
        &window,
        store,
        models,
        runtime,
        handles,
        sftp_handles.clone(),
        sftp_manual_nav.clone(),
        bufs,
        tab_statuses.clone(),
        local_snap.clone(),
        local_net_hist.clone(),
        last_term_size,
    );
    register_file_drop(&window, sftp_handles);
    start_local_sampler(&window, tab_statuses, local_snap, local_net_hist);

    window.run()?;
    Ok(())
}

fn initialise_models(window: &AppWindow, store: &ConfigStore) -> AppModels {
    let sessions_model: Rc<VecModel<SessionInfo>> = Rc::new(VecModel::default());
    sync_sessions_to_model(store, &sessions_model);
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
    window.set_about_libs(ModelRc::from(Rc::new(VecModel::from(vec![
        "Rust".into(),
        "Slint".into(),
        "russh".into(),
        "russh-sftp".into(),
        "vt100".into(),
        "tokio".into(),
    ]))));

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

fn sync_sessions_to_model(store: &ConfigStore, model: &VecModel<SessionInfo>) {
    let rows: Vec<SessionInfo> = store
        .sessions()
        .iter()
        .map(|s| SessionInfo {
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
        })
        .collect();
    model.set_vec(rows);
}

fn wire_callbacks(
    window: &AppWindow,
    store: Rc<RefCell<ConfigStore>>,
    models: AppModels,
    runtime: Arc<Runtime>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    sftp_handles: SftpHandles,
    sftp_manual_nav: SftpManualNav,
    bufs: TermBuffers,
    tab_statuses: TabStatuses,
    local_snap: LocalSnap,
    local_net_hist: NetHist,
    last_term_size: Arc<Mutex<(u32, u32)>>,
) {
    let sessions_model = models.sessions.clone();
    let tabs_model = models.tabs.clone();
    let terminals_model = models.terminals.clone();

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

    let weak = window.as_weak();
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
            w.set_dialog_password("".into());
            w.set_dialog_key_path("".into());
            w.set_dialog_open(true);
        }
    });

    let weak = window.as_weak();
    let import_store = store.clone();
    let import_sessions = sessions_model.clone();
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
                });
                added += 1;
            }
            if added > 0 {
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
        }

        sync_sessions_to_model(&import_store.borrow(), &import_sessions);
        if let Some(w) = weak.upgrade() {
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
            w.set_dialog_password("".into());
            w.set_dialog_key_path(session.private_key_path.clone().into());
            w.set_dialog_editing(true);
            w.set_dialog_open(true);
        }
    });

    let weak = window.as_weak();
    let remove_store = store.clone();
    let remove_sessions = sessions_model.clone();
    window.on_remove_session(move |id: SharedString| {
        {
            let mut s = remove_store.borrow_mut();
            s.remove(&id.to_string());
            if let Err(err) = s.save() {
                tracing::warn!("failed to save config: {err:#}");
            }
        }
        sync_sessions_to_model(&remove_store.borrow(), &remove_sessions);
        if let Some(w) = weak.upgrade() {
            let _ = w.get_sessions();
        }
    });

    let weak = window.as_weak();
    let submit_store = store.clone();
    let submit_sessions = sessions_model.clone();
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
        let new_session = Session {
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
            auth: AuthMethod::from_str(&draft.auth.to_string()),
            password,
            private_key_path: draft.private_key_path.to_string().replace('\\', "/"),
            proxy: String::new(),
            last_used: None,
        };
        {
            let mut s = submit_store.borrow_mut();
            s.upsert(new_session);
            if let Err(err) = s.save() {
                tracing::warn!("failed to save config: {err:#}");
            }
        }
        sync_sessions_to_model(&submit_store.borrow(), &submit_sessions);
        if let Some(w) = weak.upgrade() {
            w.set_dialog_open(false);
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
    let connect_store = store.clone();
    let connect_tabs = tabs_model.clone();
    let connect_terminals = terminals_model.clone();
    let connect_handles = handles.clone();
    let connect_sftp_handles = sftp_handles.clone();
    let connect_sftp_manual_nav = sftp_manual_nav.clone();
    let connect_bufs = bufs.clone();
    let connect_runtime = runtime.clone();
    let connect_statuses = tab_statuses.clone();
    let connect_local = local_snap.clone();
    let connect_hist = local_net_hist.clone();
    let connect_last_size = last_term_size.clone();
    window.on_connect_session(move |id: SharedString| {
        let id = id.to_string();
        let weak = weak.clone();
        let connect_store = connect_store.clone();
        let connect_tabs = connect_tabs.clone();
        let connect_terminals = connect_terminals.clone();
        let connect_handles = connect_handles.clone();
        let connect_sftp_handles = connect_sftp_handles.clone();
        let connect_sftp_manual_nav = connect_sftp_manual_nav.clone();
        let connect_bufs = connect_bufs.clone();
        let connect_runtime = connect_runtime.clone();
        let connect_statuses = connect_statuses.clone();
        let connect_local = connect_local.clone();
        let connect_hist = connect_hist.clone();
        let connect_last_size = connect_last_size.clone();

        Timer::single_shot(std::time::Duration::from_millis(1), move || {
            let session = match connect_store.borrow().get(&id).cloned() {
                Some(s) => s,
                None => return,
            };
            let tab_id = format!("term-{}", uuid::Uuid::new_v4());
            connect_statuses.lock().unwrap().insert(
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
                find_matches: empty_model::<TermMatch>(),
                selection: empty_model::<TermMatch>(),
                sftp_path: "/".into(),
                sftp_entries: empty_model::<SftpEntry>(),
                sftp_status: crate::i18n::t("SFTP 连接中...", "SFTP connecting...").into(),
                sftp_loading: true,
                sftp_tree_nodes: empty_model::<SftpTreeNode>(),
            });
            connect_bufs.lock().unwrap().insert(
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
            connect_sftp_manual_nav
                .lock()
                .unwrap()
                .insert(tab_id.clone(), false);
            if let Some(w) = weak.upgrade() {
                w.set_active_tab_id(tab_id.clone().into());
            }
            schedule_sidebar_refresh(
                weak.clone(),
                connect_statuses.clone(),
                connect_local.clone(),
                connect_hist.clone(),
            );

            let (initial_cols, initial_rows) = *connect_last_size.lock().unwrap();
            let sftp_session = session.clone();
            let (handle, mut rx) = spawn_session(
                connect_runtime.handle(),
                tab_id.clone(),
                session,
                initial_cols,
                initial_rows,
            );
            connect_handles.borrow_mut().insert(tab_id.clone(), handle);

            let (sftp_tx, mut sftp_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
            let sftp_handle = spawn_sftp(connect_runtime.handle(), sftp_session, sftp_tx);
            connect_sftp_handles
                .lock()
                .unwrap()
                .insert(tab_id.clone(), sftp_handle);

            let weak_events = weak.clone();
            let bufs_events = connect_bufs.clone();
            let statuses_events = connect_statuses.clone();
            let local_events = connect_local.clone();
            let hist_events = connect_hist.clone();
            let sftp_handles_events = connect_sftp_handles.clone();
            let sftp_manual_events = connect_sftp_manual_nav.clone();
            let runtime_events = connect_runtime.clone();
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
                        if !is_manual {
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
                        }
                    }
                    let weak_evt = weak_events.clone();
                    let tab_evt = shell_tab_id.clone();
                    let bufs_evt = bufs_events.clone();
                    let statuses_evt = statuses_events.clone();
                    let local_evt = local_events.clone();
                    let hist_evt = hist_events.clone();
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
                            );
                        }
                    });
                }
            });

            let weak_events = weak.clone();
            let bufs_events = connect_bufs.clone();
            let statuses_events = connect_statuses.clone();
            let local_events = connect_local.clone();
            let hist_events = connect_hist.clone();
            let tab_events = tab_id.clone();
            std::thread::spawn(move || {
                while let Some(event) = sftp_rx.blocking_recv() {
                    let weak_evt = weak_events.clone();
                    let tab_evt = tab_events.clone();
                    let bufs_evt = bufs_events.clone();
                    let statuses_evt = statuses_events.clone();
                    let local_evt = local_events.clone();
                    let hist_evt = hist_events.clone();
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
                            );
                        }
                    });
                }
            });
        });
    });

    let weak = window.as_weak();
    let close_tabs = tabs_model.clone();
    let close_terminals = terminals_model.clone();
    let close_handles = handles.clone();
    let close_sftp_handles = sftp_handles.clone();
    let close_sftp_manual_nav = sftp_manual_nav.clone();
    let close_bufs = bufs.clone();
    let close_statuses = tab_statuses.clone();
    let close_local = local_snap.clone();
    let close_hist = local_net_hist.clone();
    window.on_tab_closed(move |id: SharedString| {
        let id = id.to_string();
        if id == "welcome" {
            return;
        }
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
    window.on_terminal_resize(move |tab_id: SharedString, cols_f: f32, rows_f: f32| {
        let tid = tab_id.to_string();
        let (cols, rows, should_rebuild) =
            resize_terminal_buffer(&tid, cols_f, rows_f, &resize_bufs, &resize_last_size);
        if let Some(handle) = resize_handles.borrow().get(tab_id.as_str()) {
            handle.resize(cols, rows);
        }
        if should_rebuild {
            schedule_terminal_display_rebuild(resize_weak.clone(), resize_bufs.clone(), tid);
        }
    });

    let send_handles = handles.clone();
    let send_bufs = bufs.clone();
    let last_shift_time: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
    window.on_send_key(
        move |tab_id: SharedString, key: SharedString, ctrl, alt, shift| {
            let key = key.to_string();
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
    let weak = window.as_weak();
    window.on_terminal_scroll(move |tab_id: SharedString, delta| {
        if let Some(buf) = scroll_bufs.lock().unwrap().get_mut(tab_id.as_str()) {
            let max_off = buf.history.len() as i64;
            let cur = buf.view_offset as i64;
            buf.view_offset = (cur + delta as i64).clamp(0, max_off) as usize;
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
    window.on_paste_from_clipboard(move |tab_id: SharedString| {
        let sender = paste_handles
            .borrow()
            .get(tab_id.as_str())
            .map(|h| h.commands.clone());
        let Some(sender) = sender else {
            return;
        };
        std::thread::spawn(move || {
            match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
                Ok(text) => {
                    let _ = sender.send(SessionCommand::RawInput(text.into_bytes()));
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

    let delete_sftp = sftp_handles.clone();
    window.on_sftp_delete(move |tab_id: SharedString, path: SharedString| {
        if let Ok(handles) = delete_sftp.lock() {
            if let Some(handle) = handles.get(tab_id.as_str()) {
                handle.delete(path.to_string());
            }
        }
    });

    let view_sftp = sftp_handles.clone();
    window.on_sftp_view(move |tab_id: SharedString, path: SharedString| {
        if let Ok(handles) = view_sftp.lock() {
            if let Some(handle) = handles.get(tab_id.as_str()) {
                handle.open_temp(path.to_string(), false);
            }
        }
    });

    let edit_sftp = sftp_handles.clone();
    window.on_sftp_edit(move |tab_id: SharedString, path: SharedString| {
        if let Ok(handles) = edit_sftp.lock() {
            if let Some(handle) = handles.get(tab_id.as_str()) {
                handle.open_temp(path.to_string(), true);
            }
        }
    });
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

fn apply_session_event_to_window(
    win: &AppWindow,
    tab_id: &str,
    event: SessionEvent,
    bufs: &TermBuffers,
    statuses: &TabStatuses,
    local: &LocalSnap,
    local_net_hist: &NetHist,
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
            update_terminal(&|t| t.status = crate::i18n::t("已连接", "Connected").into());
            if let Some(st) = statuses.lock().unwrap().get_mut(tab_id) {
                st.state = 1;
            }
            if win.get_active_tab_id().as_str() == tab_id {
                refresh_sidebar(win, statuses, local, local_net_hist);
            }
        }
        SessionEvent::Closed(reason) => {
            update_tab(&|t| t.connected = false);
            update_terminal(&|t| {
                t.status = format!("{} - {reason}", crate::i18n::t("已断开", "Disconnected")).into()
            });
            let show_failure_alert = {
                let mut statuses = statuses.lock().unwrap();
                let previous_state = statuses.get(tab_id).map(|st| st.state);
                if let Some(st) = statuses.get_mut(tab_id) {
                    st.state = 2;
                }
                should_show_connection_failed_alert(previous_state)
            };
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
                t.sftp_loading = false;
            });
        }
        SessionEvent::SftpStatus(msg) => {
            update_terminal(&|t| t.sftp_status = msg.clone().into());
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

    let mut should_rebuild = false;
    if let Some(buf) = bufs.lock().unwrap().get_mut(tab_id) {
        let old_rows = buf.parser.screen().size().0;
        if (rows as u16) < old_rows && !buf.parser.screen().alternate_screen() {
            let delta = old_rows - rows as u16;
            let cols_now = buf.parser.screen().size().1;
            let s = buf.parser.screen();
            for r in 0..delta {
                let line = build_row(s, r, cols_now);
                if line_has_visible_content(&line) {
                    buf.history.push(line);
                }
            }
            if buf.history.len() > MAX_HISTORY {
                let drop = buf.history.len() - MAX_HISTORY;
                buf.history.drain(0..drop);
            }
            let scroll_up = format!("\x1b[{delta}S");
            buf.parser.process(scroll_up.as_bytes());
        }
        buf.parser.set_size(rows as u16, cols as u16);
        buf.prev.clear();
        should_rebuild = true;
    }

    (cols, rows, should_rebuild)
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
            let s = if s.is_empty() { " ".to_string() } else { s };
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

const ANSI16: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00),
    (0xcd, 0x31, 0x31),
    (0x0d, 0xbc, 0x79),
    (0xe5, 0xe5, 0x10),
    (0x24, 0x72, 0xc8),
    (0xbc, 0x3f, 0xbc),
    (0x11, 0xa8, 0xcd),
    (0xe5, 0xe5, 0xe5),
    (0x66, 0x66, 0x66),
    (0xf1, 0x4c, 0x4c),
    (0x23, 0xd1, 0x8b),
    (0xf5, 0xf5, 0x43),
    (0x3b, 0x8e, 0xea),
    (0xd6, 0x70, 0xd6),
    (0x29, 0xb8, 0xdb),
    (0xff, 0xff, 0xff),
];

fn vt_color_to_slint(color: vt100::Color, bold: bool) -> slint::Color {
    let (r, g, b) = match color {
        vt100::Color::Default => (0xd4, 0xd4, 0xd4),
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(bufs.lock().unwrap()[tab_id].parser.screen().size(), (40, 120));
    }
}
