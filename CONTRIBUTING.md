# Contributing to git-warp

Thanks for your interest in contributing! Here's how to get started.

## Development Setup

### Prerequisites

- **Rust 1.70+** via [rustup](https://rustup.rs/)
- **WSL2 (Ubuntu)** or native Linux — the test suite uses Unix symlinks and inotify
- **Docker** (optional — only needed to test the volume shadowing proxy)

### Clone & Build

```bash
git clone https://github.com/Krezmitent/Git-Warp.git
cd git-warp
cargo build
```

### Running Tests

```bash
# All tests
cargo test

# With output visible
cargo test -- --nocapture

# Specific module
cargo test watcher
cargo test docker_proxy
cargo test symlink_manager
cargo test gc
```

### Linting

```bash
cargo clippy -- -D warnings
cargo fmt --check
```

---

## Project Structure

```
src/
├── main.rs              # CLI entry point + daemon orchestration
├── logging.rs           # Structured logging setup
├── watcher.rs           # inotify .git/HEAD file watcher
├── docker_proxy.rs      # Docker Compose volume rewriter
├── symlink_manager.rs   # Content-addressed dependency cache
└── gc.rs                # LRU garbage collector
```

Each module is self-contained with its own unit tests at the bottom of the file.

---

## Guidelines

### Code Style

- **Heavy comments** — This is a systems daemon. Every non-obvious decision should be documented inline.
- **Module-level doc blocks** — Each `.rs` file starts with an ASCII-art header explaining the module's purpose, design rationale, and edge cases.
- **No silent failures** — Every error path must produce a `tracing` log event. Use `tracing::warn!` for recoverable errors, `tracing::error!` for non-recoverable ones.

### Architecture Rules

- **No polling** — Use the `notify` crate's event-driven API. Never `std::thread::sleep` in a loop.
- **No raw Docker API** — Delegate to `docker` / `docker compose` via `std::process::Command` or `tokio::process::Command`.
- **No monolithic files** — Keep modules focused. If a module exceeds ~500 lines, consider splitting it.
- **Unix-first** — Target WSL2/Linux (ext4). No Windows NTFS junction logic in V1.

### Testing

- All new functionality must include unit tests.
- Tests that require filesystem operations should use `tempfile::TempDir` for isolation.
- Tests that require Docker should be clearly marked and skippable.

---

## Pull Request Process

1. **Fork** the repository and create a feature branch from `main`.
2. **Write tests** for your changes.
3. **Ensure CI passes**: `cargo test && cargo clippy -- -D warnings && cargo fmt --check`
4. **Write a clear PR description** explaining what changed and why.
5. **Keep PRs focused** — one feature or fix per PR.

---

## Reporting Issues

When filing a bug report, please include:

- Your OS and kernel version (`uname -a`)
- Rust version (`rustc --version`)
- Docker version (`docker --version`)
- The relevant section of `~/.git-warp/daemon.log`
- Steps to reproduce

---

## License

By contributing, you agree that your contributions will be licensed under the GNU General Public License v3.0.
