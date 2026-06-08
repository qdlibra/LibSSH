# LibSSH Faithful Replica Design

Date: 2026-06-08

## Goal

Build the project in `/Library/Data/project/LibSSH` from an empty workspace into a faithful Rust + Slint replica of the `LibSSH` SSH/SFTP desktop client described in the Obsidian knowledge base at `Project_Doc/LibSSH`.

The first phase is strict replication. Optional improvements and hardening from `12-APPENDIX-tech-debt.md` are deferred until the full replica passes the documented acceptance checklist.

## Source Of Truth

The implementation follows the Obsidian blueprint documents:

- `00-GENERATION-GUIDE.md` for workflow constraints
- `01-OVERVIEW.md` for product behavior and architecture
- `02-TECH-STACK.md` for Cargo metadata, dependency versions, build profile, and Slint build setup
- `03-PROJECT-LAYOUT.md` for directory structure and module boundaries
- `04-DATA-MODEL.md` for Rust and Slint data structures
- `05-BACKEND-core.md` through `08-BACKEND-app.md` for backend behavior
- `09-UI-slint.md` for Slint UI contracts
- `10-BUILD-PLAN.md` for milestone order
- `11-ACCEPTANCE.md` for verification
- `12-APPENDIX-tech-debt.md` only as a list of deferred improvements

When behavior and improvement ideas conflict, the faithful replica keeps the current behavior documented in the mainline specs.

## Architecture

The project will use Rust 2021 with Slint 1.8 and the dependency set from `02-TECH-STACK.md`. The package metadata remains aligned with the blueprint: `LibSSH` version `0.2.3`, with `build.rs` compiling `ui/app.slint`, Fluent style, and bundled gettext translations from `lang`.

The module structure mirrors the blueprint:

- `src/main.rs`: logging setup and call into `app::run()`
- `src/app.rs`: Slint bridge and application coordinator
- `src/config.rs`: persisted JSON configuration and session model
- `src/system.rs`: local system resource sampling
- `src/ssh_config.rs`: `~/.ssh/config` import
- `src/i18n.rs`: runtime zh/en language selection
- `src/proxy.rs`: SOCKS5 and HTTP CONNECT proxy resolution
- `src/ssh.rs`: SSH shell, PTY, remote output, current-directory events, and remote resource sampling
- `src/sftp.rs`: independent SFTP worker and file operations
- `ui/*.slint`: theme, widgets, tabs, welcome page, session dialog, sidebar, terminal view, SFTP panel, and app shell

The Slint UI remains single-threaded. Background SSH, SFTP, and sampling work reports back through channels, and UI updates are performed via `slint::invoke_from_event_loop` with weak handle upgrade.

## Runtime Model

Each opened terminal tab owns two independent workers:

- one SSH shell worker for PTY input/output, resize, remote resource sampling, and OSC7 current-directory events
- one SFTP worker for browsing, upload, download, delete, view, and edit flows

The terminal renderer uses `vt100` plus the documented local `TermBuffer` logic. Output parsing stays in `app.rs`; `ssh.rs` acts as the byte pump and event producer. Scrollback keeps both the `vt100` internal 5000-line buffer and the app-level `MAX_HISTORY = 100000` history.

Persistent state is limited to `sessions.json`: sessions, download directory, and language. Passwords are stored as plaintext in the file for the replica phase, while in-memory password buffers use `zeroize` as documented.

## Functional Scope

The faithful replica includes:

- session CRUD
- import from `~/.ssh/config`
- JSON config load/save, defaulting, and broken-file backup
- static and dynamic Chinese/English UI switching
- tab management with non-closable welcome tab
- local resource sidebar and network history
- SSH password/key authentication
- PTY resize and terminal output rendering
- key mapping, IME filtering, copy/paste, selection, scrollback, search, clear buffer
- Linux remote resource sampling through `/proc` and `df`
- SFTP directory tree, browsing, transfer progress, upload, download, delete, view, edit, and OSC7 follow
- SOCKS5 and HTTP CONNECT proxy support
- release/platform assets from the blueprint
- replica-phase security discipline: do not log raw key bytes, sanitize temporary filenames, do not open files via shell string concatenation

The faithful replica intentionally does not include hardening improvements until phase 2:

- known_hosts enforcement
- OS keychain credential storage
- delete/overwrite confirmation
- transfer cancellation
- structured retry/error UX
- SFTP follow toggle
- broader terminal engine replacement
- non-Linux remote resource providers

## Milestone Plan

Implementation follows `10-BUILD-PLAN.md`:

1. M0: Cargo/Slint/i18n scaffold and runnable empty window
2. M1: config, core data model, i18n runtime, SSH config parser
3. M2: static Slint UI skeleton
4. M3: session CRUD, import, settings, language, about
5. M4: local resource sidebar
6. M5: SSH connection and terminal rendering
7. M6: terminal interaction
8. M7: remote resource sampling
9. M8: SFTP file management
10. M9: proxy support
11. M10: release assets and platform integration
12. M11: replica security and acceptance sweep

Each milestone should end with `cargo check`. Runnable milestones should also use `cargo run` where practical. SSH/SFTP behavior that requires a real host will be reported as manually verifiable if no host credentials are available.

## Verification

Verification is driven by `11-ACCEPTANCE.md`. Automated checks are expected to include at minimum:

- `cargo check`
- targeted unit tests for pure parsing/mapping logic where practical, especially `ssh_config`, proxy parsing, config defaulting, and key mapping helpers if isolated
- `cargo build --release` at the end of the faithful replica

Manual verification is required for true end-to-end behavior:

- launching the Slint app
- SSH login to a reachable Linux host
- terminal behavior with commands such as `vim`, `btop`, `tmux`, and Chinese input
- SFTP browse/upload/download/delete/view/edit
- proxy connection paths

## Open Constraints

The workspace is currently not a git repository, so the design cannot be committed unless git is initialized or the user provides an existing repository. The implementation can proceed without git, but milestone commits from the blueprint will be skipped or replaced by milestone status reports until git is available.
