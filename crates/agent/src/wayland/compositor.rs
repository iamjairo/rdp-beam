//! Headless wlroots compositor lifecycle.
//!
//! Spawns `cage` (or `sway --backend=headless` as fallback) per session under
//! a private `WAYLAND_DISPLAY=wayland-beam-{display_num}` and
//! `XDG_RUNTIME_DIR=/tmp/beam-xdg-{display_num}` (mode 0700, owned by the
//! agent uid). Waits for the socket to appear, then exposes the same
//! `DisplayBackend` surface as `crate::display::XorgVirtualDisplay`.
//!
//! ## Compositor choice
//!
//! Prefers `cage` because it's smaller and bundled in Ubuntu 24.04+ archives,
//! but cage is a kiosk-style single-window compositor: it exits as soon as
//! the wrapped command terminates. So we run `cage -- /usr/bin/env sleep
//! infinity` to keep it alive while clients connect. If `cage` is absent or
//! ships a build without `wlr-virtual-pointer-v1` (some packaged versions
//! historically disabled the wlroots-virtual-* extensions), the caller can
//! re-run with `BEAM_WAYLAND_COMPOSITOR=sway` to force the heavier fallback.
//!
//! `start_desktop()` mirrors the Xorg path: it launches the same XDG-aware
//! desktop session (`startxfce4` etc.) but with `WAYLAND_DISPLAY` set so
//! apps prefer the Wayland transport. On a wlroots-only compositor, X11
//! apps need `Xwayland` running — we leave that to the host (Ubuntu 26.04
//! installs Xwayland by default with the desktop metapackage).
//!
//! `start_pulseaudio()` reuses the same per-display PulseAudio spawn the
//! Xorg path uses. The audio capture path (Xorg: libpulse, Wayland: pipewire
//! native via packet A5) doesn't care which.
//!
//! ## Drop semantics
//!
//! `Drop` runs in this order: kill desktop child → kill compositor child
//! (SIGTERM + 2s grace + SIGKILL) → remove the private XDG_RUNTIME_DIR.
//! No orphan compositor processes after the agent exits.

use crate::display::DisplayBackend;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Configuration handed to a Wayland display when it starts.
///
/// Read by `WaylandDisplay::start` to compose env vars and the output extent.
/// `width` and `height` are advisory — wlroots compositors negotiate output
/// size with clients via `wlr-output-management`; the values are logged at
/// startup and consulted later by `WaylandCapture` to set up the pipewiresrc
/// caps. They're stored on `WaylandDisplay` itself so the capture path can
/// retrieve them without re-reading config.
pub struct WaylandDisplayConfig {
    pub display_num: u32,
    pub width: u32,
    pub height: u32,
}

/// Runtime handle to a headless Wayland session.
///
/// Owns the compositor child process and any session-scoped child processes
/// (desktop session, PulseAudio). Drop tears them all down and removes the
/// per-session XDG_RUNTIME_DIR.
pub struct WaylandDisplay {
    display_num: u32,
    /// Compositor child (`cage` or `sway --backend=headless`).
    compositor_child: Child,
    /// Optional desktop session spawned by `start_desktop()`.
    desktop_child: Option<Child>,
    /// Optional PulseAudio child spawned by `start_pulseaudio()`.
    pulse_child: Option<Child>,
    /// Per-session XDG_RUNTIME_DIR — owned by the agent uid, removed on drop.
    xdg_runtime_dir: PathBuf,
    /// The Wayland socket name, e.g. `"wayland-beam-10"`.
    wayland_display: String,
    /// Reported xrandr-equivalent output name for resize ops. Wayland doesn't
    /// expose xrandr; we synthesize a stable string from the display num so
    /// the existing resize code in `main.rs` has something to log.
    output_name: String,
}

impl WaylandDisplay {
    /// The Wayland socket name, e.g. `"wayland-beam-10"`. Passed to
    /// `WaylandCapture::new` and `WaylandInput::new`.
    pub fn wayland_display(&self) -> &str {
        &self.wayland_display
    }

    /// The private XDG_RUNTIME_DIR for this session. Passed to
    /// `WaylandCapture::new` and `WaylandAudio::new`.
    pub fn xdg_runtime_dir(&self) -> &str {
        self.xdg_runtime_dir.to_str().unwrap_or("")
    }

    /// Spawn a headless wlroots compositor.
    ///
    /// Blocks up to 5 seconds waiting for the Wayland socket to appear.
    /// On failure, the compositor child is killed before returning the Err.
    pub fn start(config: WaylandDisplayConfig) -> Result<Self> {
        let display_num = config.display_num;
        let width = config.width;
        let height = config.height;
        let wayland_display = format!("wayland-beam-{display_num}");
        let xdg_runtime_dir = PathBuf::from(format!("/tmp/beam-xdg-{display_num}"));
        let output_name = format!("WAYLAND-{display_num}");

        // Create the private XDG_RUNTIME_DIR with strict perms. Wayland's
        // libwayland refuses to bind a socket in a world-writable directory.
        std::fs::create_dir_all(&xdg_runtime_dir).with_context(|| {
            format!(
                "failed to create XDG_RUNTIME_DIR {}",
                xdg_runtime_dir.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&xdg_runtime_dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| {
                    format!(
                        "failed to chmod 0700 XDG_RUNTIME_DIR {}",
                        xdg_runtime_dir.display()
                    )
                })?;
        }

        // Resolve compositor binary. Default `cage`; override via env for ops.
        let compositor_choice =
            std::env::var("BEAM_WAYLAND_COMPOSITOR").unwrap_or_else(|_| "cage".into());

        info!(
            display_num,
            width,
            height,
            wayland_display = %wayland_display,
            xdg_runtime_dir = %xdg_runtime_dir.display(),
            compositor = %compositor_choice,
            "Spawning headless Wayland compositor"
        );

        let mut cmd = match compositor_choice.as_str() {
            "cage" => {
                let mut c = Command::new("cage");
                // Cage exits when its wrapped command exits. Run sleep so the
                // compositor stays alive while beam-agent owns the session.
                c.args(["--", "/usr/bin/env", "sleep", "infinity"]);
                c
            }
            "sway" => {
                let mut c = Command::new("sway");
                c.args(["--unsupported-gpu", "--config", "/dev/null"]);
                c.env("WLR_BACKENDS", "headless");
                c.env("WLR_LIBINPUT_NO_DEVICES", "1");
                c
            }
            other => anyhow::bail!(
                "BEAM_WAYLAND_COMPOSITOR={other} not supported; expected `cage` or `sway`"
            ),
        };

        cmd.env("WAYLAND_DISPLAY", &wayland_display)
            .env("XDG_RUNTIME_DIR", &xdg_runtime_dir)
            .env("XDG_SESSION_TYPE", "wayland")
            // Some compositor builds spam stderr with informational logs;
            // route to /dev/null but keep stdout for diagnostic info if the
            // child writes anything fatal.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `{compositor_choice}`"))?;

        // Wait for the socket to appear. Up to 5 seconds.
        let socket_path = xdg_runtime_dir.join(&wayland_display);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut display = WaylandDisplay {
            display_num,
            compositor_child: child,
            desktop_child: None,
            pulse_child: None,
            xdg_runtime_dir: xdg_runtime_dir.clone(),
            wayland_display,
            output_name,
        };

        while Instant::now() < deadline {
            if socket_path.exists() {
                info!(
                    socket = %socket_path.display(),
                    "Wayland socket ready"
                );
                return Ok(display);
            }
            // Detect early compositor death so we don't spin for 5s.
            match display.compositor_child.try_wait() {
                Ok(Some(status)) => {
                    anyhow::bail!(
                        "compositor `{compositor_choice}` exited early with status {status} before the socket appeared at {}",
                        socket_path.display()
                    );
                }
                Ok(None) => {} // still running
                Err(e) => warn!(error = %e, "try_wait on compositor child failed"),
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // Timed out — kill the child before returning Err so we don't leak it.
        let _ = display.compositor_child.kill();
        let _ = display.compositor_child.wait();
        anyhow::bail!(
            "compositor `{compositor_choice}` did not create Wayland socket {} within 5s",
            socket_path.display()
        );
    }

    /// Reusable env builder so `start_desktop` / `start_pulseaudio` use the
    /// same env every time.
    fn session_env(&self) -> [(&'static str, String); 3] {
        [
            ("WAYLAND_DISPLAY", self.wayland_display.clone()),
            (
                "XDG_RUNTIME_DIR",
                self.xdg_runtime_dir.to_string_lossy().into_owned(),
            ),
            ("XDG_SESSION_TYPE", "wayland".to_string()),
        ]
    }
}

impl DisplayBackend for WaylandDisplay {
    fn output_name(&self) -> &str {
        &self.output_name
    }

    fn start_desktop(&mut self) -> Result<()> {
        if self.desktop_child.is_some() {
            return Ok(());
        }
        info!(
            display = self.display_num,
            "Starting desktop session on Wayland compositor"
        );
        // We launch the same xfce4 session the Xorg path uses; xfce4 runs
        // happily on Wayland via Xwayland for its X11 components, and the
        // panel + window manager themselves are X11-native, so the bulk of
        // the session lives in an Xwayland subwindow inside cage. This
        // matches what `apt install beam` already pulls in. For a pure
        // Wayland desktop (sway tiling, etc.), set BEAM_WAYLAND_DESKTOP and
        // we'll honour it.
        let cmd_str =
            std::env::var("BEAM_WAYLAND_DESKTOP").unwrap_or_else(|_| "startxfce4".to_string());
        let parts: Vec<&str> = cmd_str.split_whitespace().collect();
        let (cmd, args) = parts
            .split_first()
            .context("BEAM_WAYLAND_DESKTOP is empty")?;
        let mut c = Command::new(cmd);
        c.args(args);
        for (k, v) in self.session_env() {
            c.env(k, v);
        }
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = c
            .spawn()
            .with_context(|| format!("failed to spawn desktop session `{cmd}`"))?;
        self.desktop_child = Some(child);
        Ok(())
    }

    fn start_pulseaudio(&mut self) -> Result<()> {
        if self.pulse_child.is_some() {
            return Ok(());
        }
        // Same shape as the Xorg path: run a private PulseAudio bound to
        // this session's XDG_RUNTIME_DIR so the audio capture path
        // (libpulse for v1; pipewire native once A5 lands) finds the
        // expected `pulse/native` socket.
        let mut c = Command::new("pulseaudio");
        c.args(["--start", "--exit-idle-time=-1"]);
        for (k, v) in self.session_env() {
            c.env(k, v);
        }
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = c
            .spawn()
            .context("failed to spawn pulseaudio for Wayland session")?;
        self.pulse_child = Some(child);
        Ok(())
    }

    fn hide_cursor(&mut self) {
        // wlroots compositors manage their own cursor; nothing to do here.
        // The Xorg path uses `unclutter` which has no Wayland equivalent —
        // wayland compositors hide the cursor automatically when no input
        // device has reported motion recently.
    }
}

impl Drop for WaylandDisplay {
    fn drop(&mut self) {
        /// SIGTERM, wait up to 2s, then SIGKILL. Same pattern as
        /// `display.rs::stop_child`.
        fn stop(child: &mut Child, name: &str, display_num: u32) {
            #[cfg(unix)]
            unsafe {
                let pid = child.id() as libc::pid_t;
                if pid > 0 {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            warn!(
                                child = name,
                                display_num, "SIGTERM grace expired; sending SIGKILL"
                            );
                            let _ = child.kill();
                            let _ = child.wait();
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => {
                        warn!(child = name, error = %e, "try_wait failed");
                        return;
                    }
                }
            }
        }

        if let Some(mut c) = self.desktop_child.take() {
            stop(&mut c, "wayland-desktop", self.display_num);
        }
        if let Some(mut c) = self.pulse_child.take() {
            stop(&mut c, "wayland-pulseaudio", self.display_num);
        }
        stop(
            &mut self.compositor_child,
            "wayland-compositor",
            self.display_num,
        );

        // Best-effort cleanup of the XDG_RUNTIME_DIR. Failure here is not
        // fatal — leaving the directory around is mildly untidy but harmless.
        if let Err(e) = std::fs::remove_dir_all(&self.xdg_runtime_dir) {
            warn!(
                xdg_runtime_dir = %self.xdg_runtime_dir.display(),
                error = %e,
                "failed to remove per-session XDG_RUNTIME_DIR"
            );
        }
    }
}
