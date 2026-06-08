# LibSSH Replica Security Notes

This document records the security posture of the faithful replica phase. It distinguishes the behavior intentionally preserved from the original blueprint from the hardening work that should happen after replica acceptance.

## Preserved Behavior

- SSH and SFTP accept any server host key. This matches the documented current behavior and is equivalent to disabling strict host-key checking.
- Session passwords are stored in `sessions.json`. The in-memory `Secret` type zeroizes its owned buffer on drop, but the persisted configuration is still plaintext.
- SFTP Delete removes files or empty directories directly, with the remote server returning errors for unsupported or unauthorized deletes.
- SFTP Edit downloads to a sanitized local temp filename, opens it through the OS, polls for local modifications, and uploads changes back automatically.
- Non-Windows local file opening uses `xdg-open`, matching the blueprint's current non-Windows behavior.

## Current Safety Discipline

- Password values use the `Secret` wrapper and are redacted from `Debug`.
- Terminal input logging records byte counts only; it does not log raw key bytes or typed text.
- Remote filenames are sanitized before being written into the local temp directory for View/Edit.
- Local file opening passes paths as process arguments instead of shell-concatenated commands.
- Configuration JSON parse failures are backed up as `.broken` files and the app continues with defaults.

## Deferred Hardening

- known_hosts storage and first-connection fingerprint confirmation.
- OS keychain integration for passwords and private-key passphrases.
- Delete and overwrite confirmation dialogs.
- Transfer cancellation and partial-file cleanup.
- Structured user-visible errors for authentication, proxy, host-key, and SFTP failures.
