//! SFTP subsystem worker.
//!
//! Each terminal tab gets a separate SSH connection for SFTP so file transfers
//! cannot block the interactive shell PTY.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
use russh::client::{self, Handler};
use russh::Disconnect;
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use ssh_key::PublicKey;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::{AuthMethod, Session};
use crate::i18n::t;
use crate::ssh::{load_private_key_for_auth, RemoteEntry, RemoteTreeNode, SessionEvent};

#[derive(Debug)]
pub enum SftpCommand {
    ListDir(String),
    ToggleTreeNode(String),
    Download { remote: String, local_dir: String },
    Upload { local: String, remote_dir: String },
    UploadDir { local: String, remote_dir: String },
    Rename { from: String, new_name: String },
    CreateFile { dir: String, name: String },
    CreateDir { dir: String, name: String },
    Delete(String),
    ReadFile { remote: String },
    WriteFile { remote: String, content: String },
    Close,
}

#[derive(Debug, PartialEq)]
pub enum EditableError {
    TooLarge,
    NotUtf8,
}

/// 内嵌编辑器可打开的最大文件大小（5 MB）。
const MAX_EDIT_BYTES: usize = 5 * 1024 * 1024;

/// 远端文件内容是否适合在纯文本编辑器中打开。
/// 超过 `max_bytes` 或非 UTF-8 一律拒绝，避免把二进制读进编辑器、或保存时损坏文件。
pub fn check_editable(bytes: &[u8], max_bytes: usize) -> Result<String, EditableError> {
    if bytes.len() > max_bytes {
        return Err(EditableError::TooLarge);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.to_string()),
        Err(_) => Err(EditableError::NotUtf8),
    }
}

pub struct SftpHandle {
    pub commands: UnboundedSender<SftpCommand>,
    #[allow(dead_code)]
    pub join: JoinHandle<()>,
}

impl SftpHandle {
    pub fn list_dir(&self, path: String) {
        let _ = self.commands.send(SftpCommand::ListDir(path));
    }

    pub fn download(&self, remote: String, local_dir: String) {
        let _ = self
            .commands
            .send(SftpCommand::Download { remote, local_dir });
    }

    pub fn upload(&self, local: String, remote_dir: String) {
        let _ = self
            .commands
            .send(SftpCommand::Upload { local, remote_dir });
    }

    pub fn upload_dir(&self, local: String, remote_dir: String) {
        let _ = self
            .commands
            .send(SftpCommand::UploadDir { local, remote_dir });
    }

    pub fn rename(&self, from: String, new_name: String) {
        let _ = self.commands.send(SftpCommand::Rename { from, new_name });
    }

    pub fn create_file(&self, dir: String, name: String) {
        let _ = self.commands.send(SftpCommand::CreateFile { dir, name });
    }

    pub fn create_dir(&self, dir: String, name: String) {
        let _ = self.commands.send(SftpCommand::CreateDir { dir, name });
    }

    pub fn toggle_tree_node(&self, path: String) {
        let _ = self.commands.send(SftpCommand::ToggleTreeNode(path));
    }

    pub fn delete(&self, path: String) {
        let _ = self.commands.send(SftpCommand::Delete(path));
    }

    pub fn read_file(&self, remote: String) {
        let _ = self.commands.send(SftpCommand::ReadFile { remote });
    }

    pub fn write_file(&self, remote: String, content: String) {
        let _ = self
            .commands
            .send(SftpCommand::WriteFile { remote, content });
    }

    pub fn close(&self) {
        let _ = self.commands.send(SftpCommand::Close);
    }
}

pub fn spawn_sftp(
    runtime: &tokio::runtime::Handle,
    session: Session,
    events: UnboundedSender<SessionEvent>,
) -> SftpHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let events_err = events.clone();
    let join = runtime.spawn(async move {
        if let Err(err) = run_sftp(session, cmd_rx, events).await {
            // 连接/认证/握手失败：tab 初始化时 sftp_loading=true，这里必须发
            // SftpLoadFailed 复位，否则面板卡在「加载中…」。
            let _ = events_err.send(SessionEvent::SftpLoadFailed(format!(
                "{}: {err:#}",
                t("SFTP 错误", "SFTP error")
            )));
        }
    });
    SftpHandle {
        commands: cmd_tx,
        join,
    }
}

async fn run_sftp(
    session: Session,
    mut commands: UnboundedReceiver<SftpCommand>,
    events: UnboundedSender<SessionEvent>,
) -> Result<()> {
    let _ = events.send(SessionEvent::SftpStatus(
        t("SFTP 连接中...", "SFTP connecting...").into(),
    ));

    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(60 * 30)),
        ..<_>::default()
    });
    let addr = format!("{}:{}", session.host, session.port);
    let mut handle = match crate::proxy::resolve(&session.proxy) {
        Some(proxy) => {
            let stream = crate::proxy::connect(&proxy, &session.host, session.port)
                .await
                .with_context(|| format!("sftp proxy connect {} failed", addr))?;
            client::connect_stream(config, stream, SftpClientHandler)
                .await
                .with_context(|| format!("sftp connect {} failed", addr))?
        }
        None => client::connect(config, addr.as_str(), SftpClientHandler)
            .await
            .with_context(|| format!("sftp connect {} failed", addr))?,
    };

    let authed = match session.auth {
        AuthMethod::Password => handle
            .authenticate_password(&session.user, session.password.as_str())
            .await
            .context("sftp password auth failed")?,
        AuthMethod::Key => {
            let key_with_hash = load_private_key_for_auth(&session.private_key_path)
                .context("invalid private key")?;
            handle
                .authenticate_publickey(&session.user, key_with_hash)
                .await
                .context("sftp publickey auth failed")?
        }
    };

    if !authed {
        return Err(anyhow!(t("SFTP 认证失败", "SFTP authentication failed")));
    }

    let channel = handle
        .channel_open_session()
        .await
        .context("open sftp channel")?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("request sftp subsystem")?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .context("sftp handshake")?;

    let home = sftp
        .canonicalize(".")
        .await
        .unwrap_or_else(|_| "/".to_string());
    let _ = events.send(SessionEvent::SftpStatus(format!(
        "{} {}...",
        t("SFTP 加载", "SFTP loading"),
        home
    )));
    match list_dir_impl(&sftp, &home).await {
        Ok(entries) => {
            let _ = events.send(SessionEvent::SftpEntries {
                path: home.clone(),
                entries,
            });
            let _ = events.send(SessionEvent::SftpStatus(home.clone()));
        }
        Err(e) => {
            // 初始 home 目录列举失败：同样复位 loading，而非只更新状态文本。
            let _ = events.send(SessionEvent::SftpLoadFailed(format!(
                "{}: {e}",
                t("SFTP 错误", "SFTP error")
            )));
        }
    }

    let mut tree_dirs: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut tree_expanded: HashSet<String> = HashSet::new();
    let root_dirs = list_dirs_only_impl(&sftp, "/").await.unwrap_or_default();
    tree_dirs.insert("/".to_string(), root_dirs);
    tree_expanded.insert("/".to_string());

    if home != "/" {
        let mut current = "/".to_string();
        for segment in home.trim_start_matches('/').split('/') {
            if segment.is_empty() {
                continue;
            }
            let child = format!("{}/{}", current.trim_end_matches('/'), segment);
            let found = tree_dirs
                .get(&current)
                .map(|c| c.iter().any(|(_, p)| p == &child))
                .unwrap_or(false);
            if !found {
                break;
            }
            let dirs = list_dirs_only_impl(&sftp, &child).await.unwrap_or_default();
            tree_dirs.insert(child.clone(), dirs);
            tree_expanded.insert(child.clone());
            current = child;
        }
    }
    emit_tree(&events, &tree_expanded, &tree_dirs);

    while let Some(cmd) = commands.recv().await {
        match cmd {
            SftpCommand::Close => break,
            SftpCommand::ListDir(path) => {
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("加载", "Loading"),
                    path
                )));
                match list_dir_impl(&sftp, &path).await {
                    Ok(entries) => {
                        let _ = events.send(SessionEvent::SftpEntries {
                            path: path.clone(),
                            entries,
                        });
                        let _ = events.send(SessionEvent::SftpStatus(path));
                    }
                    Err(e) => {
                        // 后续 ListDir 失败（如终端 cd 进了 SFTP 用户无权访问的
                        // 目录）：发 SftpLoadFailed 复位 loading 并回显原因，否则
                        // 面板永久「加载中…」，且刷新会重复同一次失败、毫无反馈。
                        let _ = events.send(SessionEvent::SftpLoadFailed(format!(
                            "{}: {e}",
                            t("列目录失败", "list directory failed")
                        )));
                    }
                }
            }
            SftpCommand::ToggleTreeNode(path) => {
                if tree_expanded.contains(&path) {
                    let prefix = format!("{}/", path.trim_end_matches('/'));
                    tree_expanded.retain(|p| p != &path && !p.starts_with(&prefix));
                } else {
                    if !tree_dirs.contains_key(&path) {
                        let dirs = list_dirs_only_impl(&sftp, &path).await.unwrap_or_default();
                        tree_dirs.insert(path.clone(), dirs);
                    }
                    tree_expanded.insert(path);
                }
                emit_tree(&events, &tree_expanded, &tree_dirs);
            }
            SftpCommand::Download { remote, local_dir } => {
                let filename = base_name(&remote);
                let is_dir = sftp
                    .metadata(remote.as_str())
                    .await
                    .ok()
                    .and_then(|m| m.permissions)
                    .map(|p| (p & 0o170_000) == 0o040_000)
                    .unwrap_or(false);
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("下载", "Downloading"),
                    filename
                )));
                if is_dir {
                    match download_dir_impl(&sftp, &remote, &local_dir, &events).await {
                        Ok((ok, 0)) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {} ({} {})",
                                t("下载完成", "Downloaded"),
                                filename,
                                ok,
                                t("个文件", "files")
                            )));
                        }
                        Ok((ok, failed)) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {} {} / {} {}",
                                t("下载结束", "Download finished"),
                                ok,
                                t("成功", "ok"),
                                failed,
                                t("失败", "failed")
                            )));
                        }
                        Err(e) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}",
                                t("下载失败", "Download failed")
                            )));
                        }
                    }
                } else {
                    let local_path = format!("{}/{}", local_dir.trim_end_matches('/'), filename);
                    let id = Uuid::new_v4().to_string();
                    match download_impl(&sftp, &remote, &local_path, &filename, &id, &events).await
                    {
                        Ok(()) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("下载完成", "Downloaded"),
                                filename
                            )));
                        }
                        Err(e) => {
                            emit_transfer(&events, &id, &filename, false, 0, 0, 2, &e.to_string());
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}",
                                t("下载失败", "Download failed")
                            )));
                        }
                    }
                }
            }
            SftpCommand::Upload { local, remote_dir } => {
                let filename = base_name(&local);
                let remote_path = format!("{}/{}", remote_dir.trim_end_matches('/'), filename);
                let id = Uuid::new_v4().to_string();
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("上传", "Uploading"),
                    filename
                )));
                match upload_pipelined(&handle, &local, &remote_path, &filename, &id, &events).await
                {
                    Ok(()) => {
                        if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                            let _ = events.send(SessionEvent::SftpEntries {
                                path: remote_dir.clone(),
                                entries,
                            });
                        }
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("上传完成", "Uploaded"),
                            filename
                        )));
                    }
                    Err(e) => {
                        emit_transfer(&events, &id, &filename, true, 0, 0, 2, &e.to_string());
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("上传失败", "Upload failed")
                        )));
                    }
                }
            }
            SftpCommand::UploadDir { local, remote_dir } => {
                let dirname = base_name(&local);
                let remote_root = join_remote(&remote_dir, &dirname);
                let mut dirs: Vec<String> = vec![remote_root.clone()];
                let mut files: Vec<(String, String)> = Vec::new();
                collect_upload_items(
                    std::path::Path::new(&local),
                    &remote_root,
                    0,
                    &mut dirs,
                    &mut files,
                );
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {} ({} {})...",
                    t("上传文件夹", "Uploading folder"),
                    dirname,
                    files.len(),
                    t("个文件", "files")
                )));
                // create_dir on an existing dir fails — ignore so re-uploads merge.
                for d in &dirs {
                    let _ = sftp.create_dir(d.as_str()).await;
                }
                let mut ok = 0usize;
                let mut failed = 0usize;
                for (lp, rp) in &files {
                    let fname = base_name(rp);
                    let id = Uuid::new_v4().to_string();
                    match upload_pipelined(&handle, lp, rp, &fname, &id, &events).await {
                        Ok(()) => ok += 1,
                        Err(e) => {
                            failed += 1;
                            emit_transfer(&events, &id, &fname, true, 0, 0, 2, &e.to_string());
                        }
                    }
                }
                if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                    let _ = events.send(SessionEvent::SftpEntries {
                        path: remote_dir.clone(),
                        entries,
                    });
                }
                let _ = events.send(SessionEvent::SftpStatus(if failed == 0 {
                    format!(
                        "{}: {} ({} {})",
                        t("上传完成", "Uploaded"),
                        dirname,
                        ok,
                        t("个文件", "files")
                    )
                } else {
                    format!(
                        "{}: {} {} / {} {}",
                        t("上传结束", "Upload finished"),
                        ok,
                        t("成功", "ok"),
                        failed,
                        t("失败", "failed")
                    )
                }));
            }
            SftpCommand::Rename { from, new_name } => {
                let to = join_remote(&parent_dir(&from), &new_name);
                match sftp.rename(from.as_str(), to.as_str()).await {
                    Ok(_) => {
                        let parent = parent_dir(&from);
                        if let Ok(entries) = list_dir_impl(&sftp, &parent).await {
                            let _ = events.send(SessionEvent::SftpEntries {
                                path: parent,
                                entries,
                            });
                        }
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已重命名", "Renamed"),
                            new_name
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("重命名失败", "Rename failed")
                        )));
                    }
                }
            }
            SftpCommand::CreateFile { dir, name } => {
                let path = join_remote(&dir, &name);
                // Refuse to clobber an existing entry — sftp.write() would
                // silently truncate it.
                if sftp.metadata(path.as_str()).await.is_ok() {
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{}: {}",
                        t("已存在同名文件", "Already exists"),
                        name
                    )));
                } else {
                    match sftp.write(path.as_str(), b"").await {
                        Ok(()) => {
                            if let Ok(entries) = list_dir_impl(&sftp, &dir).await {
                                let _ = events.send(SessionEvent::SftpEntries {
                                    path: dir.clone(),
                                    entries,
                                });
                            }
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("已新建文件", "File created"),
                                name
                            )));
                        }
                        Err(e) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}",
                                t("新建文件失败", "Create file failed")
                            )));
                        }
                    }
                }
            }
            SftpCommand::CreateDir { dir, name } => {
                let path = join_remote(&dir, &name);
                match sftp.create_dir(path.as_str()).await {
                    Ok(_) => {
                        if let Ok(entries) = list_dir_impl(&sftp, &dir).await {
                            let _ = events.send(SessionEvent::SftpEntries {
                                path: dir.clone(),
                                entries,
                            });
                        }
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已新建文件夹", "Folder created"),
                            name
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("新建文件夹失败", "Create folder failed")
                        )));
                    }
                }
            }
            SftpCommand::Delete(path) => {
                let filename = base_name(&path);
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("删除", "Deleting"),
                    filename
                )));
                let res = match sftp.remove_file(&path).await {
                    Ok(_) => Ok(()),
                    Err(_) => sftp.remove_dir(&path).await.map(|_| ()),
                };
                match res {
                    Ok(()) => {
                        let parent = parent_dir(&path);
                        if let Ok(entries) = list_dir_impl(&sftp, &parent).await {
                            let _ = events.send(SessionEvent::SftpEntries {
                                path: parent,
                                entries,
                            });
                        }
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已删除", "Deleted"),
                            filename
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("删除失败", "Delete failed")
                        )));
                    }
                }
            }
            SftpCommand::ReadFile { remote } => {
                let filename = base_name(&remote);
                // 先看大小，避免把超大文件整体读进内存。
                let too_big = sftp
                    .metadata(remote.as_str())
                    .await
                    .ok()
                    .and_then(|m| m.size)
                    .map(|sz| sz as usize > MAX_EDIT_BYTES)
                    .unwrap_or(false);
                if too_big {
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{}: {}",
                        t("文件过大，无法编辑", "File too large to edit"),
                        filename
                    )));
                } else {
                    match sftp.read(remote.as_str()).await {
                        Ok(bytes) => match check_editable(&bytes, MAX_EDIT_BYTES) {
                            Ok(content) => {
                                let _ = events.send(SessionEvent::SftpFileContent {
                                    remote: remote.clone(),
                                    filename,
                                    content,
                                });
                            }
                            Err(EditableError::TooLarge) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("文件过大，无法编辑", "File too large to edit"),
                                    filename
                                )));
                            }
                            Err(EditableError::NotUtf8) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t(
                                        "二进制或非 UTF-8 文件，暂不支持编辑",
                                        "Binary / non-UTF-8 file, not editable"
                                    ),
                                    filename
                                )));
                            }
                        },
                        Err(e) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}",
                                t("打开失败", "Open failed")
                            )));
                        }
                    }
                }
            }
            SftpCommand::WriteFile { remote, content } => {
                let filename = base_name(&remote);
                match sftp.write(remote.as_str(), content.as_bytes()).await {
                    Ok(()) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已保存", "Saved"),
                            filename
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("保存失败", "Save failed")
                        )));
                    }
                }
            }
        }
    }

    let _ = handle
        .disconnect(Disconnect::ByApplication, "bye", "")
        .await;
    Ok(())
}

fn emit_tree(
    events: &UnboundedSender<SessionEvent>,
    expanded: &HashSet<String>,
    tree_dirs: &HashMap<String, Vec<(String, String)>>,
) {
    let mut nodes = Vec::new();
    build_tree_nodes("/", 0, expanded, tree_dirs, &mut nodes);
    let _ = events.send(SessionEvent::SftpTreeUpdate(nodes));
}

fn build_tree_nodes(
    path: &str,
    depth: u32,
    expanded: &HashSet<String>,
    tree_dirs: &HashMap<String, Vec<(String, String)>>,
    nodes: &mut Vec<RemoteTreeNode>,
) {
    let name = if path == "/" {
        "/".to_string()
    } else {
        path.rsplit('/').next().unwrap_or(path).to_string()
    };
    let children = tree_dirs.get(path);
    let has_children = children.map(|c| !c.is_empty()).unwrap_or(true);
    let is_expanded = expanded.contains(path);
    nodes.push(RemoteTreeNode {
        path: path.to_string(),
        name,
        depth,
        expanded: is_expanded,
        has_children,
    });
    if is_expanded {
        if let Some(children) = children {
            for (_, child_path) in children {
                build_tree_nodes(child_path, depth + 1, expanded, tree_dirs, nodes);
            }
        }
    }
}

async fn list_dir_impl(sftp: &SftpSession, path: &str) -> Result<Vec<RemoteEntry>> {
    let raw = sftp
        .read_dir(path)
        .await
        .with_context(|| format!("read_dir {path} failed"))?;
    let mut entries: Vec<RemoteEntry> = raw
        .into_iter()
        .filter(|e| {
            let name = e.file_name();
            name != "." && name != ".."
        })
        .map(|e| {
            let name = e.file_name().to_string();
            let full_path = format!("{}/{}", path.trim_end_matches('/'), name);
            let meta = e.metadata();
            let permissions = meta.permissions.unwrap_or(0);
            let is_dir = (permissions & 0o170_000) == 0o040_000;
            RemoteEntry {
                name,
                full_path,
                is_dir,
                size: meta.size.unwrap_or(0),
                modified: meta.mtime.unwrap_or(0),
            }
        })
        .collect();
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    Ok(entries)
}

async fn list_dirs_only_impl(sftp: &SftpSession, path: &str) -> Result<Vec<(String, String)>> {
    Ok(list_dir_impl(sftp, path)
        .await?
        .into_iter()
        .filter(|e| e.is_dir)
        .map(|e| (e.name, e.full_path))
        .collect())
}

#[allow(clippy::too_many_arguments)]
fn emit_transfer(
    events: &UnboundedSender<SessionEvent>,
    id: &str,
    name: &str,
    is_upload: bool,
    transferred: u64,
    total: u64,
    state: u8,
    msg: &str,
) {
    let _ = events.send(SessionEvent::SftpTransfer {
        id: id.to_string(),
        name: name.to_string(),
        is_upload,
        transferred,
        total,
        state,
        msg: msg.to_string(),
    });
}

async fn download_impl(
    sftp: &SftpSession,
    remote: &str,
    local: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const CHUNK: usize = 64 * 1024;
    let total = sftp
        .metadata(remote)
        .await
        .ok()
        .and_then(|m| m.size)
        .unwrap_or(0);
    let mut remote_file = sftp
        .open(remote)
        .await
        .with_context(|| format!("open remote {remote}"))?;
    let mut local_file = tokio::fs::File::create(local)
        .await
        .with_context(|| format!("create local {local}"))?;
    emit_transfer(events, id, name, false, 0, total, 0, "");

    let mut buf = vec![0; CHUNK];
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        let n = remote_file
            .read(&mut buf)
            .await
            .context("read remote file")?;
        if n == 0 {
            break;
        }
        local_file
            .write_all(&buf[..n])
            .await
            .context("write local file")?;
        done += n as u64;
        if last.elapsed() >= Duration::from_millis(150) {
            last = Instant::now();
            emit_transfer(events, id, name, false, done, total, 0, "");
        }
    }
    local_file.flush().await.context("flush local file")?;
    emit_transfer(events, id, name, false, done, total.max(done), 1, "");
    Ok(())
}

async fn upload_pipelined(
    handle: &client::Handle<SftpClientHandler>,
    local: &str,
    remote: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    const CHUNK: usize = 32 * 1024;
    const MAX_INFLIGHT: usize = 32;

    let total = tokio::fs::metadata(local)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mut local_file = tokio::fs::File::open(local)
        .await
        .with_context(|| format!("open local {local}"))?;
    let channel = handle
        .channel_open_session()
        .await
        .context("open sftp upload channel")?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("request sftp subsystem")?;
    let raw = Arc::new(RawSftpSession::new(channel.into_stream()));
    raw.init().await.context("sftp upload handshake")?;
    let fhandle = raw
        .open(
            remote,
            OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE,
            FileAttributes::default(),
        )
        .await
        .with_context(|| format!("create remote {remote}"))?
        .handle;

    emit_transfer(events, id, name, true, 0, total, 0, "");
    let mut offset = 0u64;
    let mut done = 0u64;
    let mut last = Instant::now();
    let mut eof = false;
    let mut err: Option<anyhow::Error> = None;
    let mut inflight = FuturesUnordered::new();

    while !eof || !inflight.is_empty() {
        while !eof && inflight.len() < MAX_INFLIGHT {
            let mut buf = vec![0; CHUNK];
            match local_file.read(&mut buf).await {
                Ok(0) => eof = true,
                Ok(n) => {
                    buf.truncate(n);
                    let off = offset;
                    offset += n as u64;
                    let raw2 = raw.clone();
                    let h = fhandle.clone();
                    inflight.push(async move { raw2.write(h, off, buf).await.map(|_| n as u64) });
                }
                Err(e) => {
                    err = Some(anyhow!("read local file: {e}"));
                    eof = true;
                }
            }
        }
        match inflight.next().await {
            Some(Ok(n)) => {
                done += n;
                if last.elapsed() >= Duration::from_millis(150) {
                    last = Instant::now();
                    emit_transfer(events, id, name, true, done, total, 0, "");
                }
            }
            Some(Err(e)) => {
                err = Some(anyhow!("write remote file: {e}"));
                eof = true;
            }
            None => {}
        }
        if err.is_some() {
            break;
        }
    }

    let _ = raw.close(fhandle).await;
    if let Some(e) = err {
        return Err(e);
    }
    emit_transfer(events, id, name, true, done, total.max(done), 1, "");
    Ok(())
}

/// 在远端路径下拼接子项名（统一处理根目录的结尾斜杠）。
fn join_remote(dir: &str, name: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), name)
}

/// 递归收集本地目录的上传清单：`dirs` 收远端需要创建的目录，`files` 收
/// (本地路径, 远端路径) 文件对。深度限制防 symlink 自环。
fn collect_upload_items(
    local_root: &std::path::Path,
    remote_root: &str,
    depth: u32,
    dirs: &mut Vec<String>,
    files: &mut Vec<(String, String)>,
) {
    if depth > 32 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(local_root) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let rp = join_remote(remote_root, &name);
        // std::fs::metadata follows symlinks so linked dirs/files upload too.
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            dirs.push(rp.clone());
            collect_upload_items(&p, &rp, depth + 1, dirs, files);
        } else if meta.is_file() {
            files.push((p.to_string_lossy().to_string(), rp));
        }
    }
}

/// 递归下载远端目录到本地 `local_dir` 下的同名文件夹。
/// 返回 (成功文件数, 失败文件数)；无权限的子目录静默跳过。
async fn download_dir_impl(
    sftp: &SftpSession,
    remote_root: &str,
    local_dir: &str,
    events: &UnboundedSender<SessionEvent>,
) -> Result<(usize, usize)> {
    let root_name = base_name(remote_root);
    let local_root = format!("{}/{}", local_dir.trim_end_matches('/'), root_name);
    let mut queue = vec![(remote_root.to_string(), local_root)];
    let mut files: Vec<(String, String)> = Vec::new();
    let mut depth = 0;
    while !queue.is_empty() && depth <= 32 {
        let mut next = Vec::new();
        for (rdir, ldir) in queue {
            std::fs::create_dir_all(&ldir).with_context(|| format!("create local dir {ldir}"))?;
            for e in list_dir_impl(sftp, &rdir).await.unwrap_or_default() {
                let lpath = format!("{}/{}", ldir, e.name);
                if e.is_dir {
                    next.push((e.full_path, lpath));
                } else {
                    files.push((e.full_path, lpath));
                }
            }
        }
        queue = next;
        depth += 1;
    }
    let mut ok = 0usize;
    let mut failed = 0usize;
    for (rp, lp) in &files {
        let name = base_name(rp);
        let id = Uuid::new_v4().to_string();
        match download_impl(sftp, rp, lp, &name, &id, events).await {
            Ok(()) => ok += 1,
            Err(e) => {
                failed += 1;
                emit_transfer(events, &id, &name, false, 0, 0, 2, &e.to_string());
            }
        }
    }
    Ok((ok, failed))
}

fn base_name(path: &str) -> String {
    let sep = |c: char| c == '/' || c == '\\';
    path.trim_end_matches(sep)
        .rsplit(sep)
        .next()
        .unwrap_or(path)
        .to_string()
}

fn parent_dir(path: &str) -> String {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

struct SftpClientHandler;

#[async_trait]
impl Handler for SftpClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn data(
        &mut self,
        _channel: russh::ChannelId,
        _data: &[u8],
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_name_handles_remote_and_windows_paths() {
        assert_eq!(base_name("/var/log/syslog"), "syslog");
        assert_eq!(base_name("/var/log/"), "log");
        assert_eq!(base_name(r"C:\Users\me\file.txt"), "file.txt");
        assert_eq!(base_name("plain"), "plain");
    }

    #[test]
    fn parent_dir_clamps_at_root() {
        assert_eq!(parent_dir("/var/log/syslog"), "/var/log");
        assert_eq!(parent_dir("/var/log/"), "/var");
        assert_eq!(parent_dir("/file"), "/");
        assert_eq!(parent_dir("/"), "/");
        assert_eq!(parent_dir("relative"), "/");
    }

    #[test]
    fn join_remote_handles_root_and_nested() {
        assert_eq!(join_remote("/", "etc"), "/etc");
        assert_eq!(join_remote("/var/log", "syslog"), "/var/log/syslog");
        assert_eq!(join_remote("/var/log/", "syslog"), "/var/log/syslog");
    }

    #[test]
    fn collect_upload_items_walks_nested_dirs() {
        // 构造临时目录: root/{a.txt, sub/{b.txt}}
        let root = std::env::temp_dir().join(format!("libssh-up-{}", std::process::id()));
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(sub.join("b.txt"), b"b").unwrap();

        let mut dirs = Vec::new();
        let mut files = Vec::new();
        collect_upload_items(&root, "/up/root", 0, &mut dirs, &mut files);
        files.sort();

        assert_eq!(dirs, vec!["/up/root/sub".to_string()]);
        assert_eq!(files.len(), 2);
        assert!(files[0].0.ends_with("a.txt"));
        assert_eq!(files[0].1, "/up/root/a.txt");
        assert!(files[1].0.ends_with("b.txt"));
        assert_eq!(files[1].1, "/up/root/sub/b.txt");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn check_editable_accepts_small_utf8() {
        assert_eq!(check_editable(b"hello", 1024).unwrap(), "hello");
        assert_eq!(check_editable(b"", 1024).unwrap(), "");
    }

    #[test]
    fn check_editable_rejects_too_large() {
        assert!(matches!(
            check_editable(b"abcd", 2),
            Err(EditableError::TooLarge)
        ));
    }

    #[test]
    fn check_editable_rejects_non_utf8() {
        // 0xFF 不是合法 UTF-8 起始字节
        assert!(matches!(
            check_editable(&[0xff, 0xfe, 0x00], 1024),
            Err(EditableError::NotUtf8)
        ));
    }
}
