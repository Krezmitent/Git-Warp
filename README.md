<p align="center">
  <h1 align="center">⚡ git-warp</h1>
  <p align="center">
    <strong>An Environment Time Machine for Local Development</strong>
  </p>
  <p align="center">
    Zero-overhead daemon that eliminates build times and isolates database state per-branch.<br/>
    Switch branches → dependencies and databases swap in O(1) time.
  </p>
  <p align="center">
    <a href="#installation"><img src="https://img.shields.io/badge/platform-WSL2%20%7C%20Linux-blue?style=flat-square" alt="Platform"></a>
    <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/rust-2021_edition-orange?style=flat-square&logo=rust" alt="Rust"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0-green?style=flat-square" alt="License"></a>
  </p>
</p>

---

## The Problem

Every branch switch in a full-stack project triggers a cascade of pain:

```
git checkout feature-branch
npm install                     # 30-120 seconds ⏳
docker-compose down && up       # database reset, data lost 💀
```

Multiply by **20+ branch switches per day** across a team, and you're burning hours.

## The Solution

`git-warp` runs as a background daemon, watching `.git/HEAD` via inotify. The microsecond you switch branches:

1. **📦 Dependencies** — Symlinks swap `node_modules` to a content-addressed cache. If you've ever been on this branch before, it's instant.
2. **🐳 Docker Volumes** — Database volumes are transparently scoped per-branch. Your `main` branch Postgres data is completely isolated from `feature-branch`.
3. **🧹 Garbage Collection** — An LRU eviction policy automatically purges stale caches after 7 days of inactivity.

**Zero config. Zero overhead. Just `git checkout` and go.**

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        git-warp daemon                       │
│                                                              │
│   ┌──────────────┐    ┌──────────────┐    ┌──────────────┐  │
│   │   watcher    │───▶│   symlink    │    │   docker     │  │
│   │  (inotify)   │    │   manager    │    │   proxy      │  │
│   │              │    │              │    │              │  │
│   │ .git/HEAD ───│    │ O(1) swap    │    │ volume       │  │
│   │  monitoring  │    │ node_modules │    │ shadowing    │  │
│   └──────────────┘    └──────────────┘    └──────────────┘  │
│          │                                       │          │
│          ▼                                       ▼          │
│   ┌──────────────┐                        ┌──────────────┐  │
│   │     gc       │                        │  compose     │  │
│   │  (LRU 7d)    │                        │  rewriter    │  │
│   └──────────────┘                        └──────────────┘  │
│                                                              │
│   Cache: ~/.git-warp/                                        │
│   Logs:  ~/.git-warp/daemon.log (structured JSON)            │
└─────────────────────────────────────────────────────────────┘
```

## Installation

### Prerequisites

- **WSL2 (Ubuntu)** or native Linux (ext4/APFS)
- **Rust 1.70+** (install via [rustup](https://rustup.rs/))
- **Docker** with Docker Compose v2 (optional — only needed for volume shadowing)

### Build from Source

```bash
git clone https://github.com/<your-username>/git-warp.git
cd git-warp
cargo build --release

# Copy to your PATH
sudo cp target/release/git-warp /usr/local/bin/
```

### Verify Installation

```bash
git-warp --version
git-warp --help
```

---

## Usage

### 1. Start the Daemon

Navigate to any git repository and start the daemon:

```bash
cd ~/my-project
git-warp start-daemon
```

The daemon will:
- Watch `.git/HEAD` for branch changes (via inotify — zero polling)
- Automatically swap `node_modules` on every branch switch
- Run garbage collection every hour in the background
- Log all activity to `~/.git-warp/daemon.log`

**Options:**

```bash
# Custom GC interval (seconds)
git-warp start-daemon --gc-interval 1800

# Custom repo path
git-warp -r /path/to/repo start-daemon

# Custom cache directory
git-warp --warp-home /tmp/warp-cache start-daemon
```

### 2. Docker Volume Shadowing

Instead of running `docker-compose` directly, proxy it through `git-warp`:

```bash
# Instead of: docker-compose up -d
git-warp proxy -- up -d

# Specify a custom compose file
git-warp proxy -f docker-compose.prod.yml -- up -d

# Any docker-compose command works
git-warp proxy -- logs -f
git-warp proxy -- down
```

**What happens under the hood:**

```yaml
# ORIGINAL docker-compose.yml          # REWRITTEN (branch: feature/login)
services:                               services:
  db:                                     db:
    volumes:                                volumes:
      - ./pgdata:/var/lib/data                - ./pgdata--warp--feature-login:/var/lib/data
      - db_data:/backup                       - db_data--warp--feature-login:/backup

volumes:                                volumes:
  db_data:                                db_data--warp--feature-login:
```

Each branch gets its own isolated database volume. Switching back restores the previous state instantly.

### 3. Manual Garbage Collection

```bash
# Preview what would be deleted
git-warp gc --dry-run

# Run GC with default 7-day TTL
git-warp gc

# Custom TTL
git-warp gc --max-age-days 14
```

### 4. Check Status

```bash
git-warp status
```

```
📊 git-warp status
   Warp home: /home/user/.git-warp
   Repo root: /home/user/my-project
   Cached dependency snapshots: 5
   Shadow volume configs: 3
```

---

## How the Dependency Cache Works

```
Branch: main
  package-lock.json  ──SHA-256──▶  a1b2c3d4e5f6a7b8

  node_modules/ ──symlink──▶ ~/.git-warp/cache/a1b2c3d4e5f6a7b8/node_modules/

Branch: feature/auth
  package-lock.json  ──SHA-256──▶  f9e8d7c6b5a4e3d2

  node_modules/ ──symlink──▶ ~/.git-warp/cache/f9e8d7c6b5a4e3d2/node_modules/
```

**First time on a branch:** Cache miss → you run `npm install` once → git-warp stashes it.

**Every subsequent switch:** Cache hit → symlink swap in <1ms.

### Supported Package Managers

| Manager | Lockfile | Status |
|---------|----------|--------|
| npm     | `package-lock.json` | ✅ Supported |
| Yarn    | `yarn.lock` | ✅ Supported |
| pnpm    | `pnpm-lock.yaml` | ✅ Supported |
| Bun     | `bun.lockb` | ✅ Supported |

---

## Cache Directory Structure

```
~/.git-warp/
├── cache/                          # Content-addressed dependency cache
│   ├── a1b2c3d4e5f6a7b8/
│   │   ├── node_modules/           # Actual dependency tree
│   │   ├── .lockfile_hash          # Full SHA-256 for verification
│   │   ├── .lockfile_name          # "package-lock.json"
│   │   └── .last_used              # RFC3339 timestamp (for GC)
│   └── f9e8d7c6b5a4e3d2/
│       └── ...
├── volumes/                        # Rewritten docker-compose files
│   └── docker-compose--feature-login--a1b2c3d4.yml
└── daemon.log                      # Structured JSON logs (daily rotation)
```

---

## Logging & Debugging

All daemon activity is logged as structured JSON to `~/.git-warp/daemon.log`.

**Real-time monitoring:**
```bash
tail -f ~/.git-warp/daemon.log | jq .
```

**Filter branch changes:**
```bash
tail -f ~/.git-warp/daemon.log | jq 'select(.fields.message == "Branch change detected")'
```

**Increase verbosity:**
```bash
RUST_LOG=git_warp=debug git-warp start-daemon
```

---

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `GIT_WARP_HOME` | Override the cache/config directory | `~/.git-warp` |
| `RUST_LOG` | Control log verbosity ([tracing syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)) | `git_warp=info,warn` |

---

## Project Structure

```
src/
├── main.rs              # CLI (clap) + daemon orchestration
├── logging.rs           # Dual-layer tracing (JSON file + ANSI stderr)
├── watcher.rs           # inotify-backed .git/HEAD watcher + 50ms debounce
├── docker_proxy.rs      # Compose YAML volume rewriter + docker-compose delegation
├── symlink_manager.rs   # SHA-256 lockfile hashing + atomic symlink swap
└── gc.rs                # 3-phase LRU garbage collector (cache + volumes + compose)
```

## Design Decisions

| Decision | Rationale |
|----------|-----------|
| Watch `.git/` dir, not `.git/HEAD` | `git checkout` does atomic rename (HEAD.lock → HEAD), replacing the inode. Direct file watches break silently. |
| 50ms debounce window | A single `git checkout` fires 2-4 fs events. Debouncing collapses them into one callback. |
| Content-addressed cache (SHA-256) | Two branches with identical lockfiles share the same cache entry. No duplication. |
| `--warp--` delimiter in volume names | Prevents collisions with user-named volumes. Easy to filter with `docker volume ls --filter`. |
| Docker Compose v2 → v1 fallback | Tries `docker compose` (plugin) first, falls back to `docker-compose` (standalone). |
| JSON logs to file, compact to stderr | Machine-parseable logs for daemon mode; human-readable for interactive use. |
| GC errors are never fatal | Daemon resilience over correctness — a failed GC shouldn't kill your watcher. |

---

## Limitations (V1)

- **Linux/WSL2 only** — Uses inotify and Unix symlinks. No Windows NTFS/junction support yet.
- **No cross-filesystem cache** — `node_modules` must be on the same filesystem as `~/.git-warp/cache/` for atomic `rename()`. 
- **First-visit cost** — The first time you visit a branch with a different lockfile, you still need one `npm install`. Every subsequent visit is O(1).
- **Docker volumes are per-compose-file** — If you have multiple compose files, proxy each one separately.

---

## Roadmap

- [ ] `git-warp stash` — Manually stash current `node_modules` into cache after install
- [ ] `git-warp init` — Auto-generate shell aliases (`alias dc='git-warp proxy --'`)
- [ ] Multi-repo support — Watch multiple repos from a single daemon
- [ ] Monorepo support — Detect and cache `node_modules` in workspace subdirectories
- [ ] Cross-filesystem cache — Use `cp -al` (hardlink copy) as fallback when `rename()` fails
- [ ] macOS native support — Already partially supported via `notify` crate's FSEvents backend
- [ ] Daemon auto-start via systemd/launchd unit files

---

## Contributing

Contributions are welcome! Please read [CONTRIBUTING.md](CONTRIBUTING.md) before submitting a pull request.

```bash
# Development workflow
cargo build
cargo test
cargo clippy -- -D warnings
```

---

## License

This project is licensed under the GNU General Public License v3.0 — see the [LICENSE](LICENSE) file for details.
