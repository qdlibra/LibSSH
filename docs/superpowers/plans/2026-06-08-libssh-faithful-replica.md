# LibSSH Faithful Replica Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `/Library/Data/project/LibSSH` into the faithful Rust + Slint `meatshell` replica specified by the Obsidian `Project_Doc/LibSSH` blueprint.

**Architecture:** Implement in blueprint milestones M0-M11. Keep `app.rs` as the Slint bridge/coordinator, backend modules focused by responsibility, and terminal parsing in `app.rs` rather than `ssh.rs`. Phase 1 preserves current behavior and defers the hardening items from `12-APPENDIX-tech-debt.md`.

**Tech Stack:** Rust 2021, Cargo, Slint 1.8, slint-build 1.8, russh 0.49, russh-sftp 2, vt100 0.15, tokio 1, sysinfo 0.33, serde/serde_json, rfd, arboard, zeroize.

---

## File Structure

- Create `Cargo.toml`: package metadata, exact dependencies, release/dev profiles.
- Create `build.rs`: Slint compile entry, Fluent style, bundled translations, Windows icon embedding.
- Create `src/main.rs`: logging and process entry.
- Create `src/app.rs`: generated Slint module inclusion and application coordinator.
- Create `src/config.rs`: persisted config model and JSON load/save.
- Create `src/i18n.rs`: zh/en runtime language selection.
- Create `src/ssh_config.rs`: SSH config import parser.
- Create `src/system.rs`: local resource sampler.
- Create `src/proxy.rs`: proxy URL resolver and connector.
- Create `src/ssh.rs`: SSH shell worker and event protocol.
- Create `src/sftp.rs`: SFTP worker and file operations.
- Create `ui/*.slint`: Slint components from the blueprint.
- Create `lang/zh/LC_MESSAGES/meatshell.po` and `lang/en/LC_MESSAGES/meatshell.po`: bundled translations.
- Create `assets/` and `.github/workflows/` in M10.

## Current Execution Mode

The user asked to begin implementation now. Execute inline in this thread. Subagents are not used unless the user explicitly requests delegated agent work.

## Milestone Tasks

### Task 0: M0 Scaffolding

**Files:**
- Create: `Cargo.toml`
- Create: `build.rs`
- Create: `src/main.rs`
- Create: `src/app.rs`
- Create: `ui/app.slint`
- Create: `ui/theme.slint`
- Create: `lang/zh/LC_MESSAGES/meatshell.po`
- Create: `lang/en/LC_MESSAGES/meatshell.po`
- Modify: `README.md`

- [x] **Step 1: Write Cargo and build files**

Create `Cargo.toml` from `02-TECH-STACK.md` with all dependencies, Windows build dependency, and profiles. Create `build.rs` with `slint_build::compile_with_config("ui/app.slint", CompilerConfiguration::new().with_style("fluent".into()).with_bundled_translations("lang").with_default_translation_context(DefaultTranslationContext::None))`.

- [x] **Step 2: Write the minimal Slint UI**

Create `ui/theme.slint` with the blueprint color/type/geometry tokens. Create a minimal exported `AppWindow` in `ui/app.slint` with title `meatshell`, default size `1200x760`, minimum size `960x600`, dark root background, and one centered placeholder text.

- [x] **Step 3: Write Rust entry files**

Create `src/main.rs` with `windows_subsystem` release attr, tracing subscriber setup, and `app::run()`. Create `src/app.rs` with `slint::include_modules!()` and `pub fn run() -> anyhow::Result<()>` that constructs and runs `AppWindow`.

- [x] **Step 4: Add gettext placeholders**

Create minimal `.po` files for `zh` and `en` with headers for package `meatshell`. Keep msgids English.

- [x] **Step 5: Run compilation check**

Run: `cargo check`
Expected: build succeeds. New dependency downloads are acceptable.

- [x] **Step 6: Run the app**

Run: `cargo run`
Expected: a dark `meatshell` window opens at the M0 dimensions and closes without an error. If GUI display is unavailable in the environment, document that `cargo check` passed and `cargo run` could not be visually verified.

- [x] **Step 7: Commit M0**

Run:

```bash
git add Cargo.toml Cargo.lock build.rs README.md src ui lang docs/superpowers/plans/2026-06-08-libssh-faithful-replica.md
git commit -m "feat: add M0 scaffolding"
```

Expected: one milestone commit.

### Task 1: M1 Configuration And I18n

**Files:**
- Create: `src/config.rs`
- Create: `src/i18n.rs`
- Modify: `src/main.rs`
- Modify: `src/app.rs`
- Modify: `lang/zh/LC_MESSAGES/meatshell.po`
- Modify: `lang/en/LC_MESSAGES/meatshell.po`

- [ ] Implement `Secret`, `AuthMethod`, `Session`, `ConfigFile`, and `ConfigStore` exactly from `04-DATA-MODEL.md` and `05-BACKEND-core.md`.
- [ ] Add tests for config round trip, missing optional fields, broken JSON backup, and language defaults.
- [ ] Implement `i18n` with default zh, `set_language`, `current_code`, `is_en`, `apply_to_slint`, and `t(zh, en)`.
- [ ] Run `cargo test config i18n` and `cargo check`.
- [ ] Commit with `feat: add config and i18n foundation`.

### Task 2: M2 Static UI Skeleton

**Files:**
- Create: `ui/widgets.slint`
- Create: `ui/tabs.slint`
- Create: `ui/welcome.slint`
- Create: `ui/session_dialog.slint`
- Create: `ui/sidebar.slint`
- Create: `ui/sftp_panel.slint`
- Create: `ui/terminal_view.slint`
- Modify: `ui/app.slint`
- Create: `ui/fonts/*`

- [ ] Recreate all exported structs, properties, and callbacks from `09-UI-slint.md`.
- [ ] Wire fake/empty models in `app.rs` so the UI opens with welcome tab, empty sessions, sidebar placeholders, settings/download/about surfaces.
- [ ] Ensure `@tr(...)` msgids are English and font imports match the blueprint.
- [ ] Run `cargo check` and `cargo run`.
- [ ] Commit with `feat: add static Slint UI skeleton`.

### Task 3: M3 Session CRUD And SSH Config Import

**Files:**
- Create: `src/ssh_config.rs`
- Modify: `src/app.rs`
- Modify: `src/config.rs`
- Test: parser/config tests

- [ ] Test and implement `~/.ssh/config` parsing for Host, HostName, User, Port, IdentityFile, `~/` expansion, wildcard skipping, and duplicate skipping.
- [ ] Wire new/edit/delete/import callbacks in `app.rs`.
- [ ] Preserve password edit semantics: never prefill password; blank edit keeps old password.
- [ ] Run tests, `cargo check`, and manual UI CRUD.
- [ ] Commit with `feat: add session management`.

### Task 4: M4 Local Resource Sidebar

**Files:**
- Create: `src/system.rs`
- Modify: `src/app.rs`
- Modify: `ui/sidebar.slint`

- [ ] Implement `SystemSnapshot`, `SystemSampler`, and formatting helpers.
- [ ] Add 1 Hz Slint timer and model updates.
- [ ] Keep bottom network graph local and warning/danger disk thresholds in Slint.
- [ ] Run `cargo check` and `cargo run`.
- [ ] Commit with `feat: add local resource sidebar`.

### Task 5: M5 SSH Connection And Terminal Rendering

**Files:**
- Create: `src/ssh.rs`
- Modify: `src/app.rs`
- Modify: `ui/terminal_view.slint`

- [ ] Implement `SessionCommand`, `SessionEvent`, `SessionHandle`, SSH connection, auth, PTY, shell, resize, close, and output event pump.
- [ ] Implement `TermBuffer`, HVP rewrite, scrollback, alternate-screen handling, VT color conversion, and `TermSpan` rendering in `app.rs`.
- [ ] Wire connect-session to create independent terminal tabs.
- [ ] Run `cargo check`; manually verify with a reachable SSH host if available.
- [ ] Commit with `feat: add SSH terminal rendering`.

### Task 6: M6 Terminal Interaction

**Files:**
- Modify: `src/app.rs`
- Modify: `ui/terminal_view.slint`

- [ ] Implement key mapping, IME filtering, resize, copy, paste, select-to-copy, search, scrollback, and clear buffer.
- [ ] Keep clipboard operations off the UI thread.
- [ ] Run `cargo check`; manually verify shell input, vim/btop, clipboard, selection, and Chinese IME.
- [ ] Commit with `feat: add terminal interaction`.

### Task 7: M7 Remote Resource Sampling

**Files:**
- Modify: `src/ssh.rs`
- Modify: `src/app.rs`
- Modify: `ui/sidebar.slint`

- [ ] Inject and parse Linux `/proc` and `df -kP` sampling script.
- [ ] Emit and consume `SessionEvent::ResourceStats`.
- [ ] Make active connected tabs show remote resources while welcome and disconnected tabs fall back as documented.
- [ ] Run `cargo check`; manually verify on a Linux SSH host.
- [ ] Commit with `feat: add remote resource sampling`.

### Task 8: M8 SFTP File Management

**Files:**
- Create: `src/sftp.rs`
- Modify: `src/app.rs`
- Modify: `ui/sftp_panel.slint`
- Modify: `ui/terminal_view.slint`

- [ ] Implement `SftpCommand`, `SftpHandle`, `RemoteEntry`, `RemoteTreeNode`, connection, home resolution, list, tree, download, upload, delete, open temp, and edit watcher.
- [ ] Wire SFTP events into `TerminalState` and transfer manager.
- [ ] Implement OSC7 follow with 500 ms debounce and permanent manual-nav stop.
- [ ] Add Windows drop-zone upload behavior; keep non-Windows behavior as documented.
- [ ] Run `cargo check`; manually verify SFTP flows on a reachable host.
- [ ] Commit with `feat: add SFTP file management`.

### Task 9: M9 Proxy Support

**Files:**
- Create: `src/proxy.rs`
- Modify: `src/ssh.rs`
- Modify: `src/sftp.rs`
- Modify: `src/config.rs`

- [ ] Test and implement proxy URL parsing, environment fallback, SOCKS5 connect, and HTTP CONNECT with optional Basic auth.
- [ ] Route SSH and SFTP through the resolved proxy.
- [ ] Run proxy tests and `cargo check`; manually verify SOCKS5 and HTTP proxy paths if available.
- [ ] Commit with `feat: add proxy support`.

### Task 10: M10 Release And Platform Integration

**Files:**
- Create: `assets/*`
- Create: `.github/workflows/release.yml`
- Create/Modify: packaging scripts and README files
- Modify: `build.rs`

- [ ] Add icon assets, Linux desktop entry, Linux install script, macOS packaging guidance, and GitHub release workflow.
- [ ] Ensure Windows icon embedding and Linux app id/desktop entry alignment.
- [ ] Run `cargo build --release` locally for the current platform.
- [ ] Commit with `chore: add release packaging assets`.

### Task 11: M11 Replica Security And Acceptance Sweep

**Files:**
- Modify as needed across `src/`, `ui/`, `README*`, and `docs/`

- [ ] Audit against `11-ACCEPTANCE.md`.
- [ ] Confirm replica keeps current behavior for accepted-any-host-key and plaintext config passwords.
- [ ] Confirm no raw key bytes or sensitive text are logged.
- [ ] Confirm temp filenames are sanitized and local opens avoid shell command concatenation.
- [ ] Run `cargo test`, `cargo check`, and `cargo build --release`.
- [ ] Commit with `chore: complete faithful replica sweep`.

## Phase 2 Placeholder

After M11 passes, create a separate design/plan for hardening based on `12-APPENDIX-tech-debt.md`: known_hosts, keychain, confirmations, transfer cancellation, structured errors, and other requested improvements.
