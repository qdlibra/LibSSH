slint::include_modules!();

use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

pub fn run() -> anyhow::Result<()> {
    let store = crate::config::ConfigStore::load()?;
    crate::i18n::set_language(store.language());

    let window = AppWindow::new()?;
    crate::i18n::apply_to_slint();
    window.set_lang_en(crate::i18n::is_en());

    initialise_models(&window, &store);
    wire_m2_callbacks(&window);

    window.run()?;
    Ok(())
}

fn initialise_models(window: &AppWindow, store: &crate::config::ConfigStore) {
    let sessions_model: Rc<VecModel<SessionInfo>> = Rc::new(VecModel::default());
    sync_sessions_to_model(store, &sessions_model);
    window.set_sessions(ModelRc::from(sessions_model));

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
}

fn empty_model<T: 'static + Clone + Default>() -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::<T>::default()))
}

fn sync_sessions_to_model(store: &crate::config::ConfigStore, model: &VecModel<SessionInfo>) {
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

fn wire_m2_callbacks(window: &AppWindow) {
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
            w.set_dialog_editing(false);
            w.set_dialog_id("".into());
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
    window.on_set_language(move |code| {
        crate::i18n::set_language(code.as_str());
        if let Some(w) = weak.upgrade() {
            w.set_lang_en(crate::i18n::is_en());
            w.set_connection_state(crate::i18n::t("未连接", "Not connected").into());
            w.set_resource_title(crate::i18n::t("本机资源", "Local resources").into());
        }
    });

    window.on_tab_selected(|_| {});
    window.on_tab_closed(|_| {});
    window.on_import_ssh_config(|| {});
    window.on_connect_session(|_| {});
    window.on_edit_session(|_| {});
    window.on_remove_session(|_| {});
    window.on_session_dialog_submit(|_| {});
    window.on_session_dialog_pick_key(|| {});
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
