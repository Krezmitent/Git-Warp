// ══════════════════════════════════════════════════════════════════════
// git-warp — watcher.rs
// ══════════════════════════════════════════════════════════════════════
//
// Event-driven Git branch watcher using the `notify` crate, which
// delegates to inotify(7) on Linux/WSL2. Zero polling. Reacts within
// microseconds of a branch change.
//
// ── HOW GIT BRANCH SWITCHING WORKS AT THE FS LEVEL ──────────────────
//
//   .git/HEAD contains either:
//     1. "ref: refs/heads/<branch>\n"   (normal attached HEAD)
//     2. "<40-hex-char SHA>\n"          (detached HEAD)
//
//   `git checkout <branch>` rewrites .git/HEAD atomically:
//     1. Writes to .git/HEAD.lock  (temp file)
//     2. Renames .git/HEAD.lock → .git/HEAD
//
//   Because of this atomic-rename pattern, we CANNOT watch .git/HEAD
//   directly — the inode gets replaced. Instead we watch the .git/
//   directory itself (non-recursively) and filter for events whose
//   path ends in "HEAD".
//
// ── DEBOUNCING ──────────────────────────────────────────────────────
//
//   A single `git checkout` can fire 2-4 FS events in rapid
//   succession (create HEAD.lock, modify, rename, remove lock).
//   We debounce with a 50ms window: after the first event, we wait
//   50ms for more events before reading the file. This collapses
//   the burst into a single branch-change callback.
//
// ══════════════════════════════════════════════════════════════════════

use anyhow::{Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

// ──────────────────────────────────────────────────────────────────────
// PUBLIC API
// ──────────────────────────────────────────────────────────────────────

/// The resolved state of .git/HEAD after a change event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadState {
    /// HEAD points to a named branch (e.g. "main", "feature/login").
    Branch(String),
    /// HEAD is detached at a specific commit SHA.
    Detached(String),
}

impl std::fmt::Display for HeadState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeadState::Branch(name) => write!(f, "branch:{}", name),
            HeadState::Detached(sha) => write!(f, "detached:{}", &sha[..8.min(sha.len())]),
        }
    }
}

/// Callback type invoked when the active branch changes.
///
/// Receives the previous state (if any) and the new state.
/// Returning Err from the callback logs the error but does NOT
/// kill the watcher — resilience over correctness for a daemon.
pub type OnBranchChange =
    Box<dyn Fn(Option<&HeadState>, &HeadState) -> Result<()> + Send + Sync + 'static>;

/// Run the watcher loop. This function blocks (async) forever,
/// monitoring .git/HEAD for changes and invoking `on_change`
/// whenever the branch changes.
///
/// # Arguments
/// * `repo_root` — Absolute path to the Git repository root.
/// * `on_change` — Callback fired on every branch transition.
///
/// # Errors
/// Returns `Err` only on fatal setup failures (e.g. .git/ doesn't
/// exist, inotify fd exhaustion). Runtime errors in the callback
/// are logged and swallowed.
pub async fn run(repo_root: &Path, on_change: OnBranchChange) -> Result<()> {
    let git_dir = repo_root.join(".git");

    // ── Validate that this is actually a git repo ───────────────────
    if !git_dir.is_dir() {
        anyhow::bail!(
            "No .git directory found at {}. Is this a git repository?",
            git_dir.display()
        );
    }

    let head_path = git_dir.join("HEAD");
    if !head_path.exists() {
        anyhow::bail!(
            ".git/HEAD not found at {}. Repository may be corrupt.",
            head_path.display()
        );
    }

    // ── Read initial state ──────────────────────────────────────────
    let mut current_state = parse_head(&head_path)
        .context("Failed to read initial .git/HEAD state")?;

    tracing::info!(
        initial_state = %current_state,
        head_path = %head_path.display(),
        "Watcher initialized — monitoring .git/HEAD"
    );

    // ── Set up the notify → tokio bridge channel ────────────────────
    //
    // The `notify` crate invokes its callback on an internal thread.
    // We bridge into the tokio world with an mpsc channel. The
    // channel is bounded (64) to apply backpressure if the event
    // loop falls behind — this should never happen in practice.
    let (tx, mut rx) = mpsc::channel::<Event>(64);

    // ── Create the inotify-backed watcher ───────────────────────────
    let mut watcher = RecommendedWatcher::new(
        move |result: std::result::Result<Event, notify::Error>| {
            match result {
                Ok(event) => {
                    // Only forward events that touch HEAD or HEAD.lock.
                    // This is our first-pass filter before the async loop
                    // does the real debounce + parse.
                    if event_touches_head(&event) {
                        // If the channel is full, drop the event — the
                        // debounce logic will still pick up the final
                        // state from disk.
                        let _ = tx.try_send(event);
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "File watcher error");
                }
            }
        },
        notify::Config::default(),
    )
    .context("Failed to create file system watcher (inotify)")?;

    // Watch the .git/ directory (NOT .git/HEAD directly).
    // See module-level docs for why.
    watcher
        .watch(&git_dir, RecursiveMode::NonRecursive)
        .with_context(|| {
            format!(
                "Failed to watch .git directory at {}",
                git_dir.display()
            )
        })?;

    tracing::debug!(
        watched_path = %git_dir.display(),
        "inotify watch registered on .git/"
    );

    // ── Event loop with debouncing ──────────────────────────────────
    //
    // Strategy:
    //   1. Block on the channel for the first event.
    //   2. Once an event arrives, start a 50ms debounce window.
    //   3. Drain any additional events that arrive within the window.
    //   4. After the window closes, read .git/HEAD once and compare
    //      to the last known state.
    //   5. If changed → fire callback. If same → no-op.
    //   6. Go back to step 1.

    const DEBOUNCE_MS: u64 = 50;

    loop {
        // Step 1: Block until the first event arrives.
        match rx.recv().await {
            Some(_first_event) => {
                // Step 2-3: Debounce — drain the burst.
                let deadline = Instant::now() + Duration::from_millis(DEBOUNCE_MS);
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    match tokio::time::timeout(remaining, rx.recv()).await {
                        Ok(Some(_)) => continue, // More events in the burst — keep draining.
                        Ok(None) => {
                            // Channel closed — watcher was dropped.
                            tracing::warn!("Watcher channel closed unexpectedly");
                            return Ok(());
                        }
                        Err(_) => break, // Timeout — debounce window expired.
                    }
                }

                // Step 4: Read the final state after the burst settles.
                match parse_head(&head_path) {
                    Ok(new_state) => {
                        // Step 5: Fire callback only on actual transitions.
                        if new_state != current_state {
                            tracing::info!(
                                from = %current_state,
                                to = %new_state,
                                "Branch change detected"
                            );

                            // Invoke the user-supplied callback.
                            // Errors are logged but do NOT kill the daemon.
                            if let Err(e) = on_change(Some(&current_state), &new_state) {
                                tracing::error!(
                                    error = %e,
                                    from = %current_state,
                                    to = %new_state,
                                    "Branch change callback failed"
                                );
                            }

                            current_state = new_state;
                        } else {
                            tracing::trace!(
                                state = %current_state,
                                "HEAD event but no branch change (e.g. amend, rebase)"
                            );
                        }
                    }
                    Err(e) => {
                        // .git/HEAD might be momentarily missing during
                        // a rebase or gc. Log and retry on next event.
                        tracing::warn!(
                            error = %e,
                            "Failed to parse .git/HEAD — will retry on next event"
                        );
                    }
                }
            }
            None => {
                // Channel closed — clean shutdown.
                tracing::info!("Watcher channel closed — shutting down");
                return Ok(());
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// INTERNAL HELPERS
// ──────────────────────────────────────────────────────────────────────

/// Parse .git/HEAD and return the current branch or detached SHA.
///
/// File format:
///   - "ref: refs/heads/main\n"  → Branch("main")
///   - "abc123def456...\n"       → Detached("abc123def456...")
///
/// Handles edge cases:
///   - Trailing whitespace / newlines
///   - Nested branch names like "feature/auth/oauth2"
///   - Empty or malformed files → returns Err
fn parse_head(head_path: &Path) -> Result<HeadState> {
    let content = std::fs::read_to_string(head_path)
        .with_context(|| format!("Could not read {}", head_path.display()))?;

    let trimmed = content.trim();

    if trimmed.is_empty() {
        anyhow::bail!(".git/HEAD is empty");
    }

    if let Some(ref_target) = trimmed.strip_prefix("ref: ") {
        // Normal branch reference: "ref: refs/heads/feature/login"
        //                                 ^^^^^^^^^^^
        //                                 strip this prefix
        let branch_name = ref_target
            .strip_prefix("refs/heads/")
            .unwrap_or(ref_target); // Fallback: keep full ref if not under heads/

        if branch_name.is_empty() {
            anyhow::bail!(".git/HEAD contains empty ref: '{}'", trimmed);
        }

        tracing::trace!(branch = branch_name, "Parsed HEAD as branch ref");
        Ok(HeadState::Branch(branch_name.to_string()))
    } else if trimmed.len() >= 40 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        // Detached HEAD: raw 40-character (SHA-1) or 64-character (SHA-256) hex
        tracing::trace!(sha = &trimmed[..8], "Parsed HEAD as detached SHA");
        Ok(HeadState::Detached(trimmed.to_string()))
    } else {
        anyhow::bail!(
            ".git/HEAD contains unrecognized format: '{}'",
            trimmed
        )
    }
}

/// Check if a notify event's paths include HEAD or HEAD.lock.
///
/// We only care about events that touch these two files. Everything
/// else in .git/ (index, refs, objects) is irrelevant noise.
fn event_touches_head(event: &Event) -> bool {
    event.paths.iter().any(|p| {
        p.file_name()
            .map(|name| {
                let n = name.to_string_lossy();
                n == "HEAD" || n == "HEAD.lock"
            })
            .unwrap_or(false)
    })
}

// ──────────────────────────────────────────────────────────────────────
// UNIT TESTS
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use notify::EventKind;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Helper: create a fake .git/HEAD file with the given content.
    fn write_head(dir: &Path, content: &str) -> PathBuf {
        let git_dir = dir.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let head = git_dir.join("HEAD");
        let mut f = std::fs::File::create(&head).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        head
    }

    #[test]
    fn parse_branch_ref() {
        let tmp = TempDir::new().unwrap();
        let head = write_head(tmp.path(), "ref: refs/heads/main\n");
        let state = parse_head(&head).unwrap();
        assert_eq!(state, HeadState::Branch("main".into()));
    }

    #[test]
    fn parse_nested_branch() {
        let tmp = TempDir::new().unwrap();
        let head = write_head(tmp.path(), "ref: refs/heads/feature/auth/oauth2\n");
        let state = parse_head(&head).unwrap();
        assert_eq!(state, HeadState::Branch("feature/auth/oauth2".into()));
    }

    #[test]
    fn parse_detached_sha1() {
        let tmp = TempDir::new().unwrap();
        let sha = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let head = write_head(tmp.path(), &format!("{}\n", sha));
        let state = parse_head(&head).unwrap();
        assert_eq!(state, HeadState::Detached(sha.into()));
    }

    #[test]
    fn parse_empty_head_fails() {
        let tmp = TempDir::new().unwrap();
        let head = write_head(tmp.path(), "");
        assert!(parse_head(&head).is_err());
    }

    #[test]
    fn parse_garbage_fails() {
        let tmp = TempDir::new().unwrap();
        let head = write_head(tmp.path(), "not a valid ref\n");
        assert!(parse_head(&head).is_err());
    }

    #[test]
    fn display_branch_state() {
        let state = HeadState::Branch("develop".into());
        assert_eq!(format!("{}", state), "branch:develop");
    }

    #[test]
    fn display_detached_state() {
        let sha = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let state = HeadState::Detached(sha.into());
        assert_eq!(format!("{}", state), "detached:a1b2c3d4");
    }

    #[test]
    fn event_filter_matches_head() {
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![PathBuf::from("/repo/.git/HEAD")],
            attrs: Default::default(),
        };
        assert!(event_touches_head(&event));
    }

    #[test]
    fn event_filter_matches_head_lock() {
        let event = Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![PathBuf::from("/repo/.git/HEAD.lock")],
            attrs: Default::default(),
        };
        assert!(event_touches_head(&event));
    }

    #[test]
    fn event_filter_ignores_index() {
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![PathBuf::from("/repo/.git/index")],
            attrs: Default::default(),
        };
        assert!(!event_touches_head(&event));
    }
}
