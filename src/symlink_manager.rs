// ══════════════════════════════════════════════════════════════════════
// git-warp — symlink_manager.rs
// ══════════════════════════════════════════════════════════════════════
//
// O(1) dependency resurrection via content-addressed caching and
// atomic Unix symlinks.
//
// ── THE PROBLEM ─────────────────────────────────────────────────────
//
//   Switching branches often changes the lockfile. Developers then
//   run `npm install` (30-120s) to sync node_modules. Switching back
//   means ANOTHER install. This is O(n) in branch switches.
//
// ── THE SOLUTION ────────────────────────────────────────────────────
//
//   Content-addressed cache at ~/.git-warp/cache/:
//
//     ~/.git-warp/cache/
//       a1b2c3d4e5f6a7b8/        ← SHA-256(package-lock.json)[0..16]
//         node_modules/           ← actual dependency tree
//         .lockfile_hash          ← full SHA for verification
//         .last_used              ← timestamp for GC's LRU eviction
//       f9e8d7c6b5a4e3d2/
//         node_modules/
//         ...
//
//   On branch switch:
//     1. Hash the NEW lockfile → target_hash
//     2. If node_modules is a stale real dir → remove it (we can't
//        cache it — we don't know the OLD lockfile's hash)
//     3. If cache/{target_hash} exists → symlink node_modules → it
//     4. If cache miss → warn user, they need one `npm install`
//
//   To populate the cache correctly, use `stash_after_install` after
//   running `npm install` (lockfile and deps are known to match).
//
//   Result: branch switches are O(1) — a symlink swap, not an install.
//
// ── SUPPORTED LOCKFILES ─────────────────────────────────────────────
//
//   Detected in priority order:
//     1. package-lock.json  (npm)
//     2. yarn.lock          (yarn)
//     3. pnpm-lock.yaml     (pnpm)
//     4. bun.lockb          (bun)
//
// ── ATOMICITY ───────────────────────────────────────────────────────
//
//   True atomic swap (renameat2 RENAME_EXCHANGE) requires both paths
//   to exist and is Linux 3.15+. For maximum WSL2 compat we use:
//     1. Remove old symlink (or stash old real dir)
//     2. Create new symlink
//   The race window is <1ms on local ext4. Acceptable for dev tooling.
//
// ══════════════════════════════════════════════════════════════════════

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

// ──────────────────────────────────────────────────────────────────────
// PUBLIC TYPES
// ──────────────────────────────────────────────────────────────────────

/// Outcome of a dependency swap operation.
#[derive(Debug)]
pub enum SwapResult {
    /// Successfully swapped the symlink to a cached dependency tree.
    /// Contains: (target_hash, cache_path)
    Swapped {
        hash: String,
        #[allow(dead_code)] // Public API — used by future CLI consumers
        cache_path: PathBuf,
    },

    /// The lockfile hash has no cached node_modules yet.
    /// The user must run `npm install` (or equivalent) once.
    CacheMiss {
        hash: String,
        lockfile: String,
    },

    /// No supported lockfile found in the repository.
    NoLockfile,

    /// node_modules already points to the correct cache entry.
    AlreadyCurrent {
        hash: String,
    },

    /// A stale real node_modules directory was removed because we
    /// cannot determine which lockfile it belonged to. The caller
    /// should use `stash_after_install` after `npm install` to
    /// populate the cache correctly.
    #[allow(dead_code)] // Public API — reserved for future verbose swap reporting
    StaleRemoved {
        hash: String,
    },
}

impl std::fmt::Display for SwapResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Swapped { hash, .. } => write!(f, "swapped to cache:{}", &hash[..16]),
            Self::CacheMiss { hash, lockfile } => {
                write!(f, "cache miss for {} (hash:{})", lockfile, &hash[..16])
            }
            Self::NoLockfile => write!(f, "no lockfile found"),
            Self::AlreadyCurrent { hash } => write!(f, "already current ({})", &hash[..16]),
            Self::StaleRemoved { hash } => {
                write!(f, "removed stale node_modules (hash:{})", &hash[..16])
            }
        }
    }
}

/// Lockfile detection result.
#[derive(Debug)]
struct DetectedLockfile {
    /// Filename (e.g. "package-lock.json")
    name: String,
    /// Absolute path to the lockfile
    path: PathBuf,
}

// ──────────────────────────────────────────────────────────────────────
// SUPPORTED LOCKFILES — checked in priority order
// ──────────────────────────────────────────────────────────────────────

const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
];

/// The dependency directory that corresponds to each lockfile.
/// For all JS package managers, this is node_modules.
const DEP_DIR: &str = "node_modules";

// ──────────────────────────────────────────────────────────────────────
// PUBLIC API
// ──────────────────────────────────────────────────────────────────────

/// Perform a dependency swap for the current lockfile state.
///
/// This is the main entry point — call it whenever the branch changes.
///
/// # Arguments
/// * `repo_root` — Absolute path to the git repository root.
/// * `warp_home` — Path to ~/.git-warp.
///
/// # Returns
/// A `SwapResult` describing what happened.
pub fn swap(repo_root: &Path, warp_home: &Path) -> Result<SwapResult> {
    // ── Step 1: Detect lockfile ─────────────────────────────────────
    let lockfile = match detect_lockfile(repo_root) {
        Some(lf) => lf,
        None => {
            tracing::debug!(
                repo = %repo_root.display(),
                "No supported lockfile found — skipping dependency swap"
            );
            return Ok(SwapResult::NoLockfile);
        }
    };

    tracing::debug!(
        lockfile = %lockfile.name,
        path = %lockfile.path.display(),
        "Detected lockfile"
    );

    // ── Step 2: Hash the lockfile ───────────────────────────────────
    let hash = hash_file(&lockfile.path)
        .with_context(|| format!("Failed to hash {}", lockfile.path.display()))?;

    let hash_short = &hash[..16]; // First 16 hex chars for dir name
    let cache_entry = warp_home.join("cache").join(hash_short);
    let cached_deps = cache_entry.join(DEP_DIR);
    let dep_dir = repo_root.join(DEP_DIR);

    tracing::info!(
        lockfile = %lockfile.name,
        hash = %hash_short,
        cache_entry = %cache_entry.display(),
        "Lockfile hashed"
    );

    // ── Step 3: Check if already current ────────────────────────────
    if is_symlink_to(&dep_dir, &cached_deps) {
        tracing::debug!(hash = hash_short, "node_modules already points to correct cache");
        touch_last_used(&cache_entry)?;
        return Ok(SwapResult::AlreadyCurrent {
            hash: hash[..16].to_string(),
        });
    }

    // ── Step 4: Handle existing node_modules ────────────────────────
    if dep_dir.exists() || is_symlink(&dep_dir) {
        if is_symlink(&dep_dir) {
            // It's a symlink (from a previous warp) — just remove it.
            tracing::debug!(
                target = %fs::read_link(&dep_dir)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "unknown".into()),
                "Removing old warp symlink"
            );
            fs::remove_file(&dep_dir).with_context(|| {
                format!("Failed to remove symlink at {}", dep_dir.display())
            })?;
        } else {
            // It's a real directory, but we can't cache it — we only
            // know the NEW lockfile's hash, not the old one that these
            // deps were installed from. Caching under the wrong key
            // would poison the cache. Remove it and report back.
            //
            // Users should call `stash_after_install` right after
            // `npm install` (when lockfile and deps are known to match)
            // to populate the cache correctly.
            tracing::warn!(
                dep_dir = %dep_dir.display(),
                "Removing stale real node_modules (cannot determine original lockfile hash)"
            );
            fs::remove_dir_all(&dep_dir).with_context(|| {
                format!("Failed to remove stale node_modules at {}", dep_dir.display())
            })?;
            // Don't return early — fall through to Step 5 to attempt
            // a cache hit for the new lockfile's hash.
        }
    }

    // ── Step 5: Symlink to cached deps (or report cache miss) ───────
    if cached_deps.is_dir() {
        // Cache HIT — create symlink
        unix_fs::symlink(&cached_deps, &dep_dir).with_context(|| {
            format!(
                "Failed to create symlink {} → {}",
                dep_dir.display(),
                cached_deps.display()
            )
        })?;

        touch_last_used(&cache_entry)?;

        tracing::info!(
            hash = hash_short,
            symlink = %dep_dir.display(),
            target = %cached_deps.display(),
            "Dependency symlink created — O(1) swap complete"
        );

        Ok(SwapResult::Swapped {
            hash: hash[..16].to_string(),
            cache_path: cache_entry,
        })
    } else {
        // Cache MISS — user needs to install deps once
        tracing::warn!(
            hash = hash_short,
            lockfile = %lockfile.name,
            cache_path = %cached_deps.display(),
            "No cached node_modules for this lockfile state — run your package manager"
        );

        Ok(SwapResult::CacheMiss {
            hash: hash[..16].to_string(),
            lockfile: lockfile.name,
        })
    }
}

/// Stash the current REAL node_modules into the cache after an install.
///
/// Call this AFTER `npm install` completes to populate the cache for
/// the current lockfile state. This enables future O(1) swaps.
///
/// # Arguments
/// * `repo_root` — Absolute path to the git repository root.
/// * `warp_home` — Path to ~/.git-warp.
///
/// # Returns
/// The cache hash if stashing succeeded, or an error.
#[allow(dead_code)] // Public API — will be wired to a CLI subcommand
pub fn stash_after_install(repo_root: &Path, warp_home: &Path) -> Result<String> {
    let lockfile = detect_lockfile(repo_root)
        .context("No lockfile found — cannot determine cache key")?;

    let hash = hash_file(&lockfile.path)?;
    let hash_short = &hash[..16];
    let cache_entry = warp_home.join("cache").join(hash_short);
    let cached_deps = cache_entry.join(DEP_DIR);
    let dep_dir = repo_root.join(DEP_DIR);

    if !dep_dir.is_dir() || is_symlink(&dep_dir) {
        anyhow::bail!(
            "node_modules at {} is not a real directory — nothing to stash",
            dep_dir.display()
        );
    }

    if cached_deps.exists() {
        tracing::info!(hash = hash_short, "Cache entry already exists — skipping stash");
        return Ok(hash_short.to_string());
    }

    // Move real dir into cache
    fs::create_dir_all(&cache_entry)?;
    fs::rename(&dep_dir, &cached_deps).with_context(|| {
        format!(
            "Failed to move {} → {}",
            dep_dir.display(),
            cached_deps.display()
        )
    })?;

    // Write metadata
    write_cache_metadata(&cache_entry, &hash, &lockfile.name)?;
    touch_last_used(&cache_entry)?;

    // Create symlink back so the project still works
    unix_fs::symlink(&cached_deps, &dep_dir)?;

    tracing::info!(
        hash = hash_short,
        lockfile = %lockfile.name,
        cache_path = %cache_entry.display(),
        "Stashed node_modules into cache and created symlink"
    );

    Ok(hash_short.to_string())
}

// ──────────────────────────────────────────────────────────────────────
// INTERNAL HELPERS
// ──────────────────────────────────────────────────────────────────────

/// Detect the first supported lockfile in the repo root.
fn detect_lockfile(repo_root: &Path) -> Option<DetectedLockfile> {
    for name in LOCKFILES {
        let path = repo_root.join(name);
        if path.is_file() {
            return Some(DetectedLockfile {
                name: name.to_string(),
                path,
            });
        }
    }
    None
}

/// SHA-256 hash a file's contents. Returns the full 64-char hex string.
fn hash_file(path: &Path) -> Result<String> {
    let data = fs::read(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let mut hasher = Sha256::new();
    hasher.update(&data);
    let digest = hasher.finalize();
    Ok(hex::encode(digest))
}

/// Check if `path` is a symlink (without following it).
fn is_symlink(path: &Path) -> bool {
    // symlink_metadata doesn't follow symlinks, unlike metadata()
    path.symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Check if `path` is a symlink pointing to `target`.
fn is_symlink_to(path: &Path, target: &Path) -> bool {
    if !is_symlink(path) {
        return false;
    }
    match fs::read_link(path) {
        Ok(link_target) => link_target == target,
        Err(_) => false,
    }
}

/// Write metadata files into a cache entry for debugging and GC.
fn write_cache_metadata(cache_entry: &Path, hash: &str, lockfile_name: &str) -> Result<()> {
    // Full hash for integrity verification
    fs::write(cache_entry.join(".lockfile_hash"), hash)?;

    // Which lockfile produced this cache entry
    fs::write(cache_entry.join(".lockfile_name"), lockfile_name)?;

    Ok(())
}

/// Touch the .last_used timestamp in a cache entry.
///
/// The GC uses this file's mtime to determine LRU eviction order.
/// We write the current Unix timestamp as text for human readability.
fn touch_last_used(cache_entry: &Path) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    fs::write(cache_entry.join(".last_used"), &now)
        .with_context(|| format!("Failed to touch .last_used in {}", cache_entry.display()))?;
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// UNIT TESTS
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    /// Helper: set up a fake repo with a lockfile.
    fn setup_repo(lockfile_name: &str, content: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let mut f = fs::File::create(tmp.path().join(lockfile_name)).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        tmp
    }

    /// Helper: set up warp home.
    fn setup_warp_home() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("cache")).unwrap();
        tmp
    }

    // ── Lockfile detection ──────────────────────────────────────────

    #[test]
    fn detect_package_lock() {
        let repo = setup_repo("package-lock.json", "{}");
        let lf = detect_lockfile(repo.path()).unwrap();
        assert_eq!(lf.name, "package-lock.json");
    }

    #[test]
    fn detect_yarn_lock() {
        let repo = setup_repo("yarn.lock", "# yarn lockfile v1");
        let lf = detect_lockfile(repo.path()).unwrap();
        assert_eq!(lf.name, "yarn.lock");
    }

    #[test]
    fn detect_pnpm_lock() {
        let repo = setup_repo("pnpm-lock.yaml", "lockfileVersion: 5.4");
        let lf = detect_lockfile(repo.path()).unwrap();
        assert_eq!(lf.name, "pnpm-lock.yaml");
    }

    #[test]
    fn detect_priority_npm_over_yarn() {
        let repo = setup_repo("package-lock.json", "{}");
        fs::write(repo.path().join("yarn.lock"), "# yarn").unwrap();
        let lf = detect_lockfile(repo.path()).unwrap();
        assert_eq!(lf.name, "package-lock.json");
    }

    #[test]
    fn detect_no_lockfile() {
        let tmp = TempDir::new().unwrap();
        assert!(detect_lockfile(tmp.path()).is_none());
    }

    // ── File hashing ────────────────────────────────────────────────

    #[test]
    fn hash_produces_consistent_results() {
        let repo = setup_repo("package-lock.json", "test content");
        let h1 = hash_file(&repo.path().join("package-lock.json")).unwrap();
        let h2 = hash_file(&repo.path().join("package-lock.json")).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn hash_differs_for_different_content() {
        let r1 = setup_repo("package-lock.json", "content A");
        let r2 = setup_repo("package-lock.json", "content B");
        let h1 = hash_file(&r1.path().join("package-lock.json")).unwrap();
        let h2 = hash_file(&r2.path().join("package-lock.json")).unwrap();
        assert_ne!(h1, h2);
    }

    // ── Symlink helpers ─────────────────────────────────────────────

    #[test]
    fn is_symlink_on_real_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("real_dir");
        fs::create_dir(&dir).unwrap();
        assert!(!is_symlink(&dir));
    }

    #[test]
    fn is_symlink_on_symlink() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = tmp.path().join("link");
        unix_fs::symlink(&target, &link).unwrap();
        assert!(is_symlink(&link));
    }

    #[test]
    fn is_symlink_to_correct_target() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = tmp.path().join("link");
        unix_fs::symlink(&target, &link).unwrap();
        assert!(is_symlink_to(&link, &target));
    }

    #[test]
    fn is_symlink_to_wrong_target() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        fs::create_dir(&target).unwrap();
        let other = tmp.path().join("other");
        fs::create_dir(&other).unwrap();
        let link = tmp.path().join("link");
        unix_fs::symlink(&target, &link).unwrap();
        assert!(!is_symlink_to(&link, &other));
    }

    // ── Full swap flow ──────────────────────────────────────────────

    #[test]
    fn swap_no_lockfile() {
        let repo = TempDir::new().unwrap();
        let warp = setup_warp_home();
        let result = swap(repo.path(), warp.path()).unwrap();
        assert!(matches!(result, SwapResult::NoLockfile));
    }

    #[test]
    fn swap_cache_miss() {
        let repo = setup_repo("package-lock.json", "{\"version\": 1}");
        let warp = setup_warp_home();
        // No node_modules exists, no cache entry → cache miss
        let result = swap(repo.path(), warp.path()).unwrap();
        assert!(matches!(result, SwapResult::CacheMiss { .. }));
    }

    #[test]
    fn swap_removes_stale_real_dir_then_cache_miss() {
        let repo = setup_repo("package-lock.json", "{\"version\": 1}");
        let warp = setup_warp_home();

        // Create a real node_modules (installed for an unknown lockfile)
        let nm = repo.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join("marker.txt"), "hello").unwrap();

        // swap() can't know what lockfile these deps belong to,
        // so it removes the stale dir and reports a cache miss.
        let result = swap(repo.path(), warp.path()).unwrap();
        assert!(matches!(result, SwapResult::CacheMiss { .. }));

        // node_modules should have been removed
        assert!(!nm.exists());
    }

    #[test]
    fn swap_already_current() {
        let repo = setup_repo("package-lock.json", "{\"version\": 1}");
        let warp = setup_warp_home();

        // Use stash_after_install to correctly populate the cache,
        // then swap to create the symlink.
        let nm = repo.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join("marker.txt"), "hello").unwrap();
        stash_after_install(repo.path(), warp.path()).unwrap();

        // Second swap should detect it's already current
        let result = swap(repo.path(), warp.path()).unwrap();
        assert!(matches!(result, SwapResult::AlreadyCurrent { .. }));
    }

    #[test]
    fn swap_switches_between_hashes() {
        let repo = setup_repo("package-lock.json", "{\"version\": 1}");
        let warp = setup_warp_home();

        // Correctly populate cache for v1 via stash_after_install
        let nm = repo.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join("version.txt"), "v1").unwrap();
        stash_after_install(repo.path(), warp.path()).unwrap();

        // Change lockfile to version 2
        fs::write(repo.path().join("package-lock.json"), "{\"version\": 2}").unwrap();
        // This will be a cache miss since we haven't installed v2 deps
        let r2 = swap(repo.path(), warp.path()).unwrap();
        assert!(matches!(r2, SwapResult::CacheMiss { .. }));

        // Switch back to version 1 lockfile
        fs::write(repo.path().join("package-lock.json"), "{\"version\": 1}").unwrap();
        let r3 = swap(repo.path(), warp.path()).unwrap();
        assert!(matches!(r3, SwapResult::Swapped { .. }));

        // Verify the v1 content is back
        assert_eq!(fs::read_to_string(nm.join("version.txt")).unwrap(), "v1");
    }
}
