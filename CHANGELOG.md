# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2025-05-30

### Added

- **Event-Driven Git Watcher** — inotify-backed `.git/HEAD` monitoring with 50ms debounce. Zero polling.
- **Docker Volume Shadow Proxy** — Intercepts `docker-compose` to scope volumes per-branch via `--warp--` naming.
- **O(1) Dependency Resurrection** — SHA-256 content-addressed cache for `node_modules` with atomic symlink swapping.
- **LRU Garbage Collector** — Three-phase cleanup (cache entries, Docker volumes, compose files) with 7-day default TTL.
- **Structured Logging** — Dual-layer tracing: JSON to `~/.git-warp/daemon.log` + compact ANSI to stderr.
- **CLI** — `start-daemon`, `proxy`, `gc`, `status` subcommands via clap.
- **Package Manager Support** — npm, Yarn, pnpm, and Bun lockfile detection.
