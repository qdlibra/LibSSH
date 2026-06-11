use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::config::{AiSessionSummary, ConfigStore, Session};

#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    ExportSkill,
    ListSessions,
    PolicyShow,
    PolicyEnable(bool),
    PolicyAllow(String),
    PolicyDeny(String),
    PolicyRemoveAllow(String),
    PolicyRemoveDeny(String),
    PolicyPresets,
    PolicyAllowPreset(String),
    Check { command: String },
    Run { session: String, command: String },
}

pub fn handles_args(args: &[String]) -> bool {
    args.get(1).is_some_and(|arg| arg == "skill")
}

pub fn run_args(args: &[String]) -> Result<()> {
    match parse_args(args)? {
        CliAction::ExportSkill => {
            println!("{}", generated_skill_markdown());
        }
        CliAction::ListSessions => {
            let store = ConfigStore::load()?;
            let sessions: Vec<AiSessionSummary> = store
                .sessions()
                .iter()
                .map(AiSessionSummary::from)
                .collect();
            print_json(&sessions)?;
        }
        CliAction::PolicyShow => {
            let store = ConfigStore::load()?;
            print_json(store.ai_skill())?;
        }
        CliAction::PolicyEnable(enabled) => {
            let mut store = ConfigStore::load()?;
            store.ai_skill_mut().enabled = enabled;
            store.save()?;
            print_json(&StatusMessage::ok(if enabled {
                "AI skill CLI enabled"
            } else {
                "AI skill CLI disabled"
            }))?;
        }
        CliAction::PolicyAllow(command) => {
            let mut store = ConfigStore::load()?;
            add_unique(&mut store.ai_skill_mut().allowed_commands, command);
            store.save()?;
            print_json(&StatusMessage::ok("allowed command added"))?;
        }
        CliAction::PolicyDeny(command) => {
            let mut store = ConfigStore::load()?;
            add_unique(&mut store.ai_skill_mut().denied_commands, command);
            store.save()?;
            print_json(&StatusMessage::ok("denied command added"))?;
        }
        CliAction::PolicyRemoveAllow(command) => {
            let mut store = ConfigStore::load()?;
            remove_value(&mut store.ai_skill_mut().allowed_commands, &command);
            store.save()?;
            print_json(&StatusMessage::ok("allowed command removed"))?;
        }
        CliAction::PolicyRemoveDeny(command) => {
            let mut store = ConfigStore::load()?;
            remove_value(&mut store.ai_skill_mut().denied_commands, &command);
            store.save()?;
            print_json(&StatusMessage::ok("denied command removed"))?;
        }
        CliAction::PolicyPresets => {
            print_json(&preset_listing())?;
        }
        CliAction::PolicyAllowPreset(name) => {
            let commands = crate::config::preset_commands(&name)
                .with_context(|| format!("unknown preset: {name} (available: readonly)"))?;
            let mut store = ConfigStore::load()?;
            let before = store.ai_skill().allowed_commands.len();
            for command in commands {
                add_unique(
                    &mut store.ai_skill_mut().allowed_commands,
                    (*command).to_string(),
                );
            }
            store.save()?;
            let added = store.ai_skill().allowed_commands.len() - before;
            let message = format!("preset {name} applied ({added} commands added)");
            print_json(&StatusMessage::ok(&message))?;
        }
        CliAction::Check { command } => {
            let store = ConfigStore::load()?;
            let decision = match store.ai_skill().evaluate_command(&command) {
                Ok(()) => CommandDecision::allowed(&command),
                Err(reason) => CommandDecision::denied(&command, &reason),
            };
            print_json(&decision)?;
        }
        CliAction::Run { session, command } => {
            let store = ConfigStore::load()?;
            if let Err(reason) = store.ai_skill().evaluate_command(&command) {
                print_json(&CommandDecision::denied(&command, &reason))?;
                bail!(reason);
            }
            let mut session = find_session(store.sessions(), &session)
                .with_context(|| format!("session not found: {session}"))?
                .clone();
            // json 里密码为空（已迁入系统凭据库）时回查 keyring，
            // 且在 redaction 取值之前完成，保证真实密码也被打码。
            crate::secrets::resolve_session_password(&mut session);
            let session = session;
            let secrets = secret_values_for_session(&session);
            let runtime = tokio::runtime::Runtime::new().context("create CLI runtime")?;
            match runtime.block_on(crate::ssh::run_exec(session, &command)) {
                Ok(result) => {
                    print_json(&ExecOutput {
                        exit_status: result.exit_status,
                        stdout: redact_for_llm(&result.stdout, &secrets),
                        stderr: redact_for_llm(&result.stderr, &secrets),
                    })?;
                }
                Err(err) => {
                    let redacted = redact_for_llm(&format!("{err:#}"), &secrets);
                    print_json(&StatusMessage::error(&redacted))?;
                    bail!(redacted);
                }
            }
        }
    }
    Ok(())
}

fn parse_args(args: &[String]) -> Result<CliAction> {
    if args.get(1).map(String::as_str) != Some("skill") {
        bail!("expected `skill` subcommand");
    }
    match args.get(2).map(String::as_str) {
        Some("export") => Ok(CliAction::ExportSkill),
        Some("sessions") => Ok(CliAction::ListSessions),
        Some("check") => Ok(CliAction::Check {
            command: flag_value(args, "--command")?,
        }),
        Some("run") => Ok(CliAction::Run {
            session: flag_value(args, "--session")?,
            command: flag_value(args, "--command")?,
        }),
        Some("policy") => parse_policy_args(args),
        Some("--help") | Some("help") | None => {
            println!("{}", help_text());
            std::process::exit(0);
        }
        Some(other) => bail!("unknown skill subcommand: {other}"),
    }
}

fn parse_policy_args(args: &[String]) -> Result<CliAction> {
    match args.get(3).map(String::as_str) {
        Some("show") => Ok(CliAction::PolicyShow),
        Some("enable") => Ok(CliAction::PolicyEnable(true)),
        Some("disable") => Ok(CliAction::PolicyEnable(false)),
        Some("presets") => Ok(CliAction::PolicyPresets),
        Some("allow-preset") => Ok(CliAction::PolicyAllowPreset(required_positional(
            args,
            4,
            "preset name",
        )?)),
        Some("allow") => Ok(CliAction::PolicyAllow(required_positional(
            args,
            4,
            "command prefix",
        )?)),
        Some("deny") => Ok(CliAction::PolicyDeny(required_positional(
            args,
            4,
            "command prefix",
        )?)),
        Some("remove-allow") => Ok(CliAction::PolicyRemoveAllow(required_positional(
            args,
            4,
            "command prefix",
        )?)),
        Some("remove-deny") => Ok(CliAction::PolicyRemoveDeny(required_positional(
            args,
            4,
            "command prefix",
        )?)),
        Some(other) => bail!("unknown skill policy command: {other}"),
        None => bail!("missing skill policy command"),
    }
}

fn flag_value(args: &[String], flag: &str) -> Result<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].trim().to_string())
        .filter(|value| !value.is_empty())
        .with_context(|| format!("missing {flag} value"))
}

fn required_positional(args: &[String], index: usize, what: &str) -> Result<String> {
    args.get(index)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .with_context(|| format!("missing {what}"))
}

fn add_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn remove_value(values: &mut Vec<String>, value: &str) {
    values.retain(|existing| existing != value);
}

fn find_session<'a>(sessions: &'a [Session], selector: &str) -> Option<&'a Session> {
    sessions
        .iter()
        .find(|session| session.id == selector || session.name == selector)
}

fn secret_values_for_session(session: &Session) -> Vec<String> {
    [
        session.password.as_str().to_string(),
        session.private_key_path.clone(),
        session.proxy.clone(),
    ]
    .into_iter()
    .filter(|value| value.trim().len() >= 3)
    .collect()
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn redact_for_llm(text: &str, secret_values: &[impl AsRef<str>]) -> String {
    let mut redacted = text.to_string();
    for secret in secret_values {
        let secret = secret.as_ref().trim();
        if secret.len() >= 3 {
            redacted = redacted.replace(secret, "[REDACTED]");
        }
    }
    redacted
        .lines()
        .map(redact_sensitive_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_sensitive_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let has_sensitive_key = [
        "password",
        "passwd",
        "token",
        "api_key",
        "apikey",
        "secret",
        "credential",
        "private_key",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if !has_sensitive_key {
        return line.to_string();
    }
    for delimiter in ['=', ':'] {
        if let Some((left, _)) = line.split_once(delimiter) {
            return format!("{}{delimiter} [REDACTED]", left.trim_end());
        }
    }
    "[REDACTED]".to_string()
}

fn generated_skill_markdown() -> &'static str {
    r#"---
name: libssh
description: Use when the user wants to inspect or run a command on a remote server already saved in LibSSH (check load average, disk usage, service status, or logs on a host they name such as prod). Triggers on operating a known remote machine through LibSSH instead of a raw ssh connection.
---

# LibSSH Safe CLI

Operate the user's saved remote servers **only** through the local `LibSSH skill` CLI. It reads saved sessions locally, enforces an allow/deny policy, and redacts secrets before any output reaches you. Do not open your own `ssh` connection, do not read `~/.ssh/*` or app config, and do not reach for another server tool — route everything through this CLI.

## When to use

The user asks you to check, monitor, or run a command on a remote host they refer to by name (for example "prod" or "staging") that is managed in LibSSH.

## Workflow

1. Discover hosts: `LibSSH skill sessions` (already redacted — no secrets).
2. Pre-check: `LibSSH skill check --command "uptime"` confirms the policy allows it.
3. Run: `LibSSH skill run --session "<id-or-name>" --command "uptime"` returns redacted JSON.

Never ask the user for passwords, private keys, proxy credentials, or API tokens — credentials stay inside LibSSH.

## Quick reference

| Goal | Command |
| --- | --- |
| List saved hosts | `LibSSH skill sessions` |
| Show current policy | `LibSSH skill policy show` |
| Check a command | `LibSSH skill check --command "<cmd>"` |
| Run a command | `LibSSH skill run --session "<id-or-name>" --command "<cmd>"` |

## When a command is blocked

The CLI is **disabled by default** and denies every command until the user opts in. If `check`/`run` reports disabled, not-allowed, or blocked:

- Tell the user what to run, and let them run it: `LibSSH skill policy enable`, then `LibSSH skill policy allow "<command-prefix>"`.
- **Never work around a block.** If `rm` is blocked, do not substitute `find -delete`, `truncate`, `: >`, or any equivalent. Do not escalate with `sudo`/`su` or rewrite a command to dodge the policy. Report the block and stop.
- Destructive and secret-reading prefixes (`rm`, `dd`, `mkfs`, `shutdown`, `reboot`, `passwd`, `sudo`, `su`, `env`, `printenv`, secret managers, `kubectl ... secret`) are always blocked by design. Surface them for the user to handle manually in the LibSSH GUI.

## Output handling

Treat all command output as potentially sensitive. Rely on the CLI's redaction, do not echo back anything that still looks like a credential, and never request broader allow rules than the task needs.
"#
}

fn help_text() -> &'static str {
    "Usage:
  LibSSH skill export
  LibSSH skill sessions
  LibSSH skill policy show|enable|disable
  LibSSH skill policy allow <command-prefix>
  LibSSH skill policy deny <command-prefix>
  LibSSH skill policy remove-allow <command-prefix>
  LibSSH skill policy remove-deny <command-prefix>
  LibSSH skill policy presets
  LibSSH skill policy allow-preset <preset-name>
  LibSSH skill check --command <command>
  LibSSH skill run --session <id-or-name> --command <command>"
}

#[derive(Serialize)]
struct StatusMessage<'a> {
    ok: bool,
    message: &'a str,
}

impl<'a> StatusMessage<'a> {
    fn ok(message: &'a str) -> Self {
        Self { ok: true, message }
    }

    fn error(message: &'a str) -> Self {
        Self { ok: false, message }
    }
}

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

#[derive(Serialize)]
struct CommandDecision {
    allowed: bool,
    command: String,
    reason: String,
}

impl CommandDecision {
    fn allowed(command: &str) -> Self {
        Self {
            allowed: true,
            command: redact_for_llm(command, &Vec::<String>::new()),
            reason: "allowed".to_string(),
        }
    }

    fn denied(command: &str, reason: &str) -> Self {
        Self {
            allowed: false,
            command: redact_for_llm(command, &Vec::<String>::new()),
            reason: reason.to_string(),
        }
    }
}

#[derive(Serialize)]
struct ExecOutput {
    exit_status: Option<u32>,
    stdout: String,
    stderr: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_skill_run_session_and_command() {
        let args = vec![
            "LibSSH".to_string(),
            "skill".to_string(),
            "run".to_string(),
            "--session".to_string(),
            "prod".to_string(),
            "--command".to_string(),
            "uptime".to_string(),
        ];

        let parsed = parse_args(&args).unwrap();

        assert_eq!(
            parsed,
            CliAction::Run {
                session: "prod".to_string(),
                command: "uptime".to_string(),
            }
        );
    }

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

    #[test]
    fn redacts_common_secret_shapes_before_printing_to_ai_tool() {
        let redacted = redact_for_llm(
            "password=hunter2\napi_token: abc123\nnormal line\n/Users/me/.ssh/prod.pem",
            &["hunter2", "/Users/me/.ssh/prod.pem"],
        );

        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("abc123"));
        assert!(!redacted.contains("prod.pem"));
        assert!(redacted.contains("normal line"));
    }

    #[test]
    fn export_emits_valid_skill_frontmatter() {
        let md = generated_skill_markdown();
        assert!(
            md.starts_with("---\n"),
            "exported SKILL.md must open with YAML frontmatter"
        );
        let body_after_open = &md[4..];
        let close = body_after_open
            .find("\n---\n")
            .expect("frontmatter must be closed with a --- line");
        let front = &body_after_open[..close];
        assert!(front.contains("name:"), "frontmatter needs a name field");
        assert!(
            front.contains("description:"),
            "frontmatter needs a description field"
        );
    }
}
