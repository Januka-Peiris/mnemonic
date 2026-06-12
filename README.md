# Mnemonic

An AI-native coding workspace built on top of [Zed](https://github.com/zed-industries/zed).

Mnemonic manages projects, git worktrees, and AI agent sessions in a single interface. Each workspace gets its own isolated worktree and terminal running Claude Code, so you can work on multiple tasks across multiple repos at once.

## Building

```bash
cargo run -p mnemonic
```

Requires Rust 1.95.0 (pinned in `rust-toolchain.toml`). First build takes around 5 minutes, incremental builds are 10-15 seconds.

## Architecture

```
crates/mnemonic-core/       Backend brain - services, repos, models, SQLite
crates/mnemonic-bridge/     Event bridge between mnemonic-core and GPUI
crates/mnemonic-panels/     Projects panel, command approval, coordinator
crates/zed/                 Main app binary (renamed to "mnemonic")
```

## License

Built on Zed, which is licensed under GPL-3.0-or-later with Apache-2.0 components where marked.
