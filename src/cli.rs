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
            let session = find_session(store.sessions(), &session)
                .with_context(|| format!("session not found: {session}"))?
                .clone();
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
        Some("allow") => Ok(CliAction::PolicyAllow(required_positional(args, 4)?)),
        Some("deny") => Ok(CliAction::PolicyDeny(required_positional(args, 4)?)),
        Some("remove-allow") => Ok(CliAction::PolicyRemoveAllow(required_positional(args, 4)?)),
        Some("remove-deny") => Ok(CliAction::PolicyRemoveDeny(required_positional(args, 4)?)),
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

fn required_positional(args: &[String], index: usize) -> Result<String> {
    args.get(index)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .context("missing command prefix")
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
    r#"# LibSSH Safe CLI

Use this skill only for SSH tasks through the local `LibSSH skill` CLI. Never ask the user for passwords, private keys, proxy credentials, API tokens, or other secrets. The CLI reads saved sessions locally and redacts sensitive output before returning data to the AI tool.

## Commands

- List non-secret sessions: `LibSSH skill sessions`
- Check whether a command is allowed: `LibSSH skill check --command "uptime"`
- Run an allowed command: `LibSSH skill run --session "<id-or-name>" --command "uptime"`

## Guardrails

- The CLI is disabled until the user runs `LibSSH skill policy enable`.
- Remote commands are denied until added with `LibSSH skill policy allow "<command-prefix>"`.
- Deny rules configured with `LibSSH skill policy deny "<command-prefix>"` override allow rules.
- Built-in destructive and secret-prone command prefixes such as `rm`, `dd`, `mkfs`, `shutdown`, `reboot`, `passwd`, `sudo`, `su`, `env`, `printenv`, secret-manager CLIs, and `kubectl ... secret` are always blocked.
- Treat all command output as potentially sensitive; rely on the CLI redaction and do not request broader allow rules than needed.
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
}
