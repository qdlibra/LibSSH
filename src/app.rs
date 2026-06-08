slint::include_modules!();

use std::cell::RefCell;
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::config::{AuthMethod, ConfigStore, Secret, Session};
use crate::system::{format_bytes_per_sec, SystemSampler, SystemSnapshot};

const NET_HISTORY_LEN: usize = 60;

pub fn run() -> anyhow::Result<()> {
    let store = Rc::new(RefCell::new(ConfigStore::load()?));
    crate::i18n::set_language(store.borrow().language());

    let window = AppWindow::new()?;
    crate::i18n::apply_to_slint();
    window.set_lang_en(crate::i18n::is_en());

    let sessions_model = initialise_models(&window, &store.borrow());
    wire_callbacks(&window, store, sessions_model);
    start_local_sampler(&window);

    window.run()?;
    Ok(())
}

fn initialise_models(window: &AppWindow, store: &ConfigStore) -> Rc<VecModel<SessionInfo>> {
    let sessions_model: Rc<VecModel<SessionInfo>> = Rc::new(VecModel::default());
    sync_sessions_to_model(store, &sessions_model);
    window.set_sessions(ModelRc::from(sessions_model.clone()));

    let tabs_model: Rc<VecModel<TabInfo>> = Rc::new(VecModel::default());
    tabs_model.push(TabInfo {
        id: "welcome".into(),
        title: crate::i18n::t("新标签页", "New tab").into(),
        kind: "welcome".into(),
        connected: false,
    });
    window.set_tabs(ModelRc::from(tabs_model));
    window.set_active_tab_id("welcome".into());

    window.set_terminals(empty_model::<TerminalState>());
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
    sessions_model
}

fn start_local_sampler(window: &AppWindow) {
    let sampler = Rc::new(RefCell::new(SystemSampler::new()));
    let net_hist = Rc::new(RefCell::new(vec![0.0; NET_HISTORY_LEN]));

    {
        let snap = sampler.borrow_mut().sample();
        push_ring(&mut net_hist.borrow_mut(), snap.net_bytes_per_sec as f32);
        apply_local_snapshot(window, &snap, &net_hist.borrow());
    }

    let weak = window.as_weak();
    let tick_sampler = sampler.clone();
    let tick_hist = net_hist.clone();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, SystemSampler::recommended_interval(), move || {
        let snap = tick_sampler.borrow_mut().sample();
        {
            let mut hist = tick_hist.borrow_mut();
            push_ring(&mut hist, snap.net_bytes_per_sec as f32);
        }
        if let Some(w) = weak.upgrade() {
            apply_local_snapshot(&w, &snap, &tick_hist.borrow());
        }
    });
    Box::leak(Box::new(timer));
}

fn apply_local_snapshot(window: &AppWindow, snap: &SystemSnapshot, net_hist: &[f32]) {
    window.set_connection_state(crate::i18n::t("未连接", "Not connected").into());
    window.set_resource_title(crate::i18n::t("本机资源", "Local resources").into());
    window.set_conn_state(0);
    window.set_cpu_percent(snap.cpu_percent);
    window.set_mem_percent(snap.mem_percent);
    window.set_swap_percent(snap.swap_percent);
    window.set_mem_detail(format!("{}M/{}M", snap.mem_used_mib, snap.mem_total_mib).into());
    window.set_swap_detail(format!("{}M/{}M", snap.swap_used_mib, snap.swap_total_mib).into());

    window.set_net_top_up(format_bytes_per_sec(snap.net_tx_per_sec).into());
    window.set_net_top_down(format_bytes_per_sec(snap.net_rx_per_sec).into());
    window.set_net_bot_up(format_bytes_per_sec(snap.net_tx_per_sec).into());
    window.set_net_bot_down(format_bytes_per_sec(snap.net_rx_per_sec).into());
    let hist = normalized_model(net_hist);
    window.set_net_top_history(hist.clone());
    window.set_net_bot_history(hist);
    window.set_net_ifaces(empty_model::<SharedString>());
    window.set_net_selected("".into());
    window.set_net_show_selector(false);
    window.set_disks(disk_model(&snap.disks));
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
    sessions_model: Rc<VecModel<SessionInfo>>,
) {
    let weak = window.as_weak();
    window.on_new_tab_clicked(move || {
        if let Some(w) = weak.upgrade() {
            w.set_active_tab_id("welcome".into());
        }
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
            port: if draft.port <= 0 { 22 } else { draft.port as u16 },
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
        let mut dialog =
            rfd::FileDialog::new().set_title(crate::i18n::t("选择私钥文件", "Choose private key file"));
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

    window.on_tab_selected(|_| {});
    window.on_tab_closed(|_| {});
    window.on_connect_session(|_| {});
    window.on_refresh_sidebar(|| {});
    window.on_select_net_iface(|_| {});
    window.on_pick_download_dir(|| {});
    window.on_open_download_dir(|| {});
    window.on_clear_transfers(|| {});
    window.on_send_key(|_, _, _, _, _| {});
    window.on_terminal_resize(|_, _, _| {});
    window.on_sftp_navigate(|_, _| {});
    window.on_sftp_download(|_, _| {});
    window.on_sftp_upload_clicked(|_, _| {});
    window.on_sftp_refresh(|_, _| {});
    window.on_sftp_tree_expand(|_, _| {});
    window.on_sftp_delete(|_, _| {});
    window.on_sftp_view(|_, _| {});
    window.on_sftp_edit(|_, _| {});
    window.on_paste_from_clipboard(|_| {});
    window.on_copy_terminal_text(|_| {});
    window.on_clear_terminal(|_| {});
    window.on_find_query_changed(|_, _| {});
    window.on_terminal_scroll(|_, _| {});
    window.on_term_select_start(|_, _, _| {});
    window.on_term_select_update(|_, _, _| {});
    window.on_term_select_end(|_| {});
    window.on_term_select_autoscroll(|_, _| {});
}
