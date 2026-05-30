// ══════════════════════════════════════════════════════════════════════
// git-warp — docker_proxy.rs
// ══════════════════════════════════════════════════════════════════════
//
// Intercepts docker-compose invocations to make database volumes
// branch-aware. Given a compose file and a branch name, this module:
//
//   1. Parses the YAML into a mutable value tree
//   2. Rewrites volume mappings to be branch-scoped:
//      - Bind mounts:  ./pgdata → ./pgdata--warp--feature-login
//      - Named volumes: db_data → db_data--warp--feature-login
//   3. Writes the modified YAML to a temp file
//   4. Delegates to the real `docker-compose` with -f <temp_file>
//
// ── VOLUME TYPES IN DOCKER COMPOSE ──────────────────────────────────
//
//   Short-form bind mount:   "./host/path:/container/path[:mode]"
//     → Host path starts with ./ or / or ~/
//     → We append "--warp--{branch}" to the host portion
//
//   Short-form named volume: "volume_name:/container/path[:mode]"
//     → No path separator in the host portion
//     → We append "--warp--{branch}" to the volume name
//
//   Long-form (object):
//     type: bind|volume
//     source: <host_path_or_volume_name>
//     target: <container_path>
//     → We rewrite `source` based on `type`
//
//   Anonymous volumes:       "/container/path"
//     → No host portion — left untouched (Docker manages these)
//
// ── BRANCH NAME SANITIZATION ────────────────────────────────────────
//
//   Git branch names can contain characters illegal in Docker volume
//   names and filesystem paths: / ? * etc.
//   We sanitize: feature/login → feature-login
//   Delimiter "--warp--" prevents collisions with user-named volumes.
//
// ══════════════════════════════════════════════════════════════════════

use anyhow::{Context, Result};
use serde_yaml::Value;
use std::path::Path;
use std::process::Stdio;

// ──────────────────────────────────────────────────────────────────────
// PUBLIC API
// ──────────────────────────────────────────────────────────────────────

/// Proxy a docker-compose invocation with branch-scoped volumes.
///
/// # Arguments
/// * `repo_root`    — Absolute path to the git repository root.
/// * `compose_file` — Path to the docker-compose.yml (relative or absolute).
/// * `branch`       — Current git branch name (will be sanitized).
/// * `args`         — Arguments to forward to docker-compose (e.g. ["up", "-d"]).
/// * `warp_home`    — Path to ~/.git-warp for storing rewritten compose files.
///
/// # Returns
/// The exit code from docker-compose, or an error if we couldn't
/// parse/rewrite the compose file or launch the process.
pub async fn run(
    repo_root: &Path,
    compose_file: &Path,
    branch: &str,
    args: &[String],
    warp_home: &Path,
) -> Result<i32> {
    // ── Resolve the compose file path ───────────────────────────────
    let compose_path = if compose_file.is_absolute() {
        compose_file.to_path_buf()
    } else {
        repo_root.join(compose_file)
    };

    if !compose_path.exists() {
        anyhow::bail!(
            "Compose file not found: {}",
            compose_path.display()
        );
    }

    tracing::info!(
        compose_file = %compose_path.display(),
        branch = branch,
        args = ?args,
        "Proxying docker-compose with volume shadowing"
    );

    // ── Parse → Rewrite → Serialize ─────────────────────────────────
    let raw_yaml = std::fs::read_to_string(&compose_path)
        .with_context(|| format!("Failed to read {}", compose_path.display()))?;

    let mut doc: Value = serde_yaml::from_str(&raw_yaml)
        .with_context(|| format!("Failed to parse YAML in {}", compose_path.display()))?;

    let sanitized_branch = sanitize_branch_name(branch);
    let stats = rewrite_volumes(&mut doc, &sanitized_branch)?;

    tracing::info!(
        bind_mounts_rewritten = stats.bind_mounts,
        named_volumes_rewritten = stats.named_volumes,
        top_level_volumes_rewritten = stats.top_level_volumes,
        branch = %sanitized_branch,
        "Volume rewriting complete"
    );

    // ── Write the rewritten YAML to a temp file ─────────────────────
    //
    // We store it under ~/.git-warp/volumes/ so it's inspectable
    // for debugging and survives the proxy invocation for logs.
    let volumes_dir = warp_home.join("volumes");
    std::fs::create_dir_all(&volumes_dir)?;

    let rewritten_path = volumes_dir.join(format!(
        "docker-compose--{}--{}.yml",
        sanitized_branch,
        // Include a hash of the original path to avoid collisions
        // when multiple compose files exist in the repo.
        &hex::encode(sha2_hash(compose_path.to_string_lossy().as_bytes()))[..8]
    ));

    let rewritten_yaml = serde_yaml::to_string(&doc)
        .context("Failed to serialize rewritten compose YAML")?;

    std::fs::write(&rewritten_path, &rewritten_yaml)
        .with_context(|| format!("Failed to write rewritten YAML to {}", rewritten_path.display()))?;

    tracing::debug!(
        path = %rewritten_path.display(),
        "Wrote rewritten compose file"
    );

    // ── Invoke docker-compose ───────────────────────────────────────
    let exit_code = invoke_docker_compose(&rewritten_path, args, repo_root).await?;

    tracing::info!(exit_code = exit_code, "docker-compose exited");
    Ok(exit_code)
}

// ──────────────────────────────────────────────────────────────────────
// VOLUME REWRITING ENGINE
// ──────────────────────────────────────────────────────────────────────

/// Rewrite statistics — reported in logs for observability.
#[derive(Debug, Default)]
struct RewriteStats {
    bind_mounts: usize,
    named_volumes: usize,
    top_level_volumes: usize,
}

/// Walk the compose YAML tree and rewrite all volume references
/// to be branch-scoped.
fn rewrite_volumes(doc: &mut Value, branch: &str) -> Result<RewriteStats> {
    let mut stats = RewriteStats::default();

    // ── Phase 1: Rewrite per-service volumes ────────────────────────
    if let Some(services) = doc.get_mut("services").and_then(|s| s.as_mapping_mut()) {
        for (service_name, service_def) in services.iter_mut() {
            let svc_name = service_name
                .as_str()
                .unwrap_or("<unknown>");

            if let Some(volumes) = service_def.get_mut("volumes").and_then(|v| v.as_sequence_mut()) {
                for volume in volumes.iter_mut() {
                    match volume {
                        // Short-form string: "./host:/container" or "vol:/container"
                        Value::String(s) => {
                            if let Some(rewritten) = rewrite_short_volume(s, branch) {
                                tracing::debug!(
                                    service = svc_name,
                                    from = %s,
                                    to = %rewritten,
                                    "Rewrote short-form volume"
                                );
                                if is_bind_mount_path(s.split(':').next().unwrap_or("")) {
                                    stats.bind_mounts += 1;
                                } else {
                                    stats.named_volumes += 1;
                                }
                                *s = rewritten;
                            }
                        }
                        // Long-form object: { type: bind, source: ..., target: ... }
                        Value::Mapping(m) => {
                            if rewrite_long_volume(m, branch, svc_name) {
                                stats.bind_mounts += 1; // long-form is typically bind
                            }
                        }
                        _ => {} // Anonymous or unrecognized — skip
                    }
                }
            }
        }
    }

    // ── Phase 2: Rewrite top-level named volume definitions ─────────
    //
    // Top-level `volumes:` section defines named volumes:
    //   volumes:
    //     db_data:           →  db_data--warp--feature-login:
    //     redis_data:        →  redis_data--warp--feature-login:
    //
    // We must rename the keys here AND update any service-level
    // references (which Phase 1 already handled).
    if let Some(volumes_mapping) = doc.get("volumes").and_then(|v| v.as_mapping()).cloned() {
        let mut new_mapping = serde_yaml::Mapping::new();
        for (key, value) in volumes_mapping.iter() {
            if let Some(name) = key.as_str() {
                let new_name = format!("{}--warp--{}", name, branch);
                tracing::debug!(
                    from = name,
                    to = %new_name,
                    "Rewrote top-level volume definition"
                );
                new_mapping.insert(Value::String(new_name), value.clone());
                stats.top_level_volumes += 1;
            } else {
                // Non-string key (shouldn't happen) — preserve as-is
                new_mapping.insert(key.clone(), value.clone());
            }
        }
        if let Some(vol) = doc.get_mut("volumes") {
            *vol = Value::Mapping(new_mapping);
        }
    }

    Ok(stats)
}

/// Rewrite a short-form volume string.
///
/// Formats handled:
///   "./data:/var/lib/data"        → "./data--warp--main:/var/lib/data"
///   "./data:/var/lib/data:rw"     → "./data--warp--main:/var/lib/data:rw"
///   "pgdata:/var/lib/data"        → "pgdata--warp--main:/var/lib/data"
///   "/var/lib/data"               → None (anonymous — no host part)
///
/// Returns None if the volume should not be rewritten.
fn rewrite_short_volume(volume_str: &str, branch: &str) -> Option<String> {
    // Split on ':' — but carefully. Windows paths have colons too,
    // but we're Unix-only (WSL2/Linux), so the first colon is the
    // host:container separator.
    let parts: Vec<&str> = volume_str.splitn(3, ':').collect();

    match parts.len() {
        1 => {
            // Single path = anonymous volume or container-only mount.
            // Leave untouched.
            None
        }
        2 | 3 => {
            // parts[0] = host path or volume name
            // parts[1] = container path
            // parts[2] = optional mode (ro, rw, z, Z, etc.)
            let host = parts[0];
            let suffix = format!("--warp--{}", branch);

            // Don't double-rewrite if already tagged
            if host.contains("--warp--") {
                tracing::trace!(volume = volume_str, "Already rewritten — skipping");
                return None;
            }

            let new_host = format!("{}{}", host, suffix);
            let mut result = format!("{}:{}", new_host, parts[1]);
            if parts.len() == 3 {
                result.push(':');
                result.push_str(parts[2]);
            }
            Some(result)
        }
        _ => None,
    }
}

/// Rewrite a long-form volume mapping (YAML object).
///
/// Example input:
///   type: bind
///   source: ./pgdata
///   target: /var/lib/postgresql/data
///
/// Rewrites `source` to `./pgdata--warp--main`.
/// Returns true if a rewrite was performed.
fn rewrite_long_volume(mapping: &mut serde_yaml::Mapping, branch: &str, service: &str) -> bool {
    let source = match mapping.get_mut(Value::String("source".into())) {
        Some(Value::String(s)) => s,
        _ => return false,
    };

    // Don't double-rewrite
    if source.contains("--warp--") {
        return false;
    }

    let original = source.clone();
    let suffix = format!("--warp--{}", branch);
    source.push_str(&suffix);

    tracing::debug!(
        service = service,
        from = %original,
        to = %source,
        "Rewrote long-form volume source"
    );

    true
}

// ──────────────────────────────────────────────────────────────────────
// DOCKER-COMPOSE EXECUTION
// ──────────────────────────────────────────────────────────────────────

/// Invoke the real docker-compose binary with the rewritten compose file.
///
/// Tries `docker compose` (v2 plugin) first, falls back to
/// `docker-compose` (v1 standalone) if not found.
async fn invoke_docker_compose(
    compose_file: &Path,
    args: &[String],
    working_dir: &Path,
) -> Result<i32> {
    // ── Try docker compose v2 (plugin) first ────────────────────────
    let result = try_compose_command(
        "docker",
        &["compose", "-f"],
        compose_file,
        args,
        working_dir,
    )
    .await;

    match result {
        Ok(code) => return Ok(code),
        Err(e) => {
            tracing::debug!(
                error = %e,
                "docker compose (v2) not available, falling back to docker-compose (v1)"
            );
        }
    }

    // ── Fall back to docker-compose v1 ──────────────────────────────
    try_compose_command(
        "docker-compose",
        &["-f"],
        compose_file,
        args,
        working_dir,
    )
    .await
    .context("Neither 'docker compose' (v2) nor 'docker-compose' (v1) could be executed")
}

/// Execute a compose command and return its exit code.
async fn try_compose_command(
    binary: &str,
    prefix_args: &[&str],
    compose_file: &Path,
    user_args: &[String],
    working_dir: &Path,
) -> Result<i32> {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.current_dir(working_dir);

    // Build: docker compose -f <file> <user_args...>
    for arg in prefix_args {
        cmd.arg(arg);
    }
    cmd.arg(compose_file.as_os_str());
    for arg in user_args {
        cmd.arg(arg);
    }

    // Inherit stdio so the user sees docker-compose output in real time.
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());

    tracing::info!(
        command = %format!("{} {} {} {}",
            binary,
            prefix_args.join(" "),
            compose_file.display(),
            user_args.join(" ")
        ),
        "Executing docker-compose"
    );

    let status = cmd
        .status()
        .await
        .with_context(|| format!("Failed to execute '{}'", binary))?;

    Ok(status.code().unwrap_or(1))
}

// ──────────────────────────────────────────────────────────────────────
// UTILITY FUNCTIONS
// ──────────────────────────────────────────────────────────────────────

/// Sanitize a git branch name for use in Docker volume names and paths.
///
/// Rules:
///   - Replace '/' with '-'  (feature/login → feature-login)
///   - Replace spaces with '-'
///   - Remove characters not in [a-zA-Z0-9._-]
///   - Collapse consecutive dashes
///   - Trim leading/trailing dashes
pub fn sanitize_branch_name(branch: &str) -> String {
    let sanitized: String = branch
        .chars()
        .map(|c| match c {
            '/' | '\\' | ' ' => '-',
            c if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' => c,
            _ => '-',
        })
        .collect();

    // Collapse consecutive dashes: "a--b" → "a-b"
    let mut result = String::with_capacity(sanitized.len());
    let mut prev_dash = false;
    for c in sanitized.chars() {
        if c == '-' {
            if !prev_dash {
                result.push(c);
            }
            prev_dash = true;
        } else {
            result.push(c);
            prev_dash = false;
        }
    }

    result.trim_matches('-').to_string()
}

/// Check if a volume host path looks like a bind mount (filesystem path)
/// vs a named volume reference.
///
/// Bind mounts start with: ./ ../ / ~/
/// Named volumes are bare identifiers: "pgdata", "redis_cache"
fn is_bind_mount_path(host: &str) -> bool {
    host.starts_with('.')
        || host.starts_with('/')
        || host.starts_with('~')
}

/// Compute SHA-256 of a byte slice. Returns the raw 32-byte digest.
fn sha2_hash(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

// ──────────────────────────────────────────────────────────────────────
// UNIT TESTS
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Branch name sanitization ────────────────────────────────────

    #[test]
    fn sanitize_simple_branch() {
        assert_eq!(sanitize_branch_name("main"), "main");
    }

    #[test]
    fn sanitize_slashes() {
        assert_eq!(sanitize_branch_name("feature/login"), "feature-login");
    }

    #[test]
    fn sanitize_nested_slashes() {
        assert_eq!(
            sanitize_branch_name("feature/auth/oauth2"),
            "feature-auth-oauth2"
        );
    }

    #[test]
    fn sanitize_special_chars() {
        assert_eq!(sanitize_branch_name("fix/#123"), "fix-123");
    }

    #[test]
    fn sanitize_consecutive_specials() {
        assert_eq!(sanitize_branch_name("a///b"), "a-b");
    }

    // ── Short-form volume rewriting ─────────────────────────────────

    #[test]
    fn rewrite_bind_mount_relative() {
        let result = rewrite_short_volume("./pgdata:/var/lib/postgresql/data", "main");
        assert_eq!(
            result.unwrap(),
            "./pgdata--warp--main:/var/lib/postgresql/data"
        );
    }

    #[test]
    fn rewrite_bind_mount_with_mode() {
        let result = rewrite_short_volume("./data:/app/data:rw", "develop");
        assert_eq!(result.unwrap(), "./data--warp--develop:/app/data:rw");
    }

    #[test]
    fn rewrite_named_volume() {
        let result = rewrite_short_volume("db_data:/var/lib/postgresql/data", "feature-login");
        assert_eq!(
            result.unwrap(),
            "db_data--warp--feature-login:/var/lib/postgresql/data"
        );
    }

    #[test]
    fn rewrite_absolute_bind_mount() {
        let result = rewrite_short_volume("/tmp/logs:/var/log", "main");
        assert_eq!(result.unwrap(), "/tmp/logs--warp--main:/var/log");
    }

    #[test]
    fn skip_anonymous_volume() {
        let result = rewrite_short_volume("/var/lib/data", "main");
        assert!(result.is_none());
    }

    #[test]
    fn skip_already_rewritten() {
        let result = rewrite_short_volume(
            "./pgdata--warp--main:/var/lib/postgresql/data",
            "main",
        );
        assert!(result.is_none());
    }

    // ── Long-form volume rewriting ──────────────────────────────────

    #[test]
    fn rewrite_long_form_bind() {
        let mut mapping = serde_yaml::Mapping::new();
        mapping.insert(
            Value::String("type".into()),
            Value::String("bind".into()),
        );
        mapping.insert(
            Value::String("source".into()),
            Value::String("./pgdata".into()),
        );
        mapping.insert(
            Value::String("target".into()),
            Value::String("/var/lib/postgresql/data".into()),
        );

        let rewritten = rewrite_long_volume(&mut mapping, "main", "db");
        assert!(rewritten);
        assert_eq!(
            mapping.get(Value::String("source".into())).unwrap(),
            &Value::String("./pgdata--warp--main".into())
        );
    }

    #[test]
    fn skip_long_form_already_rewritten() {
        let mut mapping = serde_yaml::Mapping::new();
        mapping.insert(
            Value::String("source".into()),
            Value::String("./pgdata--warp--main".into()),
        );

        let rewritten = rewrite_long_volume(&mut mapping, "main", "db");
        assert!(!rewritten);
    }

    // ── Full YAML rewriting ─────────────────────────────────────────

    #[test]
    fn rewrite_full_compose_yaml() {
        let yaml = r#"
version: "3.8"
services:
  db:
    image: postgres:15
    volumes:
      - ./pgdata:/var/lib/postgresql/data
      - db_data:/backup
  redis:
    image: redis:7
    volumes:
      - redis_data:/data
volumes:
  db_data:
  redis_data:
    driver: local
"#;

        let mut doc: Value = serde_yaml::from_str(yaml).unwrap();
        let stats = rewrite_volumes(&mut doc, "feature-login").unwrap();

        // Verify service volumes were rewritten
        let db_vols = doc["services"]["db"]["volumes"].as_sequence().unwrap();
        assert_eq!(
            db_vols[0].as_str().unwrap(),
            "./pgdata--warp--feature-login:/var/lib/postgresql/data"
        );
        assert_eq!(
            db_vols[1].as_str().unwrap(),
            "db_data--warp--feature-login:/backup"
        );

        let redis_vols = doc["services"]["redis"]["volumes"].as_sequence().unwrap();
        assert_eq!(
            redis_vols[0].as_str().unwrap(),
            "redis_data--warp--feature-login:/data"
        );

        // Verify top-level volumes were rewritten
        let top_vols = doc["volumes"].as_mapping().unwrap();
        assert!(top_vols.contains_key(Value::String("db_data--warp--feature-login".into())));
        assert!(top_vols.contains_key(Value::String("redis_data--warp--feature-login".into())));

        // Verify stats
        assert_eq!(stats.bind_mounts, 1);
        assert_eq!(stats.named_volumes, 2);
        assert_eq!(stats.top_level_volumes, 2);
    }

    // ── Bind mount detection ────────────────────────────────────────

    #[test]
    fn detect_relative_bind_mount() {
        assert!(is_bind_mount_path("./data"));
        assert!(is_bind_mount_path("../data"));
    }

    #[test]
    fn detect_absolute_bind_mount() {
        assert!(is_bind_mount_path("/var/lib/data"));
    }

    #[test]
    fn detect_home_bind_mount() {
        assert!(is_bind_mount_path("~/data"));
    }

    #[test]
    fn detect_named_volume() {
        assert!(!is_bind_mount_path("db_data"));
        assert!(!is_bind_mount_path("my-volume"));
    }
}
