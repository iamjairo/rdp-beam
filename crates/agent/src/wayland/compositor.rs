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
    /// Per-session `dbus-daemon --session` bound to `$XDG_RUNTIME_DIR/bus`.
    /// Required by `xdg-desktop-portal-wlr`, which only exposes a
    /// session-bus interface — we need an isolated bus per agent session
    /// since the agent's compositor and portal must not be reachable from
    /// the user's main desktop session bus.
    bus_child: Child,
    /// `xdg-desktop-portal-wlr` backend bound to `bus_child`. Owns
    /// `org.freedesktop.impl.portal.desktop.wlr` on the session bus.
    portal_backend_child: Child,
    /// `xdg-desktop-portal` umbrella daemon. Owns the user-facing
    /// `org.freedesktop.portal.Desktop` name and routes ScreenCast
    /// requests to the wlr backend.
    portal_child: Child,
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

        // xdg-desktop-portal-wlr's screencast path requires a PipeWire
        // daemon socket inside XDG_RUNTIME_DIR (the client searches for
        // `$XDG_RUNTIME_DIR/pipewire-0`). Per-session PipeWire is the
        // strictly-correct architecture for production multi-tenant hosts
        // but is significant additional plumbing. As a pragmatic bridge,
        // symlink the agent user's existing PipeWire socket into our
        // isolated runtime dir. The dev-host smoke test uses this path.
        #[cfg(unix)]
        if let Some(uid) = real_uid() {
            let user_pw_sock = PathBuf::from(format!("/run/user/{uid}/pipewire-0"));
            if user_pw_sock.exists() {
                let target = xdg_runtime_dir.join("pipewire-0");
                let _ = std::fs::remove_file(&target);
                if let Err(e) = std::os::unix::fs::symlink(&user_pw_sock, &target) {
                    warn!(
                        from = %user_pw_sock.display(),
                        to = %target.display(),
                        error = %e,
                        "failed to symlink pipewire socket; portal screencast will fail"
                    );
                } else {
                    info!(
                        from = %user_pw_sock.display(),
                        to = %target.display(),
                        "linked user's pipewire socket into session runtime dir"
                    );
                }
            } else {
                warn!(
                    user_pw_sock = %user_pw_sock.display(),
                    "user PipeWire socket missing; portal screencast will fail until per-session pipewire lands"
                );
            }
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
            // Force wlroots into a headless, software-rendered backend.
            // Without these, cage/sway try DRM/X11 backends that don't
            // exist on a headless server box and exit ~immediately.
            .env("WLR_BACKENDS", "headless")
            .env("WLR_RENDERER", "pixman")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `{compositor_choice}`"))?;

        // Wait for any `wayland-*` socket to appear in XDG_RUNTIME_DIR.
        // Cage 0.2.x ignores the WAYLAND_DISPLAY env we set and picks its
        // own name (wayland-0, wayland-1, …) — so we discover the actual
        // socket name post-spawn rather than pre-deciding it.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut compositor_child = child;

        let discovered_socket = loop {
            match scan_for_wayland_socket(&xdg_runtime_dir) {
                Ok(Some(name)) => break Some(name),
                Ok(None) => {}
                Err(e) => warn!(error = %e, "scan_for_wayland_socket failed; will retry"),
            }
            match compositor_child.try_wait() {
                Ok(Some(status)) => {
                    anyhow::bail!(
                        "compositor `{compositor_choice}` exited early with status {status} before any wayland-* socket appeared in {}",
                        xdg_runtime_dir.display()
                    );
                }
                Ok(None) => {}
                Err(e) => warn!(error = %e, "try_wait on compositor child failed"),
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(100));
        };

        let wayland_display = match discovered_socket {
            Some(s) => s,
            None => {
                let _ = compositor_child.kill();
                let _ = compositor_child.wait();
                anyhow::bail!(
                    "compositor `{compositor_choice}` did not create a wayland-* socket in {} within 5s",
                    xdg_runtime_dir.display()
                );
            }
        };
        let socket_path = xdg_runtime_dir.join(&wayland_display);
        info!(socket = %socket_path.display(), "Wayland socket ready");

        // -- Per-session D-Bus daemon ----------------------------------------
        // `xdg-desktop-portal-wlr` requires a session bus, and we want it
        // isolated from the operator's desktop session bus. Bind dbus-daemon
        // to a unix socket inside the same XDG_RUNTIME_DIR so the portal and
        // gdbus calls from `WaylandCapture` find it the same way.
        let bus_socket = xdg_runtime_dir.join("bus");
        let bus_addr = format!("unix:path={}", bus_socket.display());
        let mut bus_cmd = Command::new("dbus-daemon");
        bus_cmd
            .args([
                "--session",
                "--nofork",
                "--nopidfile",
                "--nosyslog",
                &format!("--address={bus_addr}"),
            ])
            .env("XDG_RUNTIME_DIR", &xdg_runtime_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let bus_child = match bus_cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = compositor_child.kill();
                let _ = compositor_child.wait();
                return Err(anyhow::Error::from(e).context(
                    "failed to spawn dbus-daemon for Wayland session (install `dbus-daemon`)",
                ));
            }
        };

        if let Err(e) = wait_for_socket(&bus_socket, Duration::from_secs(3)) {
            let _ = compositor_child.kill();
            let _ = compositor_child.wait();
            let mut bc = bus_child;
            let _ = bc.kill();
            let _ = bc.wait();
            return Err(e.context("dbus-daemon did not create session bus socket"));
        }
        info!(bus = %bus_socket.display(), "Per-session dbus-daemon ready");

        // -- xdg-desktop-portal-wlr backend ----------------------------------
        // The wlr binary is a *backend*: it owns
        // `org.freedesktop.impl.portal.desktop.wlr`, not the user-facing
        // `org.freedesktop.portal.Desktop` name. It proxies wlr-screencopy
        // to a PipeWire node. Distro packaging installs it under
        // /usr/libexec, so it isn't in $PATH — use the absolute path.
        let portal_env: [(&str, &std::ffi::OsStr); 5] = [
            ("WAYLAND_DISPLAY", std::ffi::OsStr::new(&wayland_display)),
            ("XDG_RUNTIME_DIR", xdg_runtime_dir.as_os_str()),
            ("XDG_SESSION_TYPE", std::ffi::OsStr::new("wayland")),
            ("XDG_CURRENT_DESKTOP", std::ffi::OsStr::new("wlroots")),
            ("DBUS_SESSION_BUS_ADDRESS", std::ffi::OsStr::new(&bus_addr)),
        ];
        let mut backend_cmd = Command::new("/usr/libexec/xdg-desktop-portal-wlr");
        for (k, v) in &portal_env {
            backend_cmd.env(k, v);
        }
        backend_cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(
                std::fs::File::create(format!("/tmp/beam-portal-wlr-{display_num}.log"))
                    .map(Stdio::from)
                    .unwrap_or_else(|_| Stdio::null()),
            );
        let portal_backend_child = match backend_cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = compositor_child.kill();
                let _ = compositor_child.wait();
                let mut bc = bus_child;
                let _ = bc.kill();
                let _ = bc.wait();
                return Err(anyhow::Error::from(e).context(
                    "failed to spawn /usr/libexec/xdg-desktop-portal-wlr (install `xdg-desktop-portal-wlr`)",
                ));
            }
        };

        // -- xdg-desktop-portal umbrella -------------------------------------
        // Owns `org.freedesktop.portal.Desktop` and routes ScreenCast to
        // the wlr backend above. Reads $XDG_CURRENT_DESKTOP to decide.
        let mut umbrella_cmd = Command::new("/usr/libexec/xdg-desktop-portal");
        for (k, v) in &portal_env {
            umbrella_cmd.env(k, v);
        }
        umbrella_cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(
                std::fs::File::create(format!("/tmp/beam-portal-umbrella-{display_num}.log"))
                    .map(Stdio::from)
                    .unwrap_or_else(|_| Stdio::null()),
            );
        let portal_child = match umbrella_cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = compositor_child.kill();
                let _ = compositor_child.wait();
                let mut bc = bus_child;
                let _ = bc.kill();
                let _ = bc.wait();
                let mut bk = portal_backend_child;
                let _ = bk.kill();
                let _ = bk.wait();
                return Err(anyhow::Error::from(e).context(
                    "failed to spawn /usr/libexec/xdg-desktop-portal (install `xdg-desktop-portal`)",
                ));
            }
        };

        if let Err(e) = wait_for_portal(&bus_addr, &xdg_runtime_dir, Duration::from_secs(5)) {
            let _ = compositor_child.kill();
            let _ = compositor_child.wait();
            let mut bc = bus_child;
            let _ = bc.kill();
            let _ = bc.wait();
            let mut bk = portal_backend_child;
            let _ = bk.kill();
            let _ = bk.wait();
            let mut pc = portal_child;
            let _ = pc.kill();
            let _ = pc.wait();
            return Err(e.context("xdg-desktop-portal did not register on session bus"));
        }
        info!("xdg-desktop-portal registered on session bus");

        Ok(WaylandDisplay {
            display_num,
            compositor_child,
            bus_child,
            portal_backend_child,
            portal_child,
            desktop_child: None,
            pulse_child: None,
            xdg_runtime_dir,
            wayland_display,
            output_name,
        })
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
        stop(&mut self.portal_child, "wayland-portal", self.display_num);
        stop(
            &mut self.portal_backend_child,
            "wayland-portal-wlr",
            self.display_num,
        );
        stop(&mut self.bus_child, "wayland-dbus", self.display_num);
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

/// Real uid for the agent process. We only call this on Unix; libc returns
/// the kernel uid directly. Used to locate the user's PipeWire socket.
#[cfg(unix)]
fn real_uid() -> Option<u32> {
    // SAFETY: getuid() is signal-safe and always succeeds.
    Some(unsafe { libc::getuid() })
}

/// Find a `wayland-*` socket inside `dir`. Returns the socket file name
/// (e.g. `"wayland-0"`) or `Ok(None)` if none has appeared yet.
fn scan_for_wayland_socket(dir: &std::path::Path) -> anyhow::Result<Option<String>> {
    use std::os::unix::fs::FileTypeExt;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("wayland-") || name_str.ends_with(".lock") {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && meta.file_type().is_socket()
        {
            return Ok(Some(name_str.into_owned()));
        }
    }
    Ok(None)
}

/// Poll for a unix socket path to appear, up to `timeout`.
fn wait_for_socket(path: &std::path::Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!(
        "socket {} did not appear within {:?}",
        path.display(),
        timeout
    )
}

/// Probe the session bus for the xdg-desktop-portal name, up to `timeout`.
///
/// Uses `gdbus introspect` against the portal object path. Once the portal
/// has registered its bus name, introspection succeeds immediately. We
/// don't actually need the introspection output — only the exit code.
fn wait_for_portal(
    bus_addr: &str,
    xdg_runtime_dir: &std::path::Path,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let status = Command::new("gdbus")
            .args([
                "introspect",
                "--session",
                "--dest=org.freedesktop.portal.Desktop",
                "--object-path=/org/freedesktop/portal/desktop",
            ])
            .env("DBUS_SESSION_BUS_ADDRESS", bus_addr)
            .env("XDG_RUNTIME_DIR", xdg_runtime_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Ok(s) = status
            && s.success()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("org.freedesktop.portal.Desktop did not appear on {bus_addr} within {timeout:?}")
}
