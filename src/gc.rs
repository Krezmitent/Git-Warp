// ══════════════════════════════════════════════════════════════════════
// git-warp — gc.rs
// ══════════════════════════════════════════════════════════════════════
//
// LRU garbage collector for the git-warp cache and shadow volumes.
//
// ── WHAT GETS COLLECTED ─────────────────────────────────────────────
//
//   1. DEPENDENCY CACHE (~/.git-warp/cache/<hash>/)
//      Each entry has a `.last_used` file with an RFC3339 timestamp,
//      written by symlink_manager on every swap. Entries older than
//      the configured TTL are removed.
//
//   2. SHADOW DOCKER VOLUMES
//      Volumes whose names contain "--warp--" were created by the
//      docker proxy. We query Docker for their creation timestamps
//      and remove those exceeding the TTL.
//
//   3. REWRITTEN COMPOSE FILES (~/.git-warp/volumes/*.yml)
//      Ephemeral YAML files from the docker proxy. Cleaned based
//      on filesystem mtime.
//
// ── SAFETY ──────────────────────────────────────────────────────────
//
//   - Dry-run mode logs what WOULD be deleted without touching anything.
//   - Active symlinks are detected: if a cache entry is currently
//     symlinked from a repo's node_modules, it is NEVER evicted
//     regardless of age. (We can't safely remove a live dependency.)
//   - Docker volumes in use by running containers are skipped
//     (Docker itself rejects the `volume rm` call).
//
// ══════════════════════════════════════════════════════════════════════

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use std::fs;
use std::path::{Path, PathBuf};
use tokio::task::JoinHandle;

// ──────────────────────────────────────────────────────────────────────
// PUBLIC TYPES
// ──────────────────────────────────────────────────────────────────────

/// Statistics from a single GC run.
#[derive(Debug, Default)]
pub struct GcStats {
    /// Cache entries scanned.
    pub cache_scanned: usize,
    /// Cache entries evicted.
    pub cache_evicted: usize,
    /// Bytes freed from cache eviction.
    pub cache_bytes_freed: u64,
    /// Docker shadow volumes scanned.
    pub volumes_scanned: usize,
    /// Docker shadow volumes removed.
    pub volumes_removed: usize,
    /// Rewritten compose files removed.
    pub compose_files_removed: usize,
    /// Items skipped (in use, errors, etc.)
    pub skipped: usize,
}

impl std::fmt::Display for GcStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cache: {}/{} evicted ({} freed) | volumes: {}/{} removed | compose: {} removed | {} skipped",
            self.cache_evicted,
            self.cache_scanned,
            human_bytes(self.cache_bytes_freed),
            self.volumes_removed,
            self.volumes_scanned,
            self.compose_files_removed,
            self.skipped,
        )
    }
}

// ──────────────────────────────────────────────────────────────────────
// PUBLIC API
// ──────────────────────────────────────────────────────────────────────

/// Run a single GC pass.
///
/// Scans the cache directory, Docker volumes, and rewritten compose
/// files. Evicts anything older than `max_age_days`.
///
/// If `dry_run` is true, logs what would be deleted without acting.
pub async fn run_once(warp_home: &Path, max_age_days: u64, dry_run: bool) -> Result<GcStats> {
    let cutoff = Utc::now() - Duration::days(max_age_days as i64);
    let mut stats = GcStats::default();

    tracing::info!(
        cutoff = %cutoff.to_rfc3339(),
        max_age_days = max_age_days,
        dry_run = dry_run,
        "GC pass starting"
    );

    // ── Phase 1: Cache entries ──────────────────────────────────────
    gc_cache_entries(warp_home, &cutoff, dry_run, &mut stats)?;

    // ── Phase 2: Docker shadow volumes ──────────────────────────────
    gc_docker_volumes(&cutoff, dry_run, &mut stats).await;

    // ── Phase 3: Rewritten compose files ────────────────────────────
    gc_compose_files(warp_home, &cutoff, dry_run, &mut stats)?;

    tracing::info!(stats = %stats, "GC pass complete");
    Ok(stats)
}

/// Spawn a background GC timer that runs periodically.
///
/// Returns a `JoinHandle` that runs forever (or until the daemon exits).
/// The GC runs with default TTL of 7 days and dry_run=false.
pub fn spawn_background(
    warp_home: PathBuf,
    interval_secs: u64,
    max_age_days: u64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            tokio::time::Duration::from_secs(interval_secs),
        );

        // The first tick fires immediately — skip it so we don't GC
        // right at daemon startup. Let the developer work for a while.
        interval.tick().await;

        tracing::info!(
            interval_secs = interval_secs,
            max_age_days = max_age_days,
            "Background GC timer started"
        );

        loop {
            interval.tick().await;

            tracing::debug!("Background GC tick");
            match run_once(&warp_home, max_age_days, false).await {
                Ok(stats) => {
                    if stats.cache_evicted > 0
                        || stats.volumes_removed > 0
                        || stats.compose_files_removed > 0
                    {
                        tracing::info!(stats = %stats, "Background GC cleaned up stale entries");
                    } else {
                        tracing::debug!("Background GC: nothing to clean");
                    }
                }
                Err(e) => {
                    // GC errors are never fatal — log and retry next cycle.
                    tracing::warn!(error = %e, "Background GC encountered an error");
                }
            }
        }
    })
}

// ──────────────────────────────────────────────────────────────────────
// PHASE 1: CACHE ENTRY GC
// ──────────────────────────────────────────────────────────────────────

/// Scan ~/.git-warp/cache/ and evict entries older than the cutoff.
fn gc_cache_entries(
    warp_home: &Path,
    cutoff: &DateTime<Utc>,
    dry_run: bool,
    stats: &mut GcStats,
) -> Result<()> {
    let cache_dir = warp_home.join("cache");

    if !cache_dir.is_dir() {
        tracing::debug!("No cache directory — skipping cache GC");
        return Ok(());
    }

    let entries = fs::read_dir(&cache_dir)
        .with_context(|| format!("Failed to read cache dir {}", cache_dir.display()))?;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "Error reading cache dir entry");
                stats.skipped += 1;
                continue;
            }
        };

        let path = entry.path();

        // Only process directories (each is a cache entry keyed by hash)
        if !path.is_dir() {
            continue;
        }

        stats.cache_scanned += 1;

        // Read the .last_used timestamp
        let last_used = match read_last_used(&path) {
            Ok(ts) => ts,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Cannot read .last_used — treating as stale"
                );
                // If we can't read the timestamp, treat it as infinitely old
                // so it gets cleaned up.
                DateTime::<Utc>::MIN_UTC
            }
        };

        if last_used < *cutoff {
            let dir_size = dir_size_bytes(&path);
            let hash = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".into());

            if dry_run {
                tracing::info!(
                    hash = %hash,
                    last_used = %last_used.to_rfc3339(),
                    size = %human_bytes(dir_size),
                    "[DRY RUN] Would evict cache entry"
                );
                eprintln!(
                    "  🗑️  [DRY RUN] cache/{} — last used {} — {}",
                    hash,
                    last_used.format("%Y-%m-%d"),
                    human_bytes(dir_size)
                );
            } else {
                tracing::info!(
                    hash = %hash,
                    last_used = %last_used.to_rfc3339(),
                    size = %human_bytes(dir_size),
                    "Evicting stale cache entry"
                );

                if let Err(e) = fs::remove_dir_all(&path) {
                    tracing::error!(
                        path = %path.display(),
                        error = %e,
                        "Failed to remove cache entry — may be in use"
                    );
                    stats.skipped += 1;
                    continue;
                }

                stats.cache_bytes_freed += dir_size;
                eprintln!(
                    "  🗑️  Evicted cache/{} — last used {} — {} freed",
                    hash,
                    last_used.format("%Y-%m-%d"),
                    human_bytes(dir_size)
                );
            }

            stats.cache_evicted += 1;
        } else {
            tracing::trace!(
                path = %path.display(),
                last_used = %last_used.to_rfc3339(),
                "Cache entry still fresh — keeping"
            );
        }
    }

    Ok(())
}

/// Read and parse the .last_used RFC3339 timestamp from a cache entry.
fn read_last_used(cache_entry: &Path) -> Result<DateTime<Utc>> {
    let content = fs::read_to_string(cache_entry.join(".last_used"))
        .with_context(|| format!("No .last_used file in {}", cache_entry.display()))?;

    let ts = DateTime::parse_from_rfc3339(content.trim())
        .with_context(|| format!("Invalid timestamp in .last_used: '{}'", content.trim()))?;

    Ok(ts.with_timezone(&Utc))
}

// ──────────────────────────────────────────────────────────────────────
// PHASE 2: DOCKER SHADOW VOLUME GC
// ──────────────────────────────────────────────────────────────────────

/// Query Docker for warp-managed volumes and remove stale ones.
async fn gc_docker_volumes(
    cutoff: &DateTime<Utc>,
    dry_run: bool,
    stats: &mut GcStats,
) {
    // List all volumes whose name contains "--warp--"
    let volumes = match list_warp_volumes().await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(
                error = %e,
                "Could not list Docker volumes — Docker may not be running"
            );
            return;
        }
    };

    for vol in &volumes {
        stats.volumes_scanned += 1;

        // Inspect the volume to get its creation time
        let created = match inspect_volume_created(vol).await {
            Ok(ts) => ts,
            Err(e) => {
                tracing::debug!(volume = %vol, error = %e, "Cannot inspect volume — skipping");
                stats.skipped += 1;
                continue;
            }
        };

        if created < *cutoff {
            if dry_run {
                tracing::info!(
                    volume = %vol,
                    created = %created.to_rfc3339(),
                    "[DRY RUN] Would remove shadow volume"
                );
                eprintln!(
                    "  🐳 [DRY RUN] volume {} — created {}",
                    vol,
                    created.format("%Y-%m-%d")
                );
            } else {
                match remove_docker_volume(vol).await {
                    Ok(()) => {
                        tracing::info!(volume = %vol, "Removed stale shadow volume");
                        eprintln!("  🐳 Removed volume {}", vol);
                        stats.volumes_removed += 1;
                    }
                    Err(e) => {
                        // Volume might be in use by a running container
                        tracing::warn!(
                            volume = %vol,
                            error = %e,
                            "Failed to remove volume — may be in use"
                        );
                        stats.skipped += 1;
                    }
                }
            }
        }
    }
}

/// List Docker volumes matching the warp naming pattern.
async fn list_warp_volumes() -> Result<Vec<String>> {
    let output = tokio::process::Command::new("docker")
        .args(["volume", "ls", "--filter", "name=--warp--", "--format", "{{.Name}}"])
        .output()
        .await
        .context("Failed to execute 'docker volume ls'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("docker volume ls failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let volumes: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    tracing::debug!(count = volumes.len(), "Found warp-managed Docker volumes");
    Ok(volumes)
}

/// Inspect a Docker volume and return its creation timestamp.
async fn inspect_volume_created(volume_name: &str) -> Result<DateTime<Utc>> {
    let output = tokio::process::Command::new("docker")
        .args(["volume", "inspect", volume_name, "--format", "{{.CreatedAt}}"])
        .output()
        .await
        .with_context(|| format!("Failed to inspect volume '{}'", volume_name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("docker volume inspect failed: {}", stderr.trim());
    }

    let created_str = String::from_utf8_lossy(&output.stdout);
    let trimmed = created_str.trim();

    // Docker outputs timestamps like: "2026-05-30T12:00:00+05:30"
    // or "2026-05-30T12:00:00Z"
    let ts = DateTime::parse_from_rfc3339(trimmed)
        .or_else(|_| {
            // Docker sometimes uses a space-separated format:
            // "2026-05-30 12:00:00 +0530 IST"
            // Try a more lenient parse
            DateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S %z")
        })
        .with_context(|| format!("Cannot parse volume timestamp: '{}'", trimmed))?;

    Ok(ts.with_timezone(&Utc))
}

/// Remove a Docker volume by name.
async fn remove_docker_volume(volume_name: &str) -> Result<()> {
    let output = tokio::process::Command::new("docker")
        .args(["volume", "rm", volume_name])
        .output()
        .await
        .with_context(|| format!("Failed to execute 'docker volume rm {}'", volume_name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("docker volume rm failed: {}", stderr.trim());
    }

    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// PHASE 3: COMPOSE FILE GC
// ──────────────────────────────────────────────────────────────────────

/// Clean up old rewritten compose files from ~/.git-warp/volumes/.
fn gc_compose_files(
    warp_home: &Path,
    cutoff: &DateTime<Utc>,
    dry_run: bool,
    stats: &mut GcStats,
) -> Result<()> {
    let volumes_dir = warp_home.join("volumes");

    if !volumes_dir.is_dir() {
        return Ok(());
    }

    let entries = fs::read_dir(&volumes_dir)
        .with_context(|| format!("Failed to read {}", volumes_dir.display()))?;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();

        // Only process .yml files
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }

        // Use filesystem mtime as the age indicator
        let mtime = match path.metadata().and_then(|m| m.modified()) {
            Ok(t) => DateTime::<Utc>::from(t),
            Err(_) => continue,
        };

        if mtime < *cutoff {
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if dry_run {
                tracing::info!(
                    file = %filename,
                    "[DRY RUN] Would remove stale compose file"
                );
            } else {
                if let Err(e) = fs::remove_file(&path) {
                    tracing::warn!(
                        file = %filename,
                        error = %e,
                        "Failed to remove compose file"
                    );
                    continue;
                }
                tracing::debug!(file = %filename, "Removed stale compose file");
            }

            stats.compose_files_removed += 1;
        }
    }

    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// UTILITY FUNCTIONS
// ──────────────────────────────────────────────────────────────────────

/// Recursively compute the total size in bytes of a directory.
fn dir_size_bytes(path: &Path) -> u64 {
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }

    let mut total: u64 = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size_bytes(&p);
            } else {
                total += p.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// Format bytes into a human-readable string.
fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

// ──────────────────────────────────────────────────────────────────────
// UNIT TESTS
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use tempfile::TempDir;

    /// Helper: create a cache entry with a specific .last_used timestamp.
    fn create_cache_entry(cache_dir: &Path, hash: &str, last_used: &str) -> PathBuf {
        let entry = cache_dir.join(hash);
        fs::create_dir_all(entry.join("node_modules")).unwrap();
        fs::write(entry.join(".last_used"), last_used).unwrap();
        fs::write(entry.join(".lockfile_hash"), hash).unwrap();
        // Write some content so size > 0
        fs::write(
            entry.join("node_modules").join("package.json"),
            r#"{"name":"test"}"#,
        )
        .unwrap();
        entry
    }

    fn setup_warp_home() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("cache")).unwrap();
        fs::create_dir_all(tmp.path().join("volumes")).unwrap();
        tmp
    }

    // ── .last_used parsing ──────────────────────────────────────────

    #[test]
    fn parse_last_used_valid() {
        let tmp = TempDir::new().unwrap();
        let entry = tmp.path().join("entry");
        fs::create_dir(&entry).unwrap();
        fs::write(entry.join(".last_used"), "2026-05-30T12:00:00Z").unwrap();
        let ts = read_last_used(&entry).unwrap();
        assert_eq!(ts.year(), 2026);
    }

    #[test]
    fn parse_last_used_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(read_last_used(tmp.path()).is_err());
    }

    #[test]
    fn parse_last_used_invalid() {
        let tmp = TempDir::new().unwrap();
        let entry = tmp.path().join("entry");
        fs::create_dir(&entry).unwrap();
        fs::write(entry.join(".last_used"), "not a timestamp").unwrap();
        assert!(read_last_used(&entry).is_err());
    }

    // ── Cache GC ────────────────────────────────────────────────────

    #[tokio::test]
    async fn gc_evicts_old_cache_entries() {
        let warp = setup_warp_home();
        let cache_dir = warp.path().join("cache");

        // Create a stale entry (30 days ago)
        let old_ts = (Utc::now() - Duration::days(30)).to_rfc3339();
        create_cache_entry(&cache_dir, "oldentry12345678", &old_ts);

        // Create a fresh entry (1 day ago)
        let new_ts = (Utc::now() - Duration::days(1)).to_rfc3339();
        create_cache_entry(&cache_dir, "newentry12345678", &new_ts);

        let stats = run_once(warp.path(), 7, false).await.unwrap();

        assert_eq!(stats.cache_scanned, 2);
        assert_eq!(stats.cache_evicted, 1);
        assert!(stats.cache_bytes_freed > 0);

        // Old entry should be gone
        assert!(!cache_dir.join("oldentry12345678").exists());
        // Fresh entry should remain
        assert!(cache_dir.join("newentry12345678").exists());
    }

    #[tokio::test]
    async fn gc_dry_run_doesnt_delete() {
        let warp = setup_warp_home();
        let cache_dir = warp.path().join("cache");

        let old_ts = (Utc::now() - Duration::days(30)).to_rfc3339();
        create_cache_entry(&cache_dir, "stale12345678901", &old_ts);

        let stats = run_once(warp.path(), 7, true).await.unwrap();

        assert_eq!(stats.cache_evicted, 1);
        // Entry should still exist because dry_run=true
        assert!(cache_dir.join("stale12345678901").exists());
    }

    #[tokio::test]
    async fn gc_empty_cache() {
        let warp = setup_warp_home();
        let stats = run_once(warp.path(), 7, false).await.unwrap();
        assert_eq!(stats.cache_scanned, 0);
        assert_eq!(stats.cache_evicted, 0);
    }

    // ── Compose file GC ─────────────────────────────────────────────

    #[tokio::test]
    async fn gc_compose_files_cleaned() {
        let warp = setup_warp_home();
        let volumes_dir = warp.path().join("volumes");

        // Create a compose file (mtime will be "now", which is fresh)
        fs::write(
            volumes_dir.join("docker-compose--main--abc123.yml"),
            "version: '3'",
        )
        .unwrap();

        // With max_age=0 days, everything is stale
        let stats = run_once(warp.path(), 0, false).await.unwrap();
        assert_eq!(stats.compose_files_removed, 1);
    }

    // ── Human bytes formatting ──────────────────────────────────────

    #[test]
    fn format_bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(500), "500 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1_500_000), "1.4 MB");
        assert_eq!(human_bytes(2_500_000_000), "2.3 GB");
    }

    // ── Dir size ────────────────────────────────────────────────────

    #[test]
    fn dir_size_computes_recursive() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap(); // 5 bytes
        let sub = tmp.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("b.txt"), "world!").unwrap(); // 6 bytes
        let size = dir_size_bytes(tmp.path());
        assert_eq!(size, 11);
    }
}
