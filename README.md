# LibSSH

Faithful Rust + Slint replica scaffold for the LibSSH project.

The implementation follows the Obsidian blueprint in `Project_Doc/LibSSH` and is built milestone-by-milestone. Phase 1 preserves the documented current behavior; hardening and improvements are deferred until the replica passes the acceptance checklist.

## Build

```bash
cargo run
cargo build --release
```

The release binary is written to `target/release/LibSSH`.

## AI Skill CLI

LibSSH includes a guarded CLI surface for Codex, Claude Code, and similar AI tools. It is disabled by default and never prints saved passwords, proxy credentials, or private-key paths in session listings.

```bash
LibSSH skill export
LibSSH skill sessions
LibSSH skill policy enable
LibSSH skill policy allow "uptime"
LibSSH skill policy deny "systemctl reboot"
LibSSH skill check --command "uptime"
LibSSH skill run --session "<id-or-name>" --command "uptime"
```

`skill export` prints a SKILL.md-style instruction block for AI tools. Remote commands must pass the enabled flag, the configured allow list, the configured deny list, and the built-in safety policy. CLI output is redacted before it is printed back to the caller.

## Platform Packaging

Icon assets are generated from `assets/icon.png`:

```bash
python3 assets/make_icon.py
```

Linux desktop integration installs the release binary, a 512px icon, and a user-local launcher:

```bash
cargo build --release
assets/install-linux.sh target/release/LibSSH
```

macOS packaging creates an unsigned `.app` bundle and `.dmg`:

```bash
cargo build --release
scripts/package-macos-dmg.sh target/release/LibSSH dist
```

For an unsigned macOS build downloaded from a release, clear quarantine before opening:

```bash
xattr -dr com.apple.quarantine LibSSH.app
```

GitHub releases are built by `.github/workflows/release.yml` when pushing a `v*` tag.
The release workflow builds Windows, Ubuntu, macOS Apple Silicon, and macOS Intel artifacts:

- `LibSSH-windows-x86_64.zip`
- `LibSSH-ubuntu-x86_64.tar.gz`
- `LibSSH-ubuntu-x86_64.deb`
- `LibSSH-macos-arm64.dmg`
- `LibSSH-macos-x86_64.dmg`

## 自动更新

LibSSH 启动时会（每 24 小时一次）从 GitHub Releases 检查新版本；也可在「关于」对话框点「检查更新」手动触发。检测到新版本会弹出更新说明与 `跳过此版本 / 稍后 / 立即更新`。

macOS 上点「立即更新」会下载对应架构的 dmg、用 `checksums.txt` 校验 SHA256，然后自动挂载、覆盖 `/Applications/LibSSH.app`、清除隔离标记，提示重启完成更新。当 app 不在可写位置（如开发环境）时降级为打开 dmg 引导手动安装。Windows / Linux 的自动安装尚未实现。

关闭自动检查：编辑配置文件把 `auto_check_update` 设为 `false`。

## 自动打包

git tag v0.2.6 && git push origin v0.2.6
