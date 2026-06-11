# 设计：消除 AI Skill CLI 的重复授权（钥匙串弹窗 + policy 摩擦）

日期：2026-06-11
状态：已批准（方案一：稳定签名 + 只读预设，宽松集）

## 背景与根因

AI 工具（Codex / Claude Code）通过 `LibSSH skill run` 操作远程主机时，每次都被两道闸门打断：

1. **macOS 钥匙串授权弹窗**。会话密码已迁入系统钥匙串（`src/secrets.rs`），CLI `run` 时回查 keyring。本机三处二进制（`~/.local/bin/LibSSH`、`/Applications/LibSSH.app`、`target/release/LibSSH`）均为 adhoc 签名（linker-signed、无 TeamIdentifier）。adhoc 签名的钥匙串信任基于二进制内容哈希——每次 `cargo build` 后哈希改变，钥匙串视其为陌生应用并重新弹窗索要登录密码；"始终允许"也只对当前哈希有效。活跃开发 = 永远在重新授权。
2. **policy allow 逐条授权**。策略持久化没问题（`enable` 一次即可），但 allow 清单按命令前缀逐条加（`src/config.rs` `evaluate_command`）。诊断类任务命令面广，AI 每遇到新前缀就被 not-allowed 挡住，且 SKILL.md 要求 AI 停下让用户手动 `policy allow`。

## 目标与非目标

**目标**：一次性设置后，AI 调用 `sessions` / `check` / `run` 执行常规只读诊断命令时零弹窗、零手动授权。

**非目标**：

- 不改变安全模型：密码仍只存系统钥匙串；预设外命令照旧被挡；内置 deny 不放松。
- 不处理分发给其他用户的签名问题（需要 Apple Developer ID，另立项目）。
- 不动 Windows / Linux（Secret Service / 凭据管理器按用户会话授权，无此痛点）。

## 组件 A：macOS 稳定代码签名

### A1. `scripts/setup-macos-codesign.sh`（一次性）

- 检查 login 钥匙串中是否已有名为 `LibSSH Dev Signing` 的代码签名证书；存在则跳过（幂等）。
- 不存在则创建自签名证书并标记信任：`openssl` 生成带 codeSigning EKU 的自签证书 → `security import` 导入 → `security add-trusted-cert -d -r trustRoot -p codeSign`（需输一次管理员密码）。
- 任一步失败即中止并清理临时文件，不留半成品。

### A2. `scripts/codesign-macos.sh <binary-or-app>`

- `codesign --force --sign "LibSSH Dev Signing" --identifier dev.libssh.LibSSH <target>`（identifier 与打包脚本的 `CFBundleIdentifier` 保持一致）。
- 固定 `--identifier` 是关键：钥匙串信任判定（designated requirement）从"内容哈希"变为"证书 + identifier"，跨构建恒定。
- 证书不存在时提示先跑 A1；非 macOS 平台提示后退出。

### A3. 接线

- 实际调用链路：`~/.local/bin/LibSSH` 是 GUI「启用全局 CLI」创建的**符号链接**，指向 `/Applications/LibSSH.app/Contents/MacOS/LibSSH`——签名落点是 `.app`。
- `scripts/package-macos-dmg.sh`：装配 `.app` 后、打 dmg 前签名（证书存在才签，CI 无证书时保持 adhoc 不破坏流水线）。
- 新增 `scripts/install-macos-app.sh`：`cargo build --release` → 打包（内含签名）→ 部署到 `/Applications/LibSSH.app`（symlink 指向不变，AI 调用即刻生效）。
- 已知边界：自动更新安装的官方构建无本地签名，更新后需重跑 `install-macos-app.sh` 恢复。
- README + AGENTS.md 增加说明。

### 旧钥匙串条目

旧条目由 adhoc 签名的旧二进制创建，ACL 记录的是旧 requirement。签名稳定后**首次**访问每个旧条目仍会各弹一次——点"始终允许"后该条目永久安静。不做自动迁移：会话数量有限，一次性点掉比迁移代码简单可靠。此后新建/更新的条目由稳定签名的应用创建，GUI 与 CLI 同签名，天然零弹窗。

## 组件 B：policy 只读预设（readonly，宽松集）

### 新 CLI 子命令

- `LibSSH skill policy presets` —— 列出可用预设及完整命令清单（JSON），供人与 AI 审视。
- `LibSSH skill policy allow-preset readonly` —— 清单逐条并入 `allowed_commands`（复用 `add_unique`，幂等）；未知预设名报错并列出可用名字。

### readonly 清单

按现有前缀匹配语义（`command_matches_rule`：完整词边界前缀），多词条目只放行该子命令：

| 类别 | 命令 |
| --- | --- |
| 系统状态 | `uptime` `w` `who` `last` `date` `hostname` `uname` `whoami` `id` `nproc` `lscpu` `lsblk` `findmnt` |
| 文件只读 | `ls` `cat` `head` `tail` `stat` `file` `wc` `du` `df` `find` `grep` |
| 进程/资源 | `ps` `top` `free` `vmstat` `iostat` `lsof` |
| 服务/日志 | `systemctl status` `journalctl` `dmesg` |
| 网络诊断 | `netstat` `ss` `ip addr show` `ip route show` `ping` `traceroute` `dig` `nslookup` `host` `curl` `wget` |
| 容器/编排 | `docker ps` `docker logs` `docker images` `docker stats` `kubectl get` `kubectl describe` `kubectl logs` |
| 计划任务 | `crontab -l` |

### 安全边界（保持不变）

- 内置 deny 优先于 allow：`kubectl get` 入预设后 `kubectl get secret` 仍被 `BUILTIN_DENIED_COMMANDS` 拦截；`rm` / `sudo` / `env` 等照旧。
- 命令内联敏感赋值检查（`contains_sensitive_assignment`）与输出打码（`redact_for_llm`）照常生效。
- 已知权衡：`cat` / `grep` / `curl` 可读取远端配置或外发数据；`find` 前缀同时放行 `-delete` / `-exec`（写操作）、`wget` 会向远端磁盘写下载文件——宽松集固有风险，用户已知情选择。缓解：SKILL.md 明确禁止 AI 用 `find -delete` 等绕过被挡命令（行为约束），输出端 `redact_sensitive_line` 兜底打码敏感行。
- 刻意不收：`mount`（裸前缀会放行挂载写操作）、`ip` 裸前缀（`ip link set` 危险，只收 `ip addr show` / `ip route show`）、`docker` / `kubectl` 裸前缀（`docker rm` / `kubectl delete` 危险，逐条收只读子命令）。

## 组件 C：SKILL 文档同步

`src/cli.rs` `generated_skill_markdown()` 的"When a command is blocked"一节增加：被挡时可建议用户运行 `LibSSH skill policy allow-preset readonly` 一次性导入只读集，替代逐条 allow。重新导出覆盖两处副本：`SKILL/SKILL.md` 与 `.claude/skills/libssh/SKILL.md`（AGENTS.md 指定的刷新点）。

## 最终体验（数据流）

一次性设置（约 2 分钟）：

```bash
scripts/setup-macos-codesign.sh        # 建证书（输一次管理员密码）
scripts/install-macos-app.sh           # 构建 + 打包签名 + 部署 /Applications
LibSSH skill policy enable
LibSSH skill policy allow-preset readonly
# 旧钥匙串条目首次访问各点一次「始终允许」
```

之后：AI 跑 `sessions` / `check` / `run` 全程无人值守；预设外命令（如 `systemctl restart`）照旧被挡，由用户决定是否单独 allow；每次重新构建后跑 `install-macos-app.sh`（或对 `target/release/LibSSH` 单独 `codesign-macos.sh`），签名恒定，钥匙串不再弹窗。

## 错误处理

- setup 脚本：`security` 任一步失败即中止并清理；重复执行幂等跳过。
- 签名脚本：证书缺失 → 提示先跑 setup；非 macOS → 提示退出。
- `allow-preset`：未知预设名 → 报错 + 列出可用预设；重复导入幂等。

## 测试

- Rust 单测：
  - 预设导入后，清单内每条命令 `evaluate_command` 通过（enabled + preset）。
  - 导入幂等：连跑两次 `allowed_commands` 无重复。
  - 危险命令导入预设后仍被拒：`rm -rf /`、`sudo ls`、`docker rm x`、`kubectl get secret`、`mount /dev/sda1 /mnt`、`crontab -r`。
  - `presets` 子命令输出含 readonly 及其清单。
- 脚本：`bash -n` 语法检查纳入 CI。
- 手动验收清单：构建 → 签名 → GUI 存一个密码 → CLI `run` 不弹窗 → `touch src/main.rs && cargo build --release` → 重签 → CLI `run` 仍不弹窗。

## 不做的事（YAGNI）

钥匙串旧条目自动迁移、GUI policy 设置页、unlock agent（ssh-agent 式内存缓存）、CI / 分发签名、Windows / Linux 改动。
