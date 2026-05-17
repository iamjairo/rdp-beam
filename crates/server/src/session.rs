use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use beam_protocol::SessionInfo;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::RwLock;
use uuid::Uuid;

const SESSION_DIR: &str = "/var/lib/beam/sessions";

/// Resolve `backend = "auto"` to a concrete `"xorg"` or `"wayland"` value by
/// reading `/etc/os-release`. Called once at server start and the result is
/// pinned for the lifetime of the process — agents never re-detect.
///
/// Picks `wayland` on Ubuntu 26.04+ and any distro where `VARIANT_ID` or
/// `ID_LIKE` indicates a Wayland-default system; `xorg` otherwise.
/// Non-`auto` inputs pass through unchanged.
pub(crate) fn resolve_backend(value: &str) -> String {
    if value != "auto" {
        return value.to_string();
    }
    let osrel = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let mut id = String::new();
    let mut version_id = String::new();
    for line in osrel.lines() {
        if let Some(v) = line.strip_prefix("ID=") {
            id = v.trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("VERSION_ID=") {
            version_id = v.trim_matches('"').to_string();
        }
    }
    // Parse only the first two dot-separated components — handles both
    // `26.04` and `26.04.1` point releases without choking on the inner dot.
    let mut parts = version_id.split('.');
    let maj: Option<u32> = parts.next().and_then(|s| s.parse().ok());
    let min: Option<u32> = parts.next().and_then(|s| s.parse().ok());
    let is_ubuntu_2604_plus =
        id == "ubuntu" && matches!((maj, min), (Some(m), Some(n)) if (m, n) >= (26, 4));
    if is_ubuntu_2604_plus {
        "wayland".to_string()
    } else {
        "xorg".to_string()
    }
}

#[cfg(test)]
mod backend_tests {
    use super::resolve_backend;

    #[test]
    fn explicit_xorg_passes_through() {
        assert_eq!(resolve_backend("xorg"), "xorg");
    }

    #[test]
    fn explicit_wayland_passes_through() {
        assert_eq!(resolve_backend("wayland"), "wayland");
    }

    #[test]
    fn auto_returns_xorg_or_wayland() {
        // Cannot mock /etc/os-release here; just verify the result is in the valid set.
        let r = resolve_backend("auto");
        assert!(
            r == "xorg" || r == "wayland",
            "auto should resolve to xorg or wayland, got {r}"
        );
    }
}

#[derive(Serialize, Deserialize)]
struct PersistedSession {
    session_id: Uuid,
    username: String,
    display: u32,
    width: u32,
    height: u32,
    created_at: u64,
    /// Legacy field: PID of the agent process (pre-systemd sessions).
    /// Kept for backward compatibility during rolling upgrades.
    #[serde(default)]
    agent_pid: u32,
    agent_token: String,
    #[serde(default)]
    release_token: String,
    /// systemd transient service unit name (e.g., "beam-agent-<uuid>").
    #[serde(default)]
    systemd_unit: Option<String>,
}

/// Constant-time byte comparison to prevent timing side-channel attacks.
/// Always iterates over the full max(a.len(), b.len()) range so that
/// differing lengths cannot be detected via timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = if a.len() != b.len() { 1u8 } else { 0u8 };
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

/// Generate a random hex token for agent authentication.
fn generate_agent_token() -> String {
    use std::fmt::Write;
    use std::io::Read;
    let mut bytes = [0u8; 32];
    let f = std::fs::File::open("/dev/urandom").expect("Failed to open /dev/urandom");
    (&f).read_exact(&mut bytes)
        .expect("Failed to read random bytes");
    let mut hex = String::with_capacity(64);
    for b in &bytes {
        write!(hex, "{b:02x}").unwrap();
    }
    hex
}

/// Generate a short random hex token for session release (16 hex chars = 8 bytes).
/// Used by `navigator.sendBeacon()` on tab close — shorter than agent tokens
/// since it only needs to be unpredictable, not cryptographically strong.
fn generate_release_token() -> String {
    use std::fmt::Write;
    use std::io::Read;
    let mut bytes = [0u8; 8];
    let f = std::fs::File::open("/dev/urandom").expect("Failed to open /dev/urandom");
    (&f).read_exact(&mut bytes)
        .expect("Failed to read random bytes");
    let mut hex = String::with_capacity(16);
    for b in &bytes {
        write!(hex, "{b:02x}").unwrap();
    }
    hex
}

/// Manages the lifecycle of remote desktop sessions.
pub struct SessionManager {
    sessions: RwLock<HashMap<Uuid, ManagedSession>>,
    default_width: u32,
    default_height: u32,
    /// Pool of available display numbers for recycling
    display_pool: RwLock<DisplayPool>,
    /// Path to TLS cert PEM for agent cert pinning
    tls_cert_path: Option<String>,
    /// Video/audio config to pass to agents
    video_config: beam_protocol::VideoConfig,
    /// GPU driver mode to pass to agents: "auto", "nvidia", "dummy"
    gpu_driver: String,
    /// Display backend to pass to agents: "auto", "xorg", "wayland".
    /// `auto` is resolved server-side by reading /etc/os-release before spawn
    /// so each agent gets a concrete value and never re-runs detection.
    backend: String,
    /// Starting display number (for DFP output allocation)
    display_start: u32,
}

struct DisplayPool {
    next: u32,
    /// Display numbers freed by destroyed sessions
    free: HashSet<u32>,
}

impl DisplayPool {
    fn new(start: u32) -> Self {
        Self {
            next: start,
            free: HashSet::new(),
        }
    }

    fn allocate(&mut self) -> u32 {
        if let Some(&num) = self.free.iter().next() {
            self.free.remove(&num);
            num
        } else {
            let num = self.next;
            self.next += 1;
            num
        }
    }

    fn release(&mut self, num: u32) {
        self.free.insert(num);
    }
}

struct ManagedSession {
    pub info: SessionInfo,
    /// Name of the systemd transient service managing this agent
    /// (e.g., "beam-agent-<uuid>"). None for legacy pre-systemd sessions.
    pub systemd_unit: Option<String>,
    /// Timestamp of last heartbeat/activity (Unix epoch seconds)
    pub last_activity: u64,
    /// Secret token the agent must present on WebSocket upgrade
    pub agent_token: String,
    /// Short token for browser tab close release (sent via sendBeacon)
    pub release_token: String,
    /// Generation counter for grace-period cancellation. Each new grace period
    /// increments this; the spawned timer checks if the generation still matches.
    /// This avoids a race where overlapping grace periods share a single boolean.
    pub grace_generation: Arc<AtomicU64>,
    /// Number of times the agent has been restarted after unexpected exits.
    /// Managed by systemd (Restart=on-failure); kept for metrics/tests.
    #[allow(dead_code)]
    pub restart_count: u32,
    /// Per-session idle timeout override in seconds. None = use global default.
    pub idle_timeout_override: Option<u64>,
}

impl SessionManager {
    pub fn new(
        display_start: u32,
        default_width: u32,
        default_height: u32,
        tls_cert_path: Option<String>,
        video_config: beam_protocol::VideoConfig,
        gpu_driver: String,
        backend: String,
    ) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            default_width,
            default_height,
            display_pool: RwLock::new(DisplayPool::new(display_start)),
            tls_cert_path,
            video_config,
            gpu_driver,
            backend: resolve_backend(&backend),
            display_start,
        }
    }

    /// Create a new session for a user.
    ///
    /// Allocates a display number and spawns the beam-agent process.
    /// Returns an error if max_sessions would be exceeded.
    pub async fn create_session(
        &self,
        username: &str,
        server_url: &str,
        max_sessions: usize,
        initial_width: Option<u32>,
        initial_height: Option<u32>,
        idle_timeout_override: Option<u64>,
    ) -> Result<SessionInfo> {
        // Use client viewport dimensions if provided, clamped to sane bounds.
        // Fall back to config defaults for old clients or missing values.
        let width = initial_width
            .filter(|&w| (320..=3840).contains(&w))
            .unwrap_or(self.default_width);
        let height = initial_height
            .filter(|&h| (240..=2160).contains(&h))
            .unwrap_or(self.default_height);

        // Atomically check max sessions and reserve a slot under the write lock
        // to prevent TOCTOU race (two concurrent logins both passing the check).
        // Both locks are acquired in a single scope to avoid deadlock from
        // inconsistent lock ordering.
        let session_id = Uuid::new_v4();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let agent_token = generate_agent_token();
        let release_token = generate_release_token();
        let display_num;

        {
            let mut sessions = self.sessions.write().await;
            if sessions.len() >= max_sessions {
                anyhow::bail!("Maximum number of sessions reached ({max_sessions})");
            }

            display_num = self.display_pool.write().await.allocate();

            let info = SessionInfo {
                id: session_id,
                username: username.to_string(),
                display: display_num,
                width,
                height,
                created_at: now,
            };

            // Reserve the slot immediately so concurrent requests see it
            let managed = ManagedSession {
                info: info.clone(),
                systemd_unit: None,
                last_activity: now,
                agent_token: agent_token.clone(),
                release_token: release_token.clone(),
                grace_generation: Arc::new(AtomicU64::new(0)),
                restart_count: 0,
                idle_timeout_override,
            };
            sessions.insert(session_id, managed);
        }

        let info = SessionInfo {
            id: session_id,
            username: username.to_string(),
            display: display_num,
            width,
            height,
            created_at: now,
        };

        // Clean up stale temp files from previous sessions on this display number.
        // These may be owned by a different user if the previous agent was killed
        // without running its Drop handler (e.g., SIGKILL during deployment).
        let _ = std::fs::remove_file(format!("/tmp/beam-xorg-{display_num}.conf"));
        let _ = std::fs::remove_file(format!("/tmp/beam-pulse-{display_num}.pa"));
        let _ = std::fs::remove_dir_all(format!("/tmp/beam-pulse-{display_num}"));
        // Remove stale X lock file if Xorg didn't clean up
        let _ = std::fs::remove_file(format!("/tmp/.X{display_num}-lock"));
        // Keyring dir may be owned by a different user (mode 700); server runs as root
        let _ = std::fs::remove_dir_all(format!("/tmp/beam-keyring-{display_num}"));
        // EDID file from GPU-accelerated display sessions
        let _ = std::fs::remove_file(format!("/tmp/beam-edid-{display_num}.bin"));

        // Start the agent as a systemd transient service (outside the write lock)
        let unit_name = match self.spawn_agent(&info, server_url, &agent_token).await {
            Ok(unit) => unit,
            Err(e) => {
                // Clean up the reserved slot on spawn failure
                self.sessions.write().await.remove(&session_id);
                self.display_pool.write().await.release(display_num);
                return Err(e).context("Failed to start agent service");
            }
        };

        // Update the reserved slot with the systemd unit name
        {
            let mut sessions = self.sessions.write().await;
            if let Some(session) = sessions.get_mut(&session_id) {
                session.systemd_unit = Some(unit_name);
            }
        }

        tracing::info!(
            %session_id,
            %username,
            display_num,
            "Session created"
        );

        Ok(info)
    }

    /// Destroy a session, stopping the agent's systemd service.
    /// systemd sends SIGTERM to all processes in the service's cgroup
    /// (agent, Xorg, PulseAudio, desktop), waits TimeoutStopSec, then SIGKILL.
    /// After the service stops, the display number is recycled.
    pub async fn destroy_session(&self, session_id: Uuid) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.remove(&session_id) {
            let display_num = session.info.display;
            let unit = session.systemd_unit.clone();

            // Drop the write lock before blocking on systemctl
            drop(sessions);

            // Stop the systemd service (SIGTERM → TimeoutStopSec → SIGKILL)
            if let Some(ref unit) = unit {
                tracing::info!(%session_id, %unit, "Stopping agent service");
                match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    Command::new("systemctl").args(["stop", unit]).output(),
                )
                .await
                {
                    Ok(Ok(o)) if o.status.success() => {
                        tracing::info!(%session_id, %unit, "Agent service stopped");
                    }
                    Ok(Ok(o)) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        // Unit may already be gone (--collect), that's fine
                        tracing::debug!(%session_id, "systemctl stop {unit}: {stderr}");
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(%session_id, "Failed to run systemctl: {e}");
                    }
                    Err(_) => {
                        tracing::warn!(%session_id, "systemctl stop timed out, sending SIGKILL");
                        let _ = Command::new("systemctl")
                            .args(["kill", "--signal=SIGKILL", unit])
                            .output()
                            .await;
                    }
                }
            }

            // Wait for Xorg lock file cleanup before recycling the display number.
            // systemd's KillMode=control-group ensures all children are stopped,
            // but the lock file removal may lag slightly.
            let lock_path = format!("/tmp/.X{display_num}-lock");
            for _ in 0..20 {
                if !std::path::Path::new(&lock_path).exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            // Now that the agent has exited, recycle the display number
            self.display_pool.write().await.release(display_num);
            tracing::info!(%session_id, "Session destroyed");
        }
        Ok(())
    }

    /// List all active sessions.
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions.values().map(|s| s.info.clone()).collect()
    }

    /// List all active sessions with their last activity timestamps.
    pub async fn list_sessions_with_activity(&self) -> Vec<(SessionInfo, u64)> {
        let sessions = self.sessions.read().await;
        sessions
            .values()
            .map(|s| (s.info.clone(), s.last_activity))
            .collect()
    }

    /// Get a specific session's info.
    pub async fn get_session(&self, session_id: Uuid) -> Option<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions.get(&session_id).map(|s| s.info.clone())
    }

    /// Update the heartbeat timestamp for a session.
    pub async fn heartbeat(&self, session_id: Uuid) -> bool {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.last_activity = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            true
        } else {
            false
        }
    }

    /// Return IDs of sessions that haven't had activity past their idle timeout.
    /// Each session uses its own override if set, otherwise the global `max_idle_secs`.
    pub async fn stale_sessions(&self, max_idle_secs: u64) -> Vec<Uuid> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let sessions = self.sessions.read().await;
        sessions
            .values()
            .filter(|s| {
                let effective = s.idle_timeout_override.unwrap_or(max_idle_secs);
                effective > 0 && now.saturating_sub(s.last_activity) > effective
            })
            .map(|s| s.info.id)
            .collect()
    }

    /// Get the effective idle timeout for a session (override or global default).
    pub async fn get_idle_timeout(&self, session_id: Uuid, global_default: u64) -> u64 {
        let sessions = self.sessions.read().await;
        sessions
            .get(&session_id)
            .and_then(|s| s.idle_timeout_override)
            .unwrap_or(global_default)
    }

    /// Verify the agent token for a session. Returns true if valid.
    /// Uses constant-time comparison to prevent timing side-channel attacks.
    pub async fn verify_agent_token(&self, session_id: Uuid, token: &str) -> bool {
        let sessions = self.sessions.read().await;
        sessions
            .get(&session_id)
            .map(|s| constant_time_eq(s.agent_token.as_bytes(), token.as_bytes()))
            .unwrap_or(false)
    }

    /// Find a session by username (returns the first match).
    pub async fn find_by_username(&self, username: &str) -> Option<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions
            .values()
            .find(|s| s.info.username == username)
            .map(|s| s.info.clone())
    }

    /// Get the release token for a session.
    pub async fn get_release_token(&self, session_id: Uuid) -> Option<String> {
        let sessions = self.sessions.read().await;
        sessions.get(&session_id).map(|s| s.release_token.clone())
    }

    /// Verify the release token for a session. Returns true if valid.
    /// Uses constant-time comparison to prevent timing side-channel attacks.
    pub async fn verify_release_token(&self, session_id: Uuid, token: &str) -> bool {
        let sessions = self.sessions.read().await;
        sessions
            .get(&session_id)
            .map(|s| constant_time_eq(s.release_token.as_bytes(), token.as_bytes()))
            .unwrap_or(false)
    }

    /// Prepare a grace-period cleanup for a session. Increments the generation
    /// counter and returns (counter_arc, generation) so the caller can spawn a
    /// background timer that checks if the generation still matches.
    ///
    /// Each call invalidates all previously spawned grace-period timers because
    /// their generation won't match. This avoids the race where overlapping
    /// grace periods share a single boolean cancel flag.
    pub async fn start_grace_period(&self, session_id: Uuid) -> Option<(Arc<AtomicU64>, u64)> {
        let sessions = self.sessions.read().await;
        sessions.get(&session_id).map(|s| {
            let generation = s.grace_generation.fetch_add(1, Ordering::SeqCst) + 1;
            (Arc::clone(&s.grace_generation), generation)
        })
    }

    /// Cancel any pending grace-period cleanup for a session.
    /// Called when a browser WebSocket reconnects. Bumps the generation
    /// so any sleeping timer will see a mismatch and abort.
    pub async fn cancel_grace_period(&self, session_id: Uuid) {
        let sessions = self.sessions.read().await;
        if let Some(s) = sessions.get(&session_id) {
            s.grace_generation.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Increment the restart count for a session and return the new count.
    /// Returns `None` if the session does not exist.
    #[allow(dead_code)]
    pub async fn increment_restart_count(&self, session_id: Uuid) -> Option<u32> {
        let mut sessions = self.sessions.write().await;
        sessions.get_mut(&session_id).map(|s| {
            s.restart_count += 1;
            s.restart_count
        })
    }

    /// Get the current restart count for a session.
    #[cfg(test)]
    pub async fn get_restart_count(&self, session_id: Uuid) -> Option<u32> {
        let sessions = self.sessions.read().await;
        sessions.get(&session_id).map(|s| s.restart_count)
    }

    /// Respawn the agent for an existing session after permanent failure.
    ///
    /// Stops any existing systemd unit, generates a new agent token,
    /// starts a new systemd transient service, and updates the session.
    ///
    /// Returns `None` if the session does not exist.
    #[allow(dead_code)]
    pub async fn respawn_agent(&self, session_id: Uuid, server_url: &str) -> Result<Option<()>> {
        // Read session info under a read lock first
        let (info, old_unit) = {
            let sessions = self.sessions.read().await;
            match sessions.get(&session_id) {
                Some(s) => (s.info.clone(), s.systemd_unit.clone()),
                None => return Ok(None),
            }
        };

        // Stop old systemd unit if still lingering
        if let Some(ref unit) = old_unit {
            let _ = Command::new("systemctl")
                .args(["stop", unit])
                .output()
                .await;
        }

        let new_token = generate_agent_token();

        // Clean up stale temp files before respawn
        let display_num = info.display;
        let _ = std::fs::remove_file(format!("/tmp/beam-xorg-{display_num}.conf"));
        let _ = std::fs::remove_file(format!("/tmp/beam-pulse-{display_num}.pa"));
        let _ = std::fs::remove_dir_all(format!("/tmp/beam-pulse-{display_num}"));
        let _ = std::fs::remove_file(format!("/tmp/.X{display_num}-lock"));
        let _ = std::fs::remove_dir_all(format!("/tmp/beam-keyring-{display_num}"));
        let _ = std::fs::remove_file(format!("/tmp/beam-edid-{display_num}.bin"));

        let unit_name = self.spawn_agent(&info, server_url, &new_token).await?;

        // Update the session with the new systemd unit and token
        {
            let mut sessions = self.sessions.write().await;
            if let Some(session) = sessions.get_mut(&session_id) {
                session.systemd_unit = Some(unit_name);
                session.agent_token = new_token;
            }
        }

        Ok(Some(()))
    }

    /// Start the agent as a systemd transient service.
    ///
    /// Uses `systemd-run` to create a service unit that:
    /// - Runs as the authenticated user (`User=`)
    /// - Opens a PAM session (`PAMName=beam`) → registers with logind
    /// - Auto-restarts on crash (`Restart=on-failure`)
    /// - Kills all children on stop (`KillMode=control-group`)
    /// - Is auto-collected when done (`--collect`)
    ///
    /// Returns the systemd unit name on success.
    async fn spawn_agent(
        &self,
        info: &SessionInfo,
        server_url: &str,
        agent_token: &str,
    ) -> Result<String> {
        let unit_name = format!("beam-agent-{}", info.id);
        let display_str = format!(":{}", info.display);

        // Try to find the agent binary in the same directory as the server,
        // or fall back to "beam-agent" in PATH.
        let agent_path = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|parent| parent.join("beam-agent")))
            .filter(|p| p.exists())
            .unwrap_or_else(|| "beam-agent".into());
        let agent_path_str = agent_path.to_string_lossy();

        // Ensure log directory exists (systemd opens the file as PID 1)
        let log_dir = "/var/log/beam";
        let _ = std::fs::create_dir_all(log_dir);
        let log_path = format!("{log_dir}/agent-{}.log", info.id);

        // Look up user info for logging and XDG_RUNTIME_DIR
        let user_info = lookup_user(&info.username);
        if let Some(ref ui) = user_info {
            tracing::info!(
                username = %info.username,
                uid = ui.uid,
                gid = ui.gid,
                home = %ui.home,
                "Starting agent service as user"
            );
        } else {
            tracing::warn!(
                username = %info.username,
                "User not found in system — systemd-run will fail"
            );
        }

        // Build systemd-run command
        let mut cmd = Command::new("systemd-run");

        // -- systemd-run flags --
        cmd.args(["--unit", &unit_name]);
        cmd.args([
            "--description",
            &format!("Beam desktop for {} on {}", info.username, display_str),
        ]);
        cmd.args(["--uid", &info.username]);

        // PAMName=beam triggers pam_systemd → logind session registration.
        // This is what makes FUSE, snap, and /run/user/<uid> work.
        cmd.args(["--property", "PAMName=beam"]);
        cmd.args(["--property", "Type=simple"]);
        cmd.args(["--property", &format!("StandardOutput=append:{log_path}")]);
        cmd.args(["--property", &format!("StandardError=append:{log_path}")]);

        // Crash recovery: systemd auto-restarts the agent on failure
        cmd.args(["--property", "Restart=on-failure"]);
        cmd.args(["--property", "RestartSec=2"]);
        cmd.args(["--property", "StartLimitBurst=3"]);
        cmd.args(["--property", "StartLimitIntervalSec=60"]);

        // Graceful shutdown and cleanup
        cmd.args(["--property", "TimeoutStopSec=10"]);
        cmd.args(["--property", "KillMode=control-group"]);

        // Auto-remove the unit when it stops (no leftover failed units)
        cmd.arg("--collect");

        // Don't wait for the service to reach "active" state — PAM session
        // setup (pam_systemd → logind D-Bus) can take seconds on LDAP-backed
        // systems. The agent connects back via WebSocket when ready.
        cmd.arg("--no-block");

        // -- Environment variables --
        // Agent token via env (CLI args are visible in /proc/<pid>/cmdline)
        cmd.args(["--setenv", &format!("BEAM_AGENT_TOKEN={agent_token}")]);
        cmd.args(["--setenv", &format!("DISPLAY={display_str}")]);
        cmd.args(["--setenv", "RUST_LOG=info"]);

        // XDG_RUNTIME_DIR — pam_systemd creates this, but set explicitly as fallback
        if let Some(ref ui) = user_info {
            cmd.args(["--setenv", &format!("XDG_RUNTIME_DIR=/run/user/{}", ui.uid)]);
        }

        // -- Separator --
        cmd.arg("--");

        // -- Agent binary and arguments --
        cmd.arg(agent_path_str.as_ref());
        cmd.args(["--display", &display_str]);
        cmd.args(["--session-id", &info.id.to_string()]);
        cmd.args(["--server-url", server_url]);
        cmd.args(["--width", &info.width.to_string()]);
        cmd.args(["--height", &info.height.to_string()]);
        cmd.args(["--framerate", &self.video_config.framerate.to_string()]);
        cmd.args(["--bitrate", &self.video_config.bitrate.to_string()]);
        cmd.args(["--max-width", &self.video_config.max_width.to_string()]);
        cmd.args(["--max-height", &self.video_config.max_height.to_string()]);

        if let Some(ref encoder) = self.video_config.encoder {
            cmd.args(["--encoder", encoder]);
        }
        if let Some(ref cert_path) = self.tls_cert_path {
            cmd.args(["--tls-cert", cert_path]);
        }
        cmd.args(["--gpu-driver", &self.gpu_driver]);
        cmd.args(["--backend", &self.backend]);
        cmd.args(["--display-start", &self.display_start.to_string()]);

        // Run systemd-run with --no-block — exits immediately after queuing
        // the start job. The 10s timeout covers only the D-Bus call to PID 1.
        let output = tokio::time::timeout(std::time::Duration::from_secs(10), cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("systemd-run timed out starting agent (30s)"))?
            .with_context(|| format!("Failed to run systemd-run for display {display_str}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("systemd-run failed for {unit_name}: {stderr}");
        }

        tracing::info!(
            session_id = %info.id,
            display = display_str,
            unit = %unit_name,
            "Agent service started via systemd"
        );

        Ok(unit_name)
    }

    /// Save all active sessions to disk for graceful restart.
    /// Agents are left running — the new server process re-adopts them.
    pub async fn persist_sessions(&self) -> Result<()> {
        let dir = Path::new(SESSION_DIR);
        std::fs::create_dir_all(dir).context("Failed to create session persistence directory")?;

        // Clean old files
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }

        let sessions = self.sessions.read().await;
        let mut count = 0;
        for (id, managed) in sessions.iter() {
            // Only persist sessions that have a running agent
            if managed.systemd_unit.is_none() {
                continue;
            }
            let persisted = PersistedSession {
                session_id: *id,
                username: managed.info.username.clone(),
                display: managed.info.display,
                width: managed.info.width,
                height: managed.info.height,
                created_at: managed.info.created_at,
                agent_pid: 0, // legacy field, not used for systemd sessions
                agent_token: managed.agent_token.clone(),
                release_token: managed.release_token.clone(),
                systemd_unit: managed.systemd_unit.clone(),
            };
            let path = dir.join(format!("{id}.json"));
            let tmp_path = dir.join(format!("{id}.json.tmp"));
            let data = serde_json::to_string_pretty(&persisted)?;

            // Write with restricted permissions (contains agent token)
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)
                .with_context(|| format!("Failed to write {}", tmp_path.display()))?;
            file.write_all(data.as_bytes())?;
            std::fs::rename(&tmp_path, &path)?;
            count += 1;
        }

        tracing::info!(count, "Persisted sessions to disk");
        Ok(())
    }

    /// Restore sessions from a previous graceful shutdown.
    /// Checks each session's systemd unit (or legacy PID) to verify the agent
    /// is still alive. Returns session IDs for sessions successfully restored.
    pub async fn restore_sessions(&self) -> Vec<Uuid> {
        let dir = Path::new(SESSION_DIR);
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut restored = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            let data = match std::fs::read_to_string(&path) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "Failed to read session file: {e}");
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
            };

            let persisted: PersistedSession = match serde_json::from_str(&data) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "Failed to parse session file: {e}");
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
            };

            // Verify agent is still running — check systemd unit first, fall back to PID
            let unit_name = persisted
                .systemd_unit
                .clone()
                .unwrap_or_else(|| format!("beam-agent-{}", persisted.session_id));

            let is_active = std::process::Command::new("systemctl")
                .args(["is-active", "--quiet", &unit_name])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);

            // Legacy fallback: check PID for pre-systemd sessions
            let is_alive = is_active
                || (persisted.agent_pid > 0
                    && nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(persisted.agent_pid as i32),
                        None,
                    )
                    .is_ok());

            if !is_alive {
                tracing::info!(
                    session_id = %persisted.session_id,
                    unit = %unit_name,
                    "Agent no longer alive, skipping"
                );
                let _ = std::fs::remove_file(&path);
                continue;
            }

            // Reserve the display number to avoid double-allocation
            {
                let mut pool = self.display_pool.write().await;
                pool.free.remove(&persisted.display);
                if persisted.display >= pool.next {
                    pool.next = persisted.display + 1;
                }
            }

            let info = SessionInfo {
                id: persisted.session_id,
                username: persisted.username.clone(),
                display: persisted.display,
                width: persisted.width,
                height: persisted.height,
                created_at: persisted.created_at,
            };

            // If restoring an old session file without release_token, generate one
            let release_token = if persisted.release_token.is_empty() {
                generate_release_token()
            } else {
                persisted.release_token
            };

            let managed = ManagedSession {
                info,
                systemd_unit: if is_active {
                    Some(unit_name.clone())
                } else {
                    None
                },
                last_activity: now,
                agent_token: persisted.agent_token,
                release_token,
                grace_generation: Arc::new(AtomicU64::new(0)),
                restart_count: 0,
                idle_timeout_override: None, // restored sessions use global default
            };

            let mut sessions = self.sessions.write().await;
            sessions.insert(persisted.session_id, managed);
            restored.push(persisted.session_id);

            tracing::info!(
                session_id = %persisted.session_id,
                username = %persisted.username,
                display = persisted.display,
                unit = %unit_name,
                active = is_active,
                "Restored session from disk"
            );

            let _ = std::fs::remove_file(&path);
        }

        restored
    }

    /// Get the systemd unit name for a session.
    #[allow(dead_code)]
    pub async fn get_systemd_unit(&self, session_id: Uuid) -> Option<String> {
        let sessions = self.sessions.read().await;
        sessions
            .get(&session_id)
            .and_then(|s| s.systemd_unit.clone())
    }
}

struct UserInfo {
    uid: u32,
    gid: u32,
    home: String,
}

/// Look up a Unix user by name, returning UID, GID, and home directory.
/// Uses getpwnam via nix, which supports NSS (LDAP, SSSD, etc.).
fn lookup_user(username: &str) -> Option<UserInfo> {
    let user = nix::unistd::User::from_name(username).ok()??;
    Some(UserInfo {
        uid: user.uid.as_raw(),
        gid: user.gid.as_raw(),
        home: user.dir.to_string_lossy().into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_root_user() {
        // root always exists on Linux
        let user = lookup_user("root");
        assert!(user.is_some(), "root user should exist");
        let user = user.unwrap();
        assert_eq!(user.uid, 0);
        assert_eq!(user.gid, 0);
        assert_eq!(user.home, "/root");
    }

    #[test]
    fn lookup_nonexistent_user() {
        let user = lookup_user("beam_nonexistent_user_12345");
        assert!(user.is_none());
    }

    #[test]
    fn display_pool_allocates_sequentially() {
        let mut pool = DisplayPool::new(10);
        assert_eq!(pool.allocate(), 10);
        assert_eq!(pool.allocate(), 11);
        assert_eq!(pool.allocate(), 12);
    }

    #[test]
    fn display_pool_recycles() {
        let mut pool = DisplayPool::new(10);
        assert_eq!(pool.allocate(), 10);
        assert_eq!(pool.allocate(), 11);
        pool.release(10);
        // Should reuse 10 before allocating 12
        assert_eq!(pool.allocate(), 10);
        assert_eq!(pool.allocate(), 12);
    }

    #[test]
    fn agent_token_is_64_hex_chars() {
        let token = generate_agent_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn agent_token_is_unique() {
        let t1 = generate_agent_token();
        let t2 = generate_agent_token();
        assert_ne!(t1, t2);
    }

    #[tokio::test]
    async fn verify_agent_token_rejects_wrong_token() {
        let manager = SessionManager::new(
            100,
            1920,
            1080,
            None,
            beam_protocol::VideoConfig::default(),
            "auto".to_string(),
            "auto".to_string(),
        );
        let id = Uuid::new_v4();
        // Non-existent session should reject
        assert!(!manager.verify_agent_token(id, "fake-token").await);
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hell"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn release_token_is_16_hex_chars() {
        let token = generate_release_token();
        assert_eq!(token.len(), 16);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn release_token_is_unique() {
        let t1 = generate_release_token();
        let t2 = generate_release_token();
        assert_ne!(t1, t2);
    }

    #[tokio::test]
    async fn verify_release_token_rejects_wrong_token() {
        let manager = SessionManager::new(
            100,
            1920,
            1080,
            None,
            beam_protocol::VideoConfig::default(),
            "auto".to_string(),
            "auto".to_string(),
        );
        let id = Uuid::new_v4();
        // Non-existent session should reject
        assert!(!manager.verify_release_token(id, "fake-token").await);
    }

    #[tokio::test]
    async fn grace_generation_counter_works() {
        let counter = Arc::new(AtomicU64::new(0));
        // Start a grace period — generation bumps from 0 to 1
        let my_gen = counter.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(my_gen, 1);
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Cancel (reconnect) — bumps to 2
        counter.fetch_add(1, Ordering::SeqCst);
        // The old timer's generation (1) no longer matches (2)
        assert_ne!(counter.load(Ordering::SeqCst), my_gen);

        // Start a second grace period — bumps to 3
        let my_gen2 = counter.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(my_gen2, 3);
        // First timer still mismatches
        assert_ne!(counter.load(Ordering::SeqCst), my_gen);
        // Second timer matches
        assert_eq!(counter.load(Ordering::SeqCst), my_gen2);
    }

    #[test]
    fn constant_time_eq_different_lengths() {
        // Short vs long — must return false without short-circuiting
        assert!(!constant_time_eq(b"abc", b"abcdef"));
        assert!(!constant_time_eq(b"abcdef", b"abc"));
        // Same prefix, different length
        assert!(!constant_time_eq(b"token", b"token_extra"));
        // Single byte vs empty
        assert!(!constant_time_eq(b"x", b""));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn constant_time_eq_last_byte_differs() {
        // Must not short-circuit on matching prefix
        assert!(!constant_time_eq(b"abcdefg1", b"abcdefg2"));
        assert!(!constant_time_eq(
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaX",
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaY",
        ));
    }

    #[test]
    fn constant_time_eq_length_difference_with_zero_padding() {
        // The longer string ends with null bytes. The XOR with unwrap_or(0)
        // padding would produce 0, but the initial length check must still
        // cause the function to return false.
        let short = b"abc";
        let mut long = Vec::from(&b"abc"[..]);
        long.extend_from_slice(&[0u8; 300]);

        assert!(
            !constant_time_eq(short, &long),
            "different lengths must return false even when padding XOR is zero"
        );
        assert!(
            !constant_time_eq(&long, short),
            "different lengths must return false (reversed argument order)"
        );
    }

    #[test]
    fn constant_time_eq_single_byte_values() {
        assert!(constant_time_eq(b"\x00", b"\x00"));
        assert!(constant_time_eq(b"\xff", b"\xff"));
        assert!(!constant_time_eq(b"\x00", b"\x01"));
        assert!(!constant_time_eq(b"\x00", b"\xff"));
    }

    #[tokio::test]
    async fn increment_restart_count_returns_new_count() {
        let manager = SessionManager::new(
            100,
            1920,
            1080,
            None,
            beam_protocol::VideoConfig::default(),
            "auto".to_string(),
            "auto".to_string(),
        );
        let id = Uuid::new_v4();

        // Insert a session manually
        {
            let mut sessions = manager.sessions.write().await;
            sessions.insert(
                id,
                ManagedSession {
                    info: SessionInfo {
                        id,
                        username: "test".to_string(),
                        display: 100,
                        width: 1920,
                        height: 1080,
                        created_at: 0,
                    },
                    systemd_unit: None,
                    last_activity: 0,
                    agent_token: "token".to_string(),
                    release_token: "release".to_string(),
                    grace_generation: Arc::new(AtomicU64::new(0)),
                    restart_count: 0,
                    idle_timeout_override: None,
                },
            );
        }

        // First increment: 0 -> 1
        assert_eq!(manager.increment_restart_count(id).await, Some(1));
        // Second increment: 1 -> 2
        assert_eq!(manager.increment_restart_count(id).await, Some(2));
        // Third increment: 2 -> 3
        assert_eq!(manager.increment_restart_count(id).await, Some(3));
        // Verify via get
        assert_eq!(manager.get_restart_count(id).await, Some(3));
    }

    #[tokio::test]
    async fn increment_restart_count_nonexistent_session() {
        let manager = SessionManager::new(
            100,
            1920,
            1080,
            None,
            beam_protocol::VideoConfig::default(),
            "auto".to_string(),
            "auto".to_string(),
        );
        let id = Uuid::new_v4();
        assert_eq!(manager.increment_restart_count(id).await, None);
    }

    #[tokio::test]
    async fn get_restart_count_nonexistent_session() {
        let manager = SessionManager::new(
            100,
            1920,
            1080,
            None,
            beam_protocol::VideoConfig::default(),
            "auto".to_string(),
            "auto".to_string(),
        );
        let id = Uuid::new_v4();
        assert_eq!(manager.get_restart_count(id).await, None);
    }

    #[tokio::test]
    async fn restart_count_starts_at_zero() {
        let manager = SessionManager::new(
            100,
            1920,
            1080,
            None,
            beam_protocol::VideoConfig::default(),
            "auto".to_string(),
            "auto".to_string(),
        );
        let id = Uuid::new_v4();

        // Insert a session
        {
            let mut sessions = manager.sessions.write().await;
            sessions.insert(
                id,
                ManagedSession {
                    info: SessionInfo {
                        id,
                        username: "test".to_string(),
                        display: 100,
                        width: 1920,
                        height: 1080,
                        created_at: 0,
                    },
                    systemd_unit: None,
                    last_activity: 0,
                    agent_token: "token".to_string(),
                    release_token: "release".to_string(),
                    grace_generation: Arc::new(AtomicU64::new(0)),
                    restart_count: 0,
                    idle_timeout_override: None,
                },
            );
        }

        assert_eq!(manager.get_restart_count(id).await, Some(0));
    }

    #[tokio::test]
    async fn restart_count_independent_per_session() {
        let manager = SessionManager::new(
            100,
            1920,
            1080,
            None,
            beam_protocol::VideoConfig::default(),
            "auto".to_string(),
            "auto".to_string(),
        );
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        // Insert two sessions
        {
            let mut sessions = manager.sessions.write().await;
            for id in [id1, id2] {
                sessions.insert(
                    id,
                    ManagedSession {
                        info: SessionInfo {
                            id,
                            username: "test".to_string(),
                            display: 100,
                            width: 1920,
                            height: 1080,
                            created_at: 0,
                        },
                        systemd_unit: None,
                        last_activity: 0,
                        agent_token: "token".to_string(),
                        release_token: "release".to_string(),
                        grace_generation: Arc::new(AtomicU64::new(0)),
                        restart_count: 0,
                        idle_timeout_override: None,
                    },
                );
            }
        }

        // Increment session 1 twice
        manager.increment_restart_count(id1).await;
        manager.increment_restart_count(id1).await;

        // Increment session 2 once
        manager.increment_restart_count(id2).await;

        assert_eq!(manager.get_restart_count(id1).await, Some(2));
        assert_eq!(manager.get_restart_count(id2).await, Some(1));
    }

    #[tokio::test]
    async fn stale_sessions_uses_per_session_timeout() {
        let manager = SessionManager::new(
            100,
            1920,
            1080,
            None,
            beam_protocol::VideoConfig::default(),
            "auto".to_string(),
            "auto".to_string(),
        );

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let id_short = Uuid::new_v4();
        let id_long = Uuid::new_v4();
        let id_default = Uuid::new_v4();

        {
            let mut sessions = manager.sessions.write().await;

            // Session with short timeout (60s), last active 120s ago → should be stale
            sessions.insert(
                id_short,
                ManagedSession {
                    info: SessionInfo {
                        id: id_short,
                        username: "short".to_string(),
                        display: 100,
                        width: 1920,
                        height: 1080,
                        created_at: 0,
                    },
                    systemd_unit: None,
                    last_activity: now.saturating_sub(120),
                    agent_token: "token".to_string(),
                    release_token: "release".to_string(),
                    grace_generation: Arc::new(AtomicU64::new(0)),
                    restart_count: 0,
                    idle_timeout_override: Some(60),
                },
            );

            // Session with long timeout (86400s), last active 120s ago → should NOT be stale
            sessions.insert(
                id_long,
                ManagedSession {
                    info: SessionInfo {
                        id: id_long,
                        username: "long".to_string(),
                        display: 101,
                        width: 1920,
                        height: 1080,
                        created_at: 0,
                    },
                    systemd_unit: None,
                    last_activity: now.saturating_sub(120),
                    agent_token: "token".to_string(),
                    release_token: "release".to_string(),
                    grace_generation: Arc::new(AtomicU64::new(0)),
                    restart_count: 0,
                    idle_timeout_override: Some(86400),
                },
            );

            // Session with no override, last active 120s ago, global=3600 → should NOT be stale
            sessions.insert(
                id_default,
                ManagedSession {
                    info: SessionInfo {
                        id: id_default,
                        username: "default".to_string(),
                        display: 102,
                        width: 1920,
                        height: 1080,
                        created_at: 0,
                    },
                    systemd_unit: None,
                    last_activity: now.saturating_sub(120),
                    agent_token: "token".to_string(),
                    release_token: "release".to_string(),
                    grace_generation: Arc::new(AtomicU64::new(0)),
                    restart_count: 0,
                    idle_timeout_override: None,
                },
            );
        }

        let stale = manager.stale_sessions(3600).await;
        assert!(
            stale.contains(&id_short),
            "short-timeout session should be stale"
        );
        assert!(
            !stale.contains(&id_long),
            "long-timeout session should NOT be stale"
        );
        assert!(
            !stale.contains(&id_default),
            "default-timeout session should NOT be stale"
        );
    }

    #[tokio::test]
    async fn get_idle_timeout_returns_override_when_set() {
        let manager = SessionManager::new(
            100,
            1920,
            1080,
            None,
            beam_protocol::VideoConfig::default(),
            "auto".to_string(),
            "auto".to_string(),
        );
        let id = Uuid::new_v4();

        {
            let mut sessions = manager.sessions.write().await;
            sessions.insert(
                id,
                ManagedSession {
                    info: SessionInfo {
                        id,
                        username: "test".to_string(),
                        display: 100,
                        width: 1920,
                        height: 1080,
                        created_at: 0,
                    },
                    systemd_unit: None,
                    last_activity: 0,
                    agent_token: "token".to_string(),
                    release_token: "release".to_string(),
                    grace_generation: Arc::new(AtomicU64::new(0)),
                    restart_count: 0,
                    idle_timeout_override: Some(7200),
                },
            );
        }

        assert_eq!(manager.get_idle_timeout(id, 3600).await, 7200);
    }

    #[tokio::test]
    async fn get_idle_timeout_returns_global_when_no_override() {
        let manager = SessionManager::new(
            100,
            1920,
            1080,
            None,
            beam_protocol::VideoConfig::default(),
            "auto".to_string(),
            "auto".to_string(),
        );
        let id = Uuid::new_v4();

        {
            let mut sessions = manager.sessions.write().await;
            sessions.insert(
                id,
                ManagedSession {
                    info: SessionInfo {
                        id,
                        username: "test".to_string(),
                        display: 100,
                        width: 1920,
                        height: 1080,
                        created_at: 0,
                    },
                    systemd_unit: None,
                    last_activity: 0,
                    agent_token: "token".to_string(),
                    release_token: "release".to_string(),
                    grace_generation: Arc::new(AtomicU64::new(0)),
                    restart_count: 0,
                    idle_timeout_override: None,
                },
            );
        }

        assert_eq!(manager.get_idle_timeout(id, 3600).await, 3600);
    }

    #[tokio::test]
    async fn get_idle_timeout_nonexistent_returns_global() {
        let manager = SessionManager::new(
            100,
            1920,
            1080,
            None,
            beam_protocol::VideoConfig::default(),
            "auto".to_string(),
            "auto".to_string(),
        );
        let id = Uuid::new_v4();
        assert_eq!(manager.get_idle_timeout(id, 3600).await, 3600);
    }
}
