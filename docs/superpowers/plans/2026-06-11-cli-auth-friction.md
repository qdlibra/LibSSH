# AI Skill CLI 免重复授权 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 一次性设置后，Codex / Claude 调用 `LibSSH skill run` 执行常规只读诊断命令时零钥匙串弹窗、零手动 `policy allow`。

**Architecture:** 两条腿：① macOS 本地自签名证书 + 固定 identifier 签名，使钥匙串信任跨构建恒定（AI 调用链路为 `~/.local/bin/LibSSH` symlink → `/Applications/LibSSH.app`，签名落点是 .app）；② `policy allow-preset readonly` 一键导入只读诊断命令集，预设常量与策略评估同住 `config.rs`，CLI 子命令在 `cli.rs`。安全模型不变：内置 deny 优先、输出打码照旧。

**Tech Stack:** Rust（无新依赖）、bash 脚本（openssl + security + codesign）、GitHub Actions。

**Spec:** `docs/superpowers/specs/2026-06-11-cli-auth-friction-design.md`

---

## 前置检查（开工前必做）

工作区当前有**未提交的用户修改**：`.github/workflows/ci.yml`、`.github/workflows/release.yml`、`README.md`。Task 7 / Task 8 会改其中两个文件。开工前：

1. `git status --porcelain` 确认现状；`git diff .github/workflows/ci.yml README.md` 看内容。
2. 若这些修改与本计划无关 → 请用户先 commit 或 stash，再开工（避免本计划的 commit 裹挟无关改动）。
3. 若用户不在场且修改无冲突 → 照常叠加编辑，但 commit 信息中注明"包含工作区既有未提交修改"，并在收尾时向用户报告。

## 背景速览（给零上下文的实施者）

- CLI 与 GUI 是同一个二进制：`LibSSH skill ...` 走 CLI（`src/cli.rs`），不带 `skill` 启动 GUI。
- 策略评估在 `src/config.rs` 的 `AiSkillConfig::evaluate_command`（行 ~189）：顺序为 enabled → 内置 deny（`BUILTIN_DENIED_COMMANDS`，行 9）→ 敏感赋值 → 用户 deny → 用户 allow。前缀匹配按词边界（`command_matches_rule`，行 223）。
- 会话密码在系统钥匙串（`src/secrets.rs`），CLI `run` 时回查。当前所有二进制为 adhoc 签名 → 每次重新构建钥匙串都重新弹窗，这是要消除的痛点。
- 测试跑法：`cargo test --locked`；格式 `cargo fmt --all --check`；lint `cargo clippy --all-targets --locked -- -D warnings`。

---

### Task 0: 提交 spec 的事实修正（若工作区尚未提交）

**Files:**
- Modify: `docs/superpowers/specs/2026-06-11-cli-auth-friction-design.md`（已在工作区改好：identifier 改为 `dev.libssh.LibSSH`、接线改为 `install-macos-app.sh`、SKILL 导出两处副本）

- [ ] **Step 0.1: 确认并提交**

```bash
git diff docs/superpowers/specs/2026-06-11-cli-auth-friction-design.md
git add docs/superpowers/specs/2026-06-11-cli-auth-friction-design.md
git commit -m "docs(spec): 签名 identifier 对齐 bundle id、接线改为 install-macos-app、SKILL 双副本导出"
```

---

### Task 1: `config.rs` —— readonly 预设常量与查找函数（TDD）

**Files:**
- Modify: `src/config.rs`（`BUILTIN_DENIED_COMMANDS` 之后加常量与函数；`tests` 模块加测试）
- Test: `src/config.rs` 内联 `#[cfg(test)] mod tests`

- [ ] **Step 1.1: 写失败测试**

在 `src/config.rs` 的 `mod tests` 内（紧跟现有 `ai_skill_policy_*` 测试之后）添加：

```rust
    #[test]
    fn readonly_preset_commands_all_pass_policy_evaluation() {
        let policy = AiSkillConfig {
            enabled: true,
            allowed_commands: READONLY_PRESET.iter().map(|s| s.to_string()).collect(),
            denied_commands: Vec::new(),
        };
        for cmd in READONLY_PRESET {
            assert!(
                policy.evaluate_command(cmd).is_ok(),
                "preset command should pass: {cmd}"
            );
        }
        // 带参数的典型形态也要落在词边界前缀内
        assert!(policy.evaluate_command("df -h").is_ok());
        assert!(policy.evaluate_command("journalctl -u nginx --since today").is_ok());
        assert!(policy.evaluate_command("docker logs --tail 100 web").is_ok());
        assert!(policy.evaluate_command("systemctl status sshd").is_ok());
    }

    #[test]
    fn readonly_preset_does_not_unlock_dangerous_commands() {
        let policy = AiSkillConfig {
            enabled: true,
            allowed_commands: READONLY_PRESET.iter().map(|s| s.to_string()).collect(),
            denied_commands: Vec::new(),
        };
        for cmd in [
            "rm -rf /",              // 内置 deny
            "sudo ls",               // 内置 deny
            "env",                   // 内置 deny
            "kubectl get secret",    // 内置 deny 优先于预设的 kubectl get
            "docker rm web",         // 预设只收 docker 只读子命令
            "docker exec -it web sh",
            "kubectl delete pod web",
            "mount /dev/sda1 /mnt",  // 刻意不收 mount
            "crontab -r",            // 预设只收 crontab -l
            "ip link set eth0 down", // 预设只收 ip addr show / ip route show
            "systemctl restart nginx", // 预设只收 systemctl status
        ] {
            assert!(policy.evaluate_command(cmd).is_err(), "must stay blocked: {cmd}");
        }
    }

    #[test]
    fn preset_lookup_finds_readonly_and_rejects_unknown() {
        assert_eq!(preset_commands("readonly"), Some(READONLY_PRESET));
        assert_eq!(preset_commands("yolo"), None);
    }
```

- [ ] **Step 1.2: 跑测试确认失败**

```bash
cargo test --locked readonly_preset 2>&1 | tail -5
```

Expected: 编译错误 `cannot find value READONLY_PRESET` / `cannot find function preset_commands`。

- [ ] **Step 1.3: 最小实现**

在 `src/config.rs` 的 `BUILTIN_DENIED_COMMANDS` 数组（行 9-35）之后添加：

```rust
/// 只读诊断命令预设：`LibSSH skill policy allow-preset readonly` 一键导入。
/// 多词条目只放行该子命令（前缀匹配按词边界，见 `command_matches_rule`）。
/// 刻意不收裸 `ip` / `docker` / `kubectl` / `mount`——它们的子命令含写操作；
/// `find` / `wget` / `curl` 的写能力是宽松集已知权衡（spec「安全边界」节）。
pub const READONLY_PRESET: &[&str] = &[
    // 系统状态
    "uptime", "w", "who", "last", "date", "hostname", "uname", "whoami", "id",
    "nproc", "lscpu", "lsblk", "findmnt",
    // 文件只读
    "ls", "cat", "head", "tail", "stat", "file", "wc", "du", "df", "find", "grep",
    // 进程/资源
    "ps", "top", "free", "vmstat", "iostat", "lsof",
    // 服务/日志
    "systemctl status", "journalctl", "dmesg",
    // 网络诊断
    "netstat", "ss", "ip addr show", "ip route show", "ping", "traceroute",
    "dig", "nslookup", "host", "curl", "wget",
    // 容器/编排（内置 deny 已拦 kubectl get/describe secret）
    "docker ps", "docker logs", "docker images", "docker stats",
    "kubectl get", "kubectl describe", "kubectl logs",
    // 计划任务
    "crontab -l",
];

/// 按名字取预设清单；未知名字返回 None（调用方负责报错口径）。
pub fn preset_commands(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "readonly" => Some(READONLY_PRESET),
        _ => None,
    }
}
```

- [ ] **Step 1.4: 跑测试确认通过**

```bash
cargo test --locked readonly_preset preset_lookup 2>&1 | tail -5
```

Expected: 3 个测试 PASS。

- [ ] **Step 1.5: 格式化并 Commit**

```bash
cargo fmt --all   # READONLY_PRESET 数组会被 rustfmt 重排，提交前先格式化
git add src/config.rs
git commit -m "feat(policy): readonly 只读诊断命令预设常量与查找函数"
```

---

### Task 2: `cli.rs` —— `policy presets` / `policy allow-preset` 子命令（TDD）

**Files:**
- Modify: `src/cli.rs`（`CliAction` 枚举、`run_args`、`parse_policy_args`、`required_positional`、`help_text`、`tests`）

- [ ] **Step 2.1: 写失败测试**

在 `src/cli.rs` 的 `mod tests` 内添加：

```rust
    #[test]
    fn parses_policy_presets_listing() {
        let args = vec![
            "LibSSH".to_string(),
            "skill".to_string(),
            "policy".to_string(),
            "presets".to_string(),
        ];
        assert_eq!(parse_args(&args).unwrap(), CliAction::PolicyPresets);
    }

    #[test]
    fn parses_policy_allow_preset_name() {
        let args = vec![
            "LibSSH".to_string(),
            "skill".to_string(),
            "policy".to_string(),
            "allow-preset".to_string(),
            "readonly".to_string(),
        ];
        assert_eq!(
            parse_args(&args).unwrap(),
            CliAction::PolicyAllowPreset("readonly".to_string())
        );
    }

    #[test]
    fn applying_preset_twice_adds_no_duplicates() {
        let mut allowed: Vec<String> = Vec::new();
        for _ in 0..2 {
            for command in crate::config::READONLY_PRESET {
                add_unique(&mut allowed, (*command).to_string());
            }
        }
        assert_eq!(allowed.len(), crate::config::READONLY_PRESET.len());
    }

    #[test]
    fn preset_listing_exposes_readonly_contents() {
        let listing = preset_listing();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].name, "readonly");
        assert!(listing[0].commands.contains(&"uptime"));
        assert!(listing[0].commands.contains(&"systemctl status"));
    }
```

- [ ] **Step 2.2: 跑测试确认失败**

```bash
cargo test --locked parses_policy_presets parses_policy_allow_preset applying_preset preset_listing 2>&1 | tail -5
```

Expected: 编译错误（`PolicyPresets` / `PolicyAllowPreset` / `preset_listing` 未定义）。

- [ ] **Step 2.3: 实现**

`src/cli.rs` 五处改动：

① `CliAction` 枚举（行 6-18）加两个变体：

```rust
    PolicyRemoveAllow(String),
    PolicyRemoveDeny(String),
    PolicyPresets,
    PolicyAllowPreset(String),
```

② `run_args` 的 match（`CliAction::PolicyRemoveDeny` 分支之后、`CliAction::Check` 之前）加两个分支：

```rust
        CliAction::PolicyPresets => {
            print_json(&preset_listing())?;
        }
        CliAction::PolicyAllowPreset(name) => {
            let commands = crate::config::preset_commands(&name)
                .with_context(|| format!("unknown preset: {name} (available: readonly)"))?;
            let mut store = ConfigStore::load()?;
            let before = store.ai_skill().allowed_commands.len();
            for command in commands {
                add_unique(&mut store.ai_skill_mut().allowed_commands, (*command).to_string());
            }
            store.save()?;
            let added = store.ai_skill().allowed_commands.len() - before;
            let message = format!("preset {name} applied ({added} commands added)");
            print_json(&StatusMessage::ok(&message))?;
        }
```

③ `parse_policy_args`（行 141-153）加两个分支（放在 `Some("allow")` 之前，避免与 `allow` 前缀混淆的阅读歧义）：

```rust
        Some("presets") => Ok(CliAction::PolicyPresets),
        Some("allow-preset") => Ok(CliAction::PolicyAllowPreset(required_positional(
            args,
            4,
            "preset name",
        )?)),
```

④ `required_positional`（行 163-168）加 `what` 参数，错误信息按调用方语义：

```rust
fn required_positional(args: &[String], index: usize, what: &str) -> Result<String> {
    args.get(index)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .with_context(|| format!("missing {what}"))
}
```

同时更新既有 4 个调用点（`allow` / `deny` / `remove-allow` / `remove-deny`）为 `required_positional(args, 4, "command prefix")?`。

⑤ `preset_listing` 与序列化结构（放在 `CommandDecision` 定义附近）+ `help_text` 加两行：

```rust
#[derive(Serialize)]
struct PresetInfo {
    name: &'static str,
    commands: &'static [&'static str],
}

fn preset_listing() -> Vec<PresetInfo> {
    vec![PresetInfo {
        name: "readonly",
        commands: crate::config::READONLY_PRESET,
    }]
}
```

`help_text()`（行 287-298）在 `policy remove-deny` 行后加：

```text
  LibSSH skill policy presets
  LibSSH skill policy allow-preset <preset-name>
```

- [ ] **Step 2.4: 跑测试确认通过**

```bash
cargo test --locked 2>&1 | tail -5
```

Expected: 全部 PASS（含既有测试——`required_positional` 签名变化已同步所有调用点）。

- [ ] **Step 2.5: 手动冒烟（不碰真实配置的只读子命令）**

```bash
cargo run -- skill policy presets | head -8
cargo run -- skill policy allow-preset nope 2>&1 | tail -2
```

Expected: 前者输出 readonly 预设 JSON；后者报 `unknown preset: nope (available: readonly)`。

- [ ] **Step 2.6: 格式化并 Commit**

```bash
cargo fmt --all
git add src/cli.rs
git commit -m "feat(cli): skill policy presets / allow-preset 子命令——只读预设一键导入"
```

---

### Task 3: SKILL 文案更新与双副本导出（TDD）

**Files:**
- Modify: `src/cli.rs`（`generated_skill_markdown`）
- Modify: `SKILL/SKILL.md`、`.claude/skills/libssh/SKILL.md`（由 export 重新生成）

- [ ] **Step 3.1: 写失败测试**

`src/cli.rs` `mod tests` 加：

```rust
    #[test]
    fn skill_markdown_mentions_readonly_preset() {
        let md = generated_skill_markdown();
        assert!(md.contains("policy allow-preset readonly"));
        assert!(md.contains("policy presets"));
    }
```

- [ ] **Step 3.2: 跑测试确认失败**

```bash
cargo test --locked skill_markdown_mentions 2>&1 | tail -5
```

Expected: FAIL（assert 不成立）。

- [ ] **Step 3.3: 改文案**

`generated_skill_markdown()` 中两处：

① Quick reference 表（`| Run a command |` 行后）加一行：

```text
| Import the read-only diagnostics preset | `LibSSH skill policy allow-preset readonly` |
```

② "When a command is blocked" 一节，第一个 bullet 改为：

```text
- Tell the user what to run, and let them run it: `LibSSH skill policy enable`, then `LibSSH skill policy allow "<command-prefix>"`. For routine read-only diagnostics, suggest the one-shot preset instead of piecemeal rules: `LibSSH skill policy allow-preset readonly` (the user can inspect it first with `LibSSH skill policy presets`).
```

- [ ] **Step 3.4: 跑测试确认通过**

```bash
cargo test --locked skill_markdown 2>&1 | tail -5
```

Expected: PASS（含既有 `export_emits_valid_skill_frontmatter`）。

- [ ] **Step 3.5: 重新导出两处副本**

```bash
cargo run -- skill export > SKILL/SKILL.md
cargo run -- skill export > .claude/skills/libssh/SKILL.md
git diff --stat SKILL/SKILL.md .claude/skills/libssh/SKILL.md
```

Expected: 两个文件均有 diff 且内容一致。

- [ ] **Step 3.6: 格式化并 Commit**

```bash
cargo fmt --all
git add src/cli.rs SKILL/SKILL.md .claude/skills/libssh/SKILL.md
git commit -m "docs(skill): 被挡提示推荐 allow-preset readonly，重新导出双副本"
```

---

### Task 4: `scripts/setup-macos-codesign.sh` —— 一次性自签名证书

**Files:**
- Create: `scripts/setup-macos-codesign.sh`（mode 0755）

- [ ] **Step 4.1: 写脚本**

```bash
#!/usr/bin/env bash
# 一次性：创建并信任本地自签名代码签名证书「LibSSH Dev Signing」。
# 之后 scripts/codesign-macos.sh 用它签名，钥匙串信任跨构建恒定，
# CLI 读会话密码不再每次构建后重新弹授权框。幂等：已存在即跳过。
set -euo pipefail

CERT_NAME="LibSSH Dev Signing"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "Code signing setup is only needed on macOS; nothing to do." >&2
    exit 0
fi

if security find-certificate -c "$CERT_NAME" >/dev/null 2>&1; then
    echo "Certificate '$CERT_NAME' already exists; nothing to do."
    exit 0
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
P12_PASS="$(uuidgen)"

cat > "$TMP_DIR/cert.cnf" <<'EOF'
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = LibSSH Dev Signing
[ext]
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
basicConstraints = critical,CA:false
EOF

openssl req -x509 -newkey rsa:2048 -days 3650 -nodes \
    -keyout "$TMP_DIR/key.pem" -out "$TMP_DIR/cert.pem" \
    -config "$TMP_DIR/cert.cnf"

openssl pkcs12 -export -inkey "$TMP_DIR/key.pem" -in "$TMP_DIR/cert.pem" \
    -out "$TMP_DIR/cert.p12" -passout "pass:$P12_PASS"

security import "$TMP_DIR/cert.p12" \
    -k "$HOME/Library/Keychains/login.keychain-db" \
    -P "$P12_PASS" -T /usr/bin/codesign

echo "Marking the certificate as trusted for code signing (admin password required)..."
sudo security add-trusted-cert -d -r trustRoot -p codeSign \
    -k /Library/Keychains/System.keychain "$TMP_DIR/cert.pem"

echo "Certificate '$CERT_NAME' created and trusted."
echo "Note: the first codesign run may prompt once for keychain access — choose 'Always Allow'."
```

- [ ] **Step 4.2: 语法检查 + 赋权**

```bash
chmod +x scripts/setup-macos-codesign.sh
bash -n scripts/setup-macos-codesign.sh && echo SYNTAX-OK
```

Expected: `SYNTAX-OK`。

- [ ] **Step 4.3: 冒烟（真实创建证书，需用户在场输管理员密码）**

```bash
scripts/setup-macos-codesign.sh
security find-certificate -c "LibSSH Dev Signing" >/dev/null && echo CERT-OK
scripts/setup-macos-codesign.sh   # 第二次跑验证幂等
```

Expected: 第一次输出 created and trusted + `CERT-OK`；第二次输出 already exists。
（若实施者为无人值守 subagent：跳过本步，把它留给 Task 9 的人工验收清单。）

- [ ] **Step 4.4: Commit**

```bash
git add scripts/setup-macos-codesign.sh
git commit -m "build(macos): 一次性自签名代码签名证书脚本——钥匙串信任跨构建恒定的前提"
```

---

### Task 5: `scripts/codesign-macos.sh` —— 统一签名入口

**Files:**
- Create: `scripts/codesign-macos.sh`（mode 0755）

- [ ] **Step 5.1: 写脚本**

```bash
#!/usr/bin/env bash
# 用「LibSSH Dev Signing」证书 + 固定 identifier 签名二进制或 .app。
# identifier 必须与 package-macos-dmg.sh 的 BUNDLE_ID 一致：钥匙串信任
# 判定（designated requirement）= 证书 + identifier，跨构建恒定。
set -euo pipefail

CERT_NAME="LibSSH Dev Signing"
IDENTIFIER="dev.libssh.LibSSH"
TARGET="${1:?usage: codesign-macos.sh <binary-or-app>}"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "Code signing is only needed on macOS; skipping." >&2
    exit 0
fi

if ! security find-certificate -c "$CERT_NAME" >/dev/null 2>&1; then
    echo "Signing certificate '$CERT_NAME' not found." >&2
    echo "Run scripts/setup-macos-codesign.sh first." >&2
    exit 1
fi

codesign --force --sign "$CERT_NAME" --identifier "$IDENTIFIER" "$TARGET"
codesign --verify --strict "$TARGET"
echo "Signed: $TARGET ($IDENTIFIER)"
```

- [ ] **Step 5.2: 语法检查 + 赋权**

```bash
chmod +x scripts/codesign-macos.sh
bash -n scripts/codesign-macos.sh && echo SYNTAX-OK
```

Expected: `SYNTAX-OK`。

- [ ] **Step 5.3: 冒烟（需 Task 4 的证书已存在；二进制可先 `cargo build --release`）**

```bash
scripts/codesign-macos.sh target/release/LibSSH
codesign -dv target/release/LibSSH 2>&1 | grep -E "Identifier|Authority" | head -3
```

Expected: `Identifier=dev.libssh.LibSSH`，`Authority=LibSSH Dev Signing`（不再是 adhoc）。
（无证书环境跳过，留给 Task 9。）

- [ ] **Step 5.4: Commit**

```bash
git add scripts/codesign-macos.sh
git commit -m "build(macos): 统一签名脚本——固定证书与 identifier"
```

---

### Task 6: 打包接线 + `scripts/install-macos-app.sh`

**Files:**
- Modify: `scripts/package-macos-dmg.sh`（Info.plist 写完后、`rm -f "$DMG"` 前插入签名）
- Create: `scripts/install-macos-app.sh`（mode 0755）

- [ ] **Step 6.1: 打包脚本接线**

在 `scripts/package-macos-dmg.sh` 的 Info.plist heredoc 结束（行 66 `EOF`）与 `rm -f "$DMG"`（行 68）之间插入：

```bash
# 有本地签名证书就签 .app（钥匙串信任跨构建恒定）；没有（如 CI）保持 adhoc。
if [[ "$(uname -s)" == "Darwin" ]] \
    && security find-certificate -c "LibSSH Dev Signing" >/dev/null 2>&1; then
    "$ROOT/scripts/codesign-macos.sh" "$APP_DIR"
else
    echo "Signing certificate 'LibSSH Dev Signing' not found; packaging with adhoc signature." >&2
fi
```

- [ ] **Step 6.2: 写安装脚本**

`scripts/install-macos-app.sh`：

```bash
#!/usr/bin/env bash
# 构建 → 打包（内含签名）→ 部署到 /Applications。
# GUI「启用全局 CLI」创建的 ~/.local/bin/LibSSH 符号链接指向 .app 内
# 二进制，部署后 AI 工具调用立即用上新构建，且签名恒定不触发钥匙串弹窗。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="LibSSH"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "This installer is macOS-only." >&2
    exit 1
fi

cargo build --release --manifest-path "$ROOT/Cargo.toml"
"$ROOT/scripts/package-macos-dmg.sh" "$ROOT/target/release/$APP_NAME" "$ROOT/dist"

rm -rf "/Applications/$APP_NAME.app"
ditto "$ROOT/dist/$APP_NAME.app" "/Applications/$APP_NAME.app"
echo "Installed: /Applications/$APP_NAME.app"
```

- [ ] **Step 6.3: 语法检查 + 赋权**

```bash
chmod +x scripts/install-macos-app.sh
bash -n scripts/package-macos-dmg.sh scripts/install-macos-app.sh && echo SYNTAX-OK
```

Expected: `SYNTAX-OK`。

- [ ] **Step 6.4: 冒烟（有证书环境）**

```bash
scripts/install-macos-app.sh
codesign -dv /Applications/LibSSH.app 2>&1 | grep -E "Identifier|Authority" | head -3
ls -la ~/.local/bin/LibSSH
```

Expected: `Identifier=dev.libssh.LibSSH`、`Authority=LibSSH Dev Signing`；symlink 仍指向 `/Applications/LibSSH.app/Contents/MacOS/LibSSH`。
（无证书环境只验证脚本能产出 adhoc .app，留签名验证给 Task 9。）

- [ ] **Step 6.5: Commit**

```bash
git add scripts/package-macos-dmg.sh scripts/install-macos-app.sh
git commit -m "build(macos): 打包接线本地签名 + 一条龙安装脚本（构建/签名/部署 /Applications）"
```

---

### Task 7: CI 增加 shell 脚本语法检查

**Files:**
- Modify: `.github/workflows/ci.yml`（`check` job 的 `Test` step 之后）

> 注意：`ci.yml` 在工作区可能有未提交的本地修改。只追加下面这个 step，不动其他内容；提交时只 `git add -p` 本任务的改动。

- [ ] **Step 7.1: 加 step**

```yaml
      - name: Shell script syntax check
        run: bash -n scripts/*.sh
```

- [ ] **Step 7.2: 本地等价验证**

```bash
bash -n scripts/*.sh && echo ALL-SCRIPTS-OK
```

Expected: `ALL-SCRIPTS-OK`。

- [ ] **Step 7.3: Commit**

前置检查已确保 `ci.yml` 工作区干净（用户已 commit/stash 先前修改），直接整文件提交：

```bash
git add .github/workflows/ci.yml
git commit -m "ci: scripts/*.sh 语法检查"
```

---

### Task 8: README 与 AGENTS.md 文档

**Files:**
- Modify: `README.md`（"AI Skill CLI"章节内：命令总览加两条；新增"免重复授权（macOS）"小节）
- Modify: `AGENTS.md`（被挡提示加 preset 选项）

- [ ] **Step 8.1: README 命令总览补两行**

在 `LibSSH skill policy remove-deny <command-prefix>` 行后加：

```text
LibSSH skill policy presets
LibSSH skill policy allow-preset <preset-name>
```

- [ ] **Step 8.2: README 新增小节（放在"启用全局 CLI"小节之后）**

```markdown
### 免重复授权（macOS）

CLI 每次 `run` 都会从系统钥匙串读取会话密码。adhoc 签名的二进制每次重新构建后哈希都会变化，钥匙串会把它当成陌生应用反复弹授权框。用稳定的本地自签名证书签名后，「始终允许」即可长期生效：

```bash
scripts/setup-macos-codesign.sh        # 一次性：创建并信任本地签名证书（需管理员密码）
scripts/install-macos-app.sh           # 构建 + 打包签名 + 部署到 /Applications
LibSSH skill policy enable
LibSSH skill policy allow-preset readonly   # 一键导入只读诊断命令集（先用 policy presets 查看内容）
```

说明：

- 旧钥匙串条目首次被新签名访问时会各弹一次授权框，选「始终允许」后永久安静。
- 之后每次重新构建，跑一遍 `scripts/install-macos-app.sh` 即可，签名恒定，不再弹窗。
- 通过自动更新安装的官方构建没有本地签名，更新后需重跑 `scripts/install-macos-app.sh` 恢复。
- `readonly` 预设只含只读诊断命令；预设外命令照旧被挡，按需单独 `policy allow`。
```

- [ ] **Step 8.3: AGENTS.md 被挡提示更新**

把（行 26-27 附近）：

```text
- The CLI is disabled by default. If a command is blocked, tell the user to run
  `LibSSH skill policy enable` / `LibSSH skill policy allow "<prefix>"`. **Never
```

改为：

```text
- The CLI is disabled by default. If a command is blocked, tell the user to run
  `LibSSH skill policy enable` / `LibSSH skill policy allow "<prefix>"` — or
  `LibSSH skill policy allow-preset readonly` for the read-only diagnostics
  bundle. **Never
```

（保留原 bullet 其余文字与后续内容不变。）

- [ ] **Step 8.4: Commit**

```bash
git add README.md AGENTS.md
git commit -m "docs: macOS 免重复授权设置指南与 readonly 预设说明"
```

---

### Task 9: 端到端验证与验收

- [ ] **Step 9.1: 全量自动验证**

```bash
cargo fmt --all --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked 2>&1 | tail -3
```

Expected: 三者全绿。

- [ ] **Step 9.2: 人工验收清单（需要用户在场，逐项打勾）**

```text
[ ] scripts/setup-macos-codesign.sh 成功创建证书（security find-certificate -c "LibSSH Dev Signing" 命中）
[ ] 第二次跑 setup 脚本输出 already exists（幂等）
[ ] scripts/install-macos-app.sh 部署后 codesign -dv /Applications/LibSSH.app 显示
    Identifier=dev.libssh.LibSSH 且 Authority=LibSSH Dev Signing
[ ] LibSSH skill policy enable && LibSSH skill policy allow-preset readonly 成功
[ ] LibSSH skill run --session <某密码认证会话> --command "uptime"：
    首次弹一次钥匙串授权框 → 选「始终允许」→ 命令成功返回
[ ] 再次 run 同一会话：零弹窗
[ ] touch src/main.rs && scripts/install-macos-app.sh（重新构建+部署）后再 run：零弹窗 ←—— 核心验收
[ ] LibSSH skill check --command "rm -rf /tmp/x" 仍被内置策略拒绝
```

- [ ] **Step 9.3: 收尾**

全部通过后，使用 superpowers:finishing-a-development-branch 流程决定合并方式。
