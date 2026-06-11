# LibSSH

LibSSH 是一个使用 Rust + Slint 编写的轻量级 SSH / SFTP 桌面客户端，目标是提供接近 FinalShell 的本地体验：会话管理、标签页终端、SFTP 文件面板、远端文本编辑、系统资源侧栏、快捷命令和自动更新。

项目还内置了一个受策略保护的 `LibSSH skill ...` CLI，供 Codex、Claude Code 等 AI 编程工具在用户授权后安全地操作已保存的远程主机。CLI 与 GUI 共用同一个二进制，凭据始终由 LibSSH 本地读取和脱敏输出。

## 功能特性

- SSH 会话管理：新增、编辑、删除、快速连接、延迟探测。
- 认证方式：密码认证、私钥认证；私钥路径支持 `~` 展开和 `.pub` 后缀归一化。
- 代理连接：支持 `socks5://` / `socks5h://` / `http://` / `https://`，可在会话中配置，也可使用 `ALL_PROXY` / `all_proxy`。
- 标签页终端：PTY 尺寸同步、滚动历史、复制/粘贴、选择复制、搜索高亮、清屏、断线重连。
- SFTP 面板：目录树、文件列表、刷新、上传文件、上传文件夹、下载、重命名、新建文件、新建目录、删除、复制远端路径。
- 内置远端文本编辑器：可打开和保存 UTF-8 文本文件，默认拒绝超过 5 MB 或非 UTF-8 的内容。
- 快捷命令：底部命令栏支持历史建议和用户自定义快捷命令。
- 本机资源侧栏：CPU、内存、交换分区、网络速率、磁盘空间。
- 国际化：中文 / English。
- 主题：跟随系统明暗主题。
- 运行日志浮层：收集 WARN / ERROR，便于无控制台环境排障。
- 自动更新：从 GitHub Releases 检查新版本，macOS 支持下载、SHA256 校验和安装引导。
- AI Skill CLI：受 allow/deny 策略和内置安全规则保护，面向 AI Agent 的远程命令执行入口。

## 技术栈

- Rust 2021，最低 Rust 版本：`1.75`
- UI：Slint
- SSH：`russh` / `russh-keys`
- SFTP：`russh-sftp`
- 异步运行时：Tokio
- 终端解析：`vt100`
- 配置与持久化：`serde` / `serde_json` / `directories`
- 系统凭据库：`keyring`
- 自动更新：`reqwest` + `semver` + `sha2`

## 快速开始

### 环境要求

安装 Rust stable：

```bash
rustup toolchain install stable
```

Linux 需要安装 Slint / winit 常用系统依赖，以及 keyring（Secret Service）所需的 D-Bus 开发库。Ubuntu 示例：

```bash
sudo apt-get update
sudo apt-get install -y \
  libfontconfig1-dev \
  libxkbcommon-dev \
  libwayland-dev \
  libxcb1-dev \
  libxcb-render0-dev \
  libxcb-shape0-dev \
  libxcb-xfixes0-dev \
  libdbus-1-dev
```

### 构建与运行

```bash
cargo run
cargo build --release
cargo test
```

Release 二进制位置：

```text
target/release/LibSSH
```

Windows release 构建会生成 `LibSSH.exe`。

## 安装

### macOS

构建并打包 `.app` / `.dmg`：

```bash
cargo build --release
scripts/package-macos-dmg.sh target/release/LibSSH dist
```

产物示例：

```text
dist/LibSSH.app
dist/LibSSH-macos-arm64.dmg
dist/LibSSH-macos-x86_64.dmg
```

如果打开未签名构建时被系统拦截，可清除隔离标记：

```bash
xattr -dr com.apple.quarantine LibSSH.app
```

### Linux

安装到用户目录：

```bash
cargo build --release
assets/install-linux.sh target/release/LibSSH
```

默认安装位置：

```text
~/.local/bin/LibSSH
~/.local/share/applications/LibSSH.desktop
~/.local/share/icons/hicolor/512x512/apps/LibSSH.png
```

也可以通过 `PREFIX` 指定安装前缀：

```bash
PREFIX=/opt/libssh assets/install-linux.sh target/release/LibSSH
```

### Windows

```bash
cargo build --release
```

运行：

```text
target\release\LibSSH.exe
```

## 配置与数据

LibSSH 使用 `directories::ProjectDirs` 计算用户配置目录，并将主要配置保存为 `sessions.json`。配置内容包括：

- SSH 会话列表
- 默认下载目录
- 当前语言
- AI Skill CLI 策略
- 自动更新设置
- 快捷命令

密码优先存入系统凭据库：

- macOS Keychain
- Windows Credential Manager
- Linux Secret Service

如果系统凭据库写入失败，会回退到配置文件保存。配置文件损坏时，LibSSH 会将旧文件重命名为 `sessions.json.broken` 并使用默认配置启动。

## AI Skill CLI

`LibSSH skill ...` 是专为 AI 编程工具设计的安全 CLI。它允许 AI Agent 在用户明确授权后，对 LibSSH 中已经保存的远程主机执行有限命令。这个入口的核心目标是：让 Agent 能完成检查负载、看服务状态、查日志等低风险运维任务，同时避免绕过 LibSSH 的凭据管理和安全策略。

### 设计原则

- CLI 和 GUI 是同一个二进制：`LibSSH skill ...` 进入 CLI；不带 `skill` 启动 GUI。
- 默认关闭：未启用前，所有远程命令都会被拒绝。
- 先检查再执行：Agent 应先调用 `check`，确认策略允许，再调用 `run`。
- 最小授权：allow list 使用命令前缀，不需要也不应该授权过宽命令。
- deny 优先：内置安全策略和用户 deny list 会拦截危险命令。
- 不暴露凭据：会话列表只输出是否存在密码/私钥，不输出密码、代理密码或私钥路径。
- 输出脱敏：远端 stdout / stderr 返回前会按已知凭据和常见敏感字段做脱敏。
- 不读外部 SSH 凭据：AI Agent 不应该读取 `~/.ssh/*`，也不应该建立原始 `ssh` 连接。

### 启用全局 CLI 命令

AI 工具通常以非交互子进程执行命令，读不到 shell alias，因此推荐让 `LibSSH` 出现在 `PATH` 中。

macOS / Linux 可以在 GUI 的设置菜单中点击「启用全局 CLI」。该操作会创建或刷新：

```text
~/.local/bin/LibSSH -> 当前运行的 LibSSH 二进制
```

如果 `~/.local/bin` 不在 `PATH` 中，把下面一行加入 shell 配置，例如 `~/.zshrc`：

```bash
export PATH="$HOME/.local/bin:$PATH"
```

也可以直接用 release 二进制路径调用：

```bash
/Applications/LibSSH.app/Contents/MacOS/LibSSH skill sessions
./target/release/LibSSH skill sessions
```

Windows 当前没有 GUI 内的全局符号链接开关，可使用完整 exe 路径：

```powershell
.\target\release\LibSSH.exe skill sessions
```

### 命令总览

```bash
LibSSH skill export
LibSSH skill sessions
LibSSH skill policy show
LibSSH skill policy enable
LibSSH skill policy disable
LibSSH skill policy allow <command-prefix>
LibSSH skill policy deny <command-prefix>
LibSSH skill policy remove-allow <command-prefix>
LibSSH skill policy remove-deny <command-prefix>
LibSSH skill check --command <command>
LibSSH skill run --session <id-or-name> --command <command>
```

### 推荐工作流

1. 导出给 Agent 使用的技能说明：

```bash
LibSSH skill export > .claude/skills/libssh/SKILL.md
```

2. 查看已保存会话：

```bash
LibSSH skill sessions
```

输出为 JSON，只包含连接元数据和凭据存在标记：

```json
[
  {
    "id": "6c9b...",
    "name": "prod",
    "host": "203.0.113.10",
    "port": 22,
    "user": "ubuntu",
    "auth": "key",
    "has_password": false,
    "has_private_key": true
  }
]
```

3. 开启 AI CLI：

```bash
LibSSH skill policy enable
```

4. 只授权当前任务需要的命令前缀：

```bash
LibSSH skill policy allow "uptime"
LibSSH skill policy allow "df -h"
LibSSH skill policy allow "systemctl status nginx"
LibSSH skill policy allow "journalctl -u nginx"
```

前缀匹配规则为「完全相等」或「规则后面接空白」。例如：

- `allow "uptime"` 允许 `uptime` 和 `uptime -p`
- `allow "systemctl status"` 允许 `systemctl status nginx`
- `allow "cat"` 不会允许 `catfoo`

5. 执行前检查：

```bash
LibSSH skill check --command "uptime"
```

允许时返回：

```json
{
  "allowed": true,
  "command": "uptime",
  "reason": "allowed"
}
```

被拒绝时返回原因：

```json
{
  "allowed": false,
  "command": "rm -rf /tmp/demo",
  "reason": "command is blocked by the built-in safety policy"
}
```

6. 在指定会话执行：

```bash
LibSSH skill run --session "prod" --command "uptime"
```

输出为 JSON：

```json
{
  "exit_status": 0,
  "stdout": "10:12:30 up 12 days,  3 users,  load average: 0.08, 0.04, 0.01\n",
  "stderr": ""
}
```

`--session` 可传会话 `id` 或会话 `name`。

### 策略规则

每条命令必须同时通过以下检查：

1. AI Skill CLI 已启用。
2. 命令非空。
3. 不命中内置危险命令。
4. 命令中不包含疑似敏感赋值，例如 `password=...`、`token: ...`。
5. 不命中用户 deny list。
6. 命中用户 allow list。

内置危险命令包括但不限于：

```text
rm, dd, mkfs, shutdown, reboot, halt, poweroff,
passwd, userdel, groupdel, chown, chmod,
sudo, su, env, printenv, set, history,
kubectl get secret, kubectl describe secret,
aws secretsmanager, gcloud secrets, op item, pass,
security find-generic-password
```

用户 deny list 用于临时覆盖已授权前缀：

```bash
LibSSH skill policy deny "systemctl restart"
LibSSH skill policy remove-deny "systemctl restart"
```

关闭入口：

```bash
LibSSH skill policy disable
```

查看当前策略：

```bash
LibSSH skill policy show
```

### Agent 调用约束

项目根目录的 `AGENTS.md` 要求 AI Agent 操作远程服务器时必须走 LibSSH Skill CLI：

```text
LibSSH skill sessions
LibSSH skill check --command "<cmd>"
LibSSH skill run --session "<id-or-name>" --command "<cmd>"
```

Agent 不应该：

- 使用原始 `ssh` 命令连接远端。
- 读取 `~/.ssh/*` 或 LibSSH 配置文件来寻找凭据。
- 向用户索要密码、私钥、代理凭据或 API token。
- 在命令被拒绝时改写为等价危险命令来绕过策略。
- 请求过宽授权，例如 `allow "bash"`、`allow "sh"`、`allow "cat"`。

当命令被阻止时，Agent 应报告阻止原因，并让用户自行决定是否调整策略。

### 适合授权给 AI 的命令示例

```bash
LibSSH skill policy allow "uptime"
LibSSH skill policy allow "hostname"
LibSSH skill policy allow "whoami"
LibSSH skill policy allow "df -h"
LibSSH skill policy allow "free -m"
LibSSH skill policy allow "systemctl status nginx"
LibSSH skill policy allow "journalctl -u nginx --no-pager"
LibSSH skill policy allow "tail -n 100 /var/log/nginx/error.log"
```

更高风险的变更类操作建议由用户在 GUI 终端中手动执行，或临时、精确地授权到具体命令前缀。

## 自动更新

LibSSH 启动时默认每 24 小时从 GitHub Releases 检查一次新版本，也可以在「关于」对话框中手动检查。

发现新版本后可选择：

- 跳过此版本
- 稍后
- 立即更新

macOS 更新流程：

1. 选择当前架构对应的 `LibSSH-macos-<arch>.dmg`。
2. 下载 `checksums.txt`。
3. 校验 SHA256。
4. 下载 dmg。
5. 尝试自动挂载并覆盖 `/Applications/LibSSH.app`。
6. 如果当前安装位置不可写，则打开 dmg 并引导用户手动拖拽安装。

Windows / Linux 暂未实现自动安装，会保留手动安装路径。

关闭自动检查：在配置文件中将 `auto_check_update` 设置为 `false`。

## 开发

常用命令：

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo run
```

CI 在 GitHub Actions 中执行：

- `cargo fmt --all --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked`

### 目录结构

```text
src/
  app.rs          GUI 状态、事件接线、业务流程
  cli.rs          AI Skill CLI
  config.rs       会话、配置、AI CLI 策略
  ssh.rs          SSH 终端与 exec
  sftp.rs         SFTP worker 与文件操作
  proxy.rs        SOCKS5 / HTTP CONNECT 代理
  secrets.rs      系统凭据库读写
  system.rs       本机资源采样、主题检测、全局 CLI 链接
  updater.rs      GitHub Releases 自动更新
  i18n.rs         中英文翻译
  logbuf.rs       运行日志缓冲
  ssh_config.rs   ~/.ssh/config 导入解析

ui/
  app.slint
  terminal_view.slint
  sftp_panel.slint
  command_bar.slint
  session_dialog.slint
  sidebar.slint
  tabs.slint
  theme.slint
  welcome.slint

assets/           图标、Linux 安装脚本
scripts/          macOS 打包脚本
lang/             gettext 翻译文件
docs/             设计与实现计划文档
```

## 发布

Release workflow 支持三种触发方式：

- 推送到 `main`：如果 `Cargo.toml` 版本号对应的 `v<version>` tag 不存在，会自动创建 tag 并发布。
- 推送 `v*` tag：直接发布该 tag。
- 手动运行 workflow：按当前 `Cargo.toml` 版本重新构建并更新 release assets。

构建产物：

- `LibSSH-windows-x86_64.exe`
- `LibSSH-ubuntu-x86_64.tar.gz`
- `LibSSH-ubuntu-x86_64.deb`
- `LibSSH-macos-arm64.dmg`
- `LibSSH-macos-x86_64.dmg`
- `checksums.txt`

手动发版示例：

```bash
git tag v0.2.8 && git push origin v0.2.8
```

或仅更新 `Cargo.toml` 的版本号后推送到 `main`，由 workflow 自动创建 tag。

## 安全说明

- AI CLI 默认关闭，必须由用户显式启用。
- 远程命令必须命中 allow list，且不能命中 deny list 或内置安全策略。
- CLI 输出会脱敏常见敏感字段和当前会话中的已知秘密值。
- SFTP 内置编辑器只处理 UTF-8 文本，并限制默认最大 5 MB，避免误写二进制文件。
- 私钥认证通过本地文件路径加载，AI CLI 的会话列表不会输出私钥路径。
- 密码内存使用 `zeroize` 在释放时清零。

## 排障

### `LibSSH: command not found`

先启用 GUI 设置中的「全局 CLI」，或手动确保 `~/.local/bin` 在 `PATH` 中：

```bash
export PATH="$HOME/.local/bin:$PATH"
```

### AI CLI 返回 `AI skill CLI is disabled`

先启用：

```bash
LibSSH skill policy enable
```

### AI CLI 返回 `no allowed commands are configured`

至少添加一个精确命令前缀：

```bash
LibSSH skill policy allow "uptime"
```

### AI CLI 返回 `command is not in the configured allow list`

先检查命令，再按最小范围授权：

```bash
LibSSH skill check --command "systemctl status nginx"
LibSSH skill policy allow "systemctl status nginx"
```

### 自动更新下载失败

确认网络可访问 GitHub Releases，并检查运行日志浮层中的 WARN / ERROR。macOS 未签名构建可能需要手动打开 dmg 或清除 quarantine。
