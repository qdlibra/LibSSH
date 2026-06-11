---
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
| Import the read-only diagnostics preset | `LibSSH skill policy allow-preset readonly` |

## When a command is blocked

The CLI is **disabled by default** and denies every command until the user opts in. If `check`/`run` reports disabled, not-allowed, or blocked:

- Tell the user what to run, and let them run it: `LibSSH skill policy enable`, then `LibSSH skill policy allow "<command-prefix>"`. For routine read-only diagnostics, suggest the one-shot preset instead of piecemeal rules: `LibSSH skill policy allow-preset readonly` (the user can inspect it first with `LibSSH skill policy presets`).
- **Never work around a block.** If `rm` is blocked, do not substitute `find -delete`, `truncate`, `: >`, or any equivalent. Do not escalate with `sudo`/`su` or rewrite a command to dodge the policy. Report the block and stop.
- Destructive and secret-reading prefixes (`rm`, `dd`, `mkfs`, `shutdown`, `reboot`, `passwd`, `sudo`, `su`, `env`, `printenv`, secret managers, `kubectl ... secret`) are always blocked by design. Surface them for the user to handle manually in the LibSSH GUI.

## Output handling

Treat all command output as potentially sensitive. Rely on the CLI's redaction, do not echo back anything that still looks like a credential, and never request broader allow rules than the task needs.

