# LibSSH

Faithful Rust + Slint replica scaffold for the LibSSH project.

The implementation follows the Obsidian blueprint in `Project_Doc/LibSSH` and is built milestone-by-milestone. Phase 1 preserves the documented current behavior; hardening and improvements are deferred until the replica passes the acceptance checklist.

## Build

```bash
cargo run
cargo build --release
```

The release binary is written to `target/release/LibSSH`.

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
scripts/package-macos-aarch64-dmg.sh target/release/LibSSH dist
```

For an unsigned macOS build downloaded from a release, clear quarantine before opening:

```bash
xattr -dr com.apple.quarantine LibSSH.app
```

GitHub releases are built by `.github/workflows/release.yml` when pushing a `v*` tag.
