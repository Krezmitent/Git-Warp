// ══════════════════════════════════════════════════════════════════════
// git-warp — logging.rs
// ══════════════════════════════════════════════════════════════════════
//
// Sets up dual-layer structured logging:
//
//   Layer 1: FILE (JSON)
//     - Machine-parseable structured JSON logs
//     - Written to ~/.git-warp/daemon.log
//     - Rotated daily by tracing-appender
//     - Level: DEBUG (captures everything)
//
//   Layer 2: STDERR (human-readable)
//     - Pretty-printed with ANSI colors for interactive use
//     - Level: INFO (avoids noise for the developer)
//     - Respects $RUST_LOG for overrides
//
// WHY JSON LOGS?
//   When running as a headless daemon, structured logs are the
//   primary debugging interface. JSON lets users pipe daemon.log
//   through `jq` for real-time filtering:
//
//     tail -f ~/.git-warp/daemon.log | jq 'select(.fields.branch != null)'
//
// ══════════════════════════════════════════════════════════════════════

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::OnceLock;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Process-lifetime guard for the non-blocking log writer.
///
/// `tracing-appender`'s `NonBlocking` writer spawns a background thread
/// and returns a `WorkerGuard`. If the guard is dropped, buffered logs
/// are lost. We store it in a `OnceLock` so the guard lives for the
/// entire process lifetime. `OnceLock::set` is a no-op on subsequent
/// calls, preventing an accidental double-init from dropping the guard.
static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Initialize the global tracing subscriber.
///
/// Call this exactly once, early in `main()`, before any tracing macros.
pub fn init(warp_home: &Path) -> Result<()> {
    // ── Layer 1: JSON file logger ───────────────────────────────────
    let log_dir = warp_home.to_path_buf();
    let file_appender = tracing_appender::rolling::daily(&log_dir, "daemon.log");
    let (non_blocking_writer, guard) = tracing_appender::non_blocking(file_appender);

    // Store the guard for the process lifetime; ignore the result if
    // already initialised (e.g. a second call from tests).
    let _ = LOG_GUARD.set(guard);

    let file_layer = fmt::layer()
        .json()
        .with_writer(non_blocking_writer)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true);

    // ── Layer 2: Stderr pretty-printer ──────────────────────────────
    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(true)
        .compact();

    // ── Env filter (respects RUST_LOG) ──────────────────────────────
    // Default: INFO for interactive, DEBUG for file layer.
    // Users can override with: RUST_LOG=git_warp=debug
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("git_warp=info,warn"));

    // ── Compose and install ─────────────────────────────────────────
    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .context("Failed to initialize tracing subscriber (was it already initialized?)")?;

    tracing::debug!(
        log_path = %warp_home.join("daemon.log").display(),
        "Logging initialized — file: JSON/DEBUG, stderr: compact/INFO"
    );

    Ok(())
}
