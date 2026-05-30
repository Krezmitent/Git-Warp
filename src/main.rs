// ══════════════════════════════════════════════════════════════════════
// git-warp — main.rs
// ══════════════════════════════════════════════════════════════════════
//
// Entry point for the git-warp daemon & CLI.
//
// Architecture overview:
// ┌──────────────────────────────────────────────────────────┐
// │  CLI (clap)                                              │
// │  ├─ start-daemon   → spawn watcher + GC in background   │
// │  ├─ proxy          → intercept docker-compose invocation │
// │  ├─ gc             → manual garbage collection trigger   │
// │  └─ status         → report daemon health + cache stats  │
// └──────────────────────────────────────────────────────────┘
//
// Every subcommand funnels through a single async runtime
// (tokio multi-thread) to keep resource usage predictable.
//
// LOGGING STRATEGY:
//   All daemon activity is logged to ~/.git-warp/daemon.log via
//   tracing-appender, with a fallback to stderr if the log dir
//   can't be created. Structured JSON logs for machine parsing,
//   human-readable pretty-print on stderr for interactive use.
// ══════════════════════════════════════════════════════════════════════

mod docker_proxy;
mod gc;
mod logging;
mod symlink_manager;
mod watcher;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

// ──────────────────────────────────────────────────────────────────────
// CLI DEFINITION
// ──────────────────────────────────────────────────────────────────────

/// git-warp: An Environment Time Machine.
///
/// Zero-overhead daemon that shadows Docker volumes per-branch,
/// O(1) swaps node_modules via symlinks, and watches Git branch
/// changes in real time using inotify.
#[derive(Parser, Debug)]
#[command(
    name = "git-warp",
    version,
    about = "Environment Time Machine — instant branch-aware dev environments",
    long_about = None,
    propagate_version = true
)]
struct Cli {
    /// Path to the Git repository root to manage.
    /// Defaults to the current working directory.
    #[arg(short, long, global = true, default_value = ".")]
    repo: PathBuf,

    /// Override the cache directory (default: ~/.git-warp).
    #[arg(long, global = true, env = "GIT_WARP_HOME")]
    warp_home: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the git-warp daemon in the foreground.
    ///
    /// Watches .git/HEAD for branch changes (via inotify on Linux)
    /// and triggers dependency + volume swaps automatically.
    /// Also spawns the GC background timer.
    #[command(name = "start-daemon", alias = "daemon")]
    StartDaemon {
        /// Interval (in seconds) between GC sweeps.
        /// Default: 3600 (1 hour).
        #[arg(long, default_value_t = 3600)]
        gc_interval: u64,
    },

    /// Proxy a docker-compose command through git-warp.
    ///
    /// Rewrites volume mappings in the compose file to be
    /// branch-specific, then delegates to the real docker-compose.
    ///
    /// Usage: git-warp proxy -- up -d
    #[command(name = "proxy")]
    Proxy {
        /// Path to the docker-compose.yml file.
        /// Defaults to ./docker-compose.yml in the repo root.
        #[arg(short = 'f', long, default_value = "docker-compose.yml")]
        compose_file: PathBuf,

        /// Arguments to forward to docker-compose after rewriting.
        /// Everything after `--` is passed through verbatim.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Run garbage collection manually.
    ///
    /// Purges cached node_modules and shadow Docker volumes
    /// that haven't been accessed in the configured TTL.
    #[command(name = "gc")]
    Gc {
        /// Maximum age (in days) before a cache entry is evicted.
        /// Default: 7 days.
        #[arg(long, default_value_t = 7)]
        max_age_days: u64,

        /// Perform a dry run — log what would be deleted without
        /// actually removing anything.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },

    /// Show daemon status, cache statistics, and watched repos.
    #[command(name = "status")]
    Status,
}

// ──────────────────────────────────────────────────────────────────────
// WARP HOME RESOLUTION
// ──────────────────────────────────────────────────────────────────────

/// Resolves the git-warp home directory.
///
/// Priority:
///   1. Explicit --warp-home CLI flag
///   2. $GIT_WARP_HOME env var (handled by clap's `env` attr)
///   3. ~/.git-warp (via the `dirs` crate)
///
/// Creates the directory tree if it doesn't exist.
fn resolve_warp_home(override_path: Option<PathBuf>) -> Result<PathBuf> {
    let home = match override_path {
        Some(p) => p,
        None => {
            let user_home = dirs::home_dir()
                .context("Could not determine user home directory. Set $GIT_WARP_HOME explicitly.")?;
            user_home.join(".git-warp")
        }
    };

    // Ensure directory tree exists: ~/.git-warp/cache/
    std::fs::create_dir_all(home.join("cache"))
        .with_context(|| format!("Failed to create cache directory at {}", home.display()))?;

    // Ensure directory tree exists: ~/.git-warp/volumes/
    std::fs::create_dir_all(home.join("volumes"))
        .with_context(|| format!("Failed to create volumes directory at {}", home.display()))?;

    tracing::info!(path = %home.display(), "Resolved git-warp home directory");
    Ok(home)
}

// ──────────────────────────────────────────────────────────────────────
// ASYNC ENTRY POINT
// ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Resolve warp home early — logging setup needs it for the log file path.
    let warp_home = resolve_warp_home(cli.warp_home.clone())?;

    // Initialize structured logging (file + stderr).
    logging::init(&warp_home)?;

    // Canonicalize the repo path so all downstream modules
    // work with absolute paths. This also validates the path exists.
    let repo_root = std::fs::canonicalize(&cli.repo).with_context(|| {
        format!(
            "Repository path '{}' does not exist or is inaccessible",
            cli.repo.display()
        )
    })?;

    tracing::info!(
        repo = %repo_root.display(),
        warp_home = %warp_home.display(),
        version = env!("CARGO_PKG_VERSION"),
        "git-warp initializing"
    );

    // ── Dispatch to the appropriate subcommand ──────────────────────
    match cli.command {
        Commands::StartDaemon { gc_interval } => {
            tracing::info!(gc_interval_secs = gc_interval, "Starting daemon");

            // Spawn the background GC timer
            let _gc_handle = gc::spawn_background(
                warp_home.clone(),
                gc_interval,
                7, // default max age: 7 days
            );

            eprintln!(
                "🚀 git-warp daemon starting for repo: {}",
                repo_root.display()
            );
            eprintln!("   GC interval: {}s", gc_interval);
            eprintln!("   Cache dir:   {}", warp_home.join("cache").display());
            eprintln!("   Log file:    {}", warp_home.join("daemon.log").display());
            eprintln!();

            // ── Branch-change callback ──────────────────────────────
            // This closure is invoked every time HEAD transitions to
            // a different branch. Steps 3 & 4 will hook in here.
            let warp_home_cb = warp_home.clone();
            let repo_root_cb = repo_root.clone();
            let on_change: watcher::OnBranchChange = Box::new(move |prev, new| {
                let prev_str = prev.map(|p| p.to_string()).unwrap_or_else(|| "none".into());
                tracing::info!(
                    from = %prev_str,
                    to = %new,
                    repo = %repo_root_cb.display(),
                    warp_home = %warp_home_cb.display(),
                    "Branch transition — triggering environment swap"
                );
                eprintln!("⚡ Branch changed: {} → {}", prev_str, new);

                // ── O(1) dependency swap ────────────────────────────
                match symlink_manager::swap(&repo_root_cb, &warp_home_cb) {
                    Ok(result) => {
                        tracing::info!(result = %result, "Dependency swap outcome");
                        eprintln!("   📦 Dependencies: {}", result);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Dependency swap failed");
                        eprintln!("   ⚠️  Dependency swap error: {}", e);
                    }
                }

                // TODO(step3): docker_proxy volume shadowing is manual via `proxy` command

                Ok(())
            });

            // Blocks forever — this IS the daemon.
            watcher::run(&repo_root, on_change).await?;
        }

        Commands::Proxy { compose_file, args } => {
            tracing::info!(
                compose_file = %compose_file.display(),
                args = ?args,
                "Proxy command invoked"
            );

            // Detect the current branch to scope volumes.
            let head_path = repo_root.join(".git").join("HEAD");
            let head_content = std::fs::read_to_string(&head_path)
                .with_context(|| format!("Cannot read .git/HEAD at {}", head_path.display()))?;
            let branch = head_content
                .trim()
                .strip_prefix("ref: refs/heads/")
                .unwrap_or("detached")
                .to_string();

            eprintln!("🐳 docker-compose proxy mode");
            eprintln!("   Branch:       {}", branch);
            eprintln!("   Compose file: {}", compose_file.display());
            eprintln!("   Args:         {:?}", args);

            let exit_code = docker_proxy::run(
                &repo_root,
                &compose_file,
                &branch,
                &args,
                &warp_home,
            )
            .await?;

            std::process::exit(exit_code);
        }

        Commands::Gc {
            max_age_days,
            dry_run,
        } => {
            tracing::info!(
                max_age_days = max_age_days,
                dry_run = dry_run,
                "Manual GC triggered"
            );
            eprintln!("🧹 Garbage collection");
            eprintln!("   Max age: {} days", max_age_days);
            eprintln!("   Dry run: {}", dry_run);
            eprintln!();

            let stats = gc::run_once(&warp_home, max_age_days, dry_run).await?;
            eprintln!("\n✅ GC complete: {}", stats);
        }

        Commands::Status => {
            tracing::info!("Status query");
            eprintln!("📊 git-warp status");
            eprintln!("   Warp home: {}", warp_home.display());
            eprintln!("   Repo root: {}", repo_root.display());

            // Show cache statistics
            let cache_dir = warp_home.join("cache");
            let cache_entries = std::fs::read_dir(&cache_dir)
                .map(|rd| rd.count())
                .unwrap_or(0);
            eprintln!("   Cached dependency snapshots: {}", cache_entries);

            let volumes_dir = warp_home.join("volumes");
            let volume_entries = std::fs::read_dir(&volumes_dir)
                .map(|rd| rd.count())
                .unwrap_or(0);
            eprintln!("   Shadow volume configs: {}", volume_entries);
        }
    }

    Ok(())
}
