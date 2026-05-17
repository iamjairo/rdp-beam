use anyhow::{Context, Result, bail};
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use tracing::{debug, info, warn};

/// Abstraction over virtual-display lifecycle.
///
/// A `DisplayBackend` owns the compositor / Xorg process and exposes the
/// operations that `main.rs` uses to stand up a session.  The Wayland
/// backend (behind `#[cfg(feature = "wayland")]`) stubs this trait and
/// bails at runtime until the cage compositor path is wired up.
// See capture::ScreenCaptureBackend for why `dead_code` is allowed here.
#[allow(dead_code)]
pub trait DisplayBackend: Send {
    /// xrandr (Xorg) or Wayland output name used for resize operations.
    fn output_name(&self) -> &str;
    /// Launch a desktop environment on top of the display.
    fn start_desktop(&mut self) -> Result<()>;
    /// Start an audio daemon bound to this session.
    fn start_pulseaudio(&mut self) -> Result<()>;
    /// Hide the hardware cursor so the browser-rendered cursor is the only
    /// visible one.
    fn hide_cursor(&mut self);
}

/// Minimal PulseAudio config for virtual desktop sessions.
/// Creates a null sink (virtual audio output) with a monitor source
/// that the agent can capture from.
/// Generate a PulseAudio config that binds to a display-specific socket path.
/// This avoids conflicts with any existing user-level PulseAudio instance.
fn pa_config(runtime_dir: &str) -> String {
    format!(
        "\
load-module module-null-sink sink_name=beam sink_properties=device.description=Beam
set-default-sink beam
load-module module-native-protocol-unix socket={runtime_dir}/native auth-anonymous=1
load-module module-always-sink
"
    )
}

/// Manages a virtual X display using either the dummy or nvidia video driver.
pub struct XorgVirtualDisplay {
    display_num: u32,
    /// xrandr output name (e.g. "DUMMY0" for dummy driver, "DFP-1" for nvidia)
    output_name: String,
    xorg_child: Option<Child>,
    desktop_child: Option<Child>,
    pulse_child: Option<Child>,
    cursor_child: Option<Child>,
    /// Temp config path to clean up on drop (None for package-installed static config)
    cleanup_config: Option<String>,
    /// Temp EDID file to clean up on drop (nvidia only)
    cleanup_edid: Option<String>,
}

impl XorgVirtualDisplay {
    /// Create and start a new virtual X display on the given display number.
    ///
    /// `gpu_driver` controls the Xorg driver: "auto" (detect), "nvidia" (force), "dummy" (force).
    /// `display_start` is needed for multi-GPU DFP output allocation.
    pub fn start(
        display_num: u32,
        width: u32,
        height: u32,
        gpu_driver: &str,
        display_start: u32,
    ) -> Result<Self> {
        let gpu_config = crate::gpu::detect_gpu(gpu_driver, display_num, display_start);

        let (config_path, cleanup_edid) = if gpu_config.driver == "nvidia" {
            // NVIDIA: generate config dynamically (needs BusID, DFP, EDID path).
            // Config and EDID must be in /etc/X11/beam/ so the Xorg setuid wrapper
            // can use them (elevated privileges require configs in /etc/X11/).
            let beam_conf_dir = "/etc/X11/beam";
            let _ = fs::create_dir_all(beam_conf_dir);

            let bus_id = gpu_config.bus_id.as_deref().unwrap_or("PCI:0:0:0");
            let dfp_output = gpu_config.dfp_output.as_deref().unwrap_or("DFP-1");
            let edid_path = format!("{beam_conf_dir}/beam-edid-{display_num}.bin");
            crate::gpu::write_edid_file_to(&edid_path)?;
            let config = generate_nvidia_xorg_config(bus_id, dfp_output, &edid_path);
            let config_path = format!("{beam_conf_dir}/beam-xorg-{display_num}.conf");
            let _ = fs::remove_file(&config_path);
            fs::write(&config_path, &config)
                .with_context(|| format!("Failed to write nvidia Xorg config to {config_path}"))?;
            info!(
                bus_id,
                dfp_output, config_path, "Using NVIDIA GPU driver for display :{display_num}"
            );
            (config_path, Some(edid_path))
        } else {
            // Dummy driver: use static package config or generate temp config
            let static_config = String::from("/etc/X11/beam-xorg.conf");
            if std::path::Path::new(&static_config).exists() {
                (static_config, None)
            } else {
                let tmp_config_path = format!("/tmp/beam-xorg-{display_num}.conf");
                let _ = fs::remove_file(&tmp_config_path);
                let config = generate_xorg_config(width, height);
                fs::write(&tmp_config_path, &config)
                    .with_context(|| format!("Failed to write Xorg config to {tmp_config_path}"))?;
                (tmp_config_path, None)
            }
        };

        Self::start_with_config(display_num, width, height, config_path, cleanup_edid)
    }

    fn start_with_config(
        display_num: u32,
        width: u32,
        height: u32,
        config_path: String,
        cleanup_edid: Option<String>,
    ) -> Result<Self> {
        let display_str = format!(":{display_num}");

        // Determine how to invoke Xorg based on config location.
        // Package installs: config in /etc/X11/, use Xorg wrapper (setuid) with
        // relative path. Xwrapper.config has allowed_users=anybody +
        // needs_root_rights=yes so Xorg can access /dev/tty0 for VT management.
        // Dev/source installs: config in /tmp, use Xorg binary directly with
        // absolute path (no elevated privilege restrictions).
        let (xorg_bin, config_arg): (&str, &str) = if config_path.starts_with("/etc/X11/") {
            // Relative path required when Xorg runs with elevated privileges.
            // Strip the /etc/X11/ prefix to get the relative path (e.g.
            // "/etc/X11/beam-xorg.conf" -> "beam-xorg.conf", or
            // "/etc/X11/beam/beam-xorg-20.conf" -> "beam/beam-xorg-20.conf").
            let relative = config_path
                .strip_prefix("/etc/X11/")
                .unwrap_or(&config_path);
            ("Xorg", relative)
        } else {
            // Dev mode: use direct binary with absolute path
            if std::path::Path::new("/usr/lib/xorg/Xorg").exists() {
                ("/usr/lib/xorg/Xorg", config_path.as_str())
            } else {
                ("Xorg", config_path.as_str())
            }
        };

        // Need to own the config_arg string for the lifetime of the Command
        let config_arg_owned = config_arg.to_string();

        // Capture Xorg stderr to diagnose startup failures
        let xorg_log_path = format!("/tmp/beam-xorg-stderr-{display_num}.log");
        let xorg_log = std::fs::File::create(&xorg_log_path).ok();

        let mut child = Command::new(xorg_bin)
            .arg(&display_str)
            .arg("-config")
            .arg(&config_arg_owned)
            .arg("-noreset")
            .arg("-novtswitch")
            .arg("-nolisten")
            .arg("tcp")
            .stdout(Stdio::null())
            .stderr(xorg_log.map(Stdio::from).unwrap_or_else(Stdio::null))
            .spawn()
            .with_context(|| format!("Failed to start Xorg on {display_str}"))?;

        let pid = child.id();
        info!(display = display_num, pid, "Virtual X display started");

        // Wait briefly for Xorg to initialize
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Verify the display is running (check if process exited early)
        match child.try_wait() {
            Ok(Some(status)) => {
                // Read Xorg stderr for diagnosis
                if let Ok(stderr) = fs::read_to_string(&xorg_log_path)
                    && !stderr.is_empty()
                {
                    tracing::error!("Xorg stderr output:\n{stderr}");
                }
                bail!("Xorg exited immediately with status: {status} on :{display_num}");
            }
            Ok(None) => {} // still running, good
            Err(e) => {
                warn!("Could not check Xorg status: {e}");
            }
        }

        if !is_display_running(display_num) {
            bail!("Xorg failed to start on :{display_num}");
        }

        // Detect the xrandr output name (e.g. "DUMMY0" or "DFP-1")
        let output_name = detect_xrandr_output(&display_str);
        info!(display = display_num, output_name, "Detected xrandr output");

        // When using the static package config (no per-session modeline),
        // set the requested resolution via xrandr after Xorg starts.
        if config_path == "/etc/X11/beam-xorg.conf"
            && let Err(e) = set_display_resolution(&display_str, width, height, &output_name)
        {
            warn!("Failed to set initial resolution {width}x{height}: {e}");
        }

        // Ensure the X server uses `evdev` XKB rules. The agent injects keys
        // via XTEST using evdev scancodes + 8, which only produces correct
        // keysyms under the `evdev` ruleset. Some distros default to `base`
        // rules where the keycode→keysym mapping differs (e.g., keycode 111
        // = Print instead of Up), causing incorrect key injection.
        let _ = Command::new("setxkbmap")
            .env("DISPLAY", &display_str)
            .args(["-rules", "evdev", "-model", "pc105", "-layout", "us"])
            .output();

        // Only delete temp configs on drop, not the static package config
        let cleanup_config = if config_path.starts_with("/tmp/") {
            Some(config_path)
        } else {
            None
        };

        Ok(Self {
            display_num,
            output_name,
            xorg_child: Some(child),
            desktop_child: None,
            pulse_child: None,
            cursor_child: None,
            cleanup_config,
            cleanup_edid,
        })
    }

    /// Get the xrandr output name (e.g. "DUMMY0" or "DFP-1").
    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    /// Change the resolution of the virtual display using xrandr.
    #[allow(dead_code)]
    pub fn set_resolution(&self, width: u32, height: u32) -> Result<()> {
        set_display_resolution(
            &format!(":{}", self.display_num),
            width,
            height,
            &self.output_name,
        )
    }

    /// Start a desktop environment on this display.
    /// Prefers XFCE4 for a full desktop experience. Disables the xfwm4
    /// compositor to minimize latency for remote desktop streaming.
    /// Falls back to openbox (lightweight WM) if XFCE4 is unavailable.
    pub fn start_desktop(&mut self) -> Result<()> {
        let display = format!(":{}", self.display_num);

        // Prefer XFCE4: full desktop with panels, file manager, app menu.
        if which_exists("xfce4-session") {
            let (xfce_config_dir, is_first_session) = ensure_persistent_config(self.display_num);

            // Detect default browser/terminal (every session, needed for env vars).
            let detected_browser = find_non_snap_app(&[
                "firefox-esr",
                "google-chrome-stable",
                "google-chrome",
                "chromium-browser",
                "firefox",
                "chromium",
                "epiphany-browser",
            ]);
            let detected_terminal =
                find_non_snap_app(&["xfce4-terminal", "gnome-terminal", "xterm"]);

            if let Some(browser) = detected_browser {
                info!(browser, "Default browser");
            } else {
                warn!(
                    "No non-snap browser found. Install a .deb browser: \
                     sudo apt install epiphany-browser"
                );
            }
            if let Some(term) = detected_terminal {
                info!(term, "Default terminal");
            }

            // Create XDG_RUNTIME_DIR for this session. Without it, D-Bus services,
            // GVFS, and PulseAudio can't find proper socket paths. Normally created
            // by logind for interactive sessions, but beam-agent is spawned by the
            // beam-server systemd service (not a PAM login session).
            let runtime_dir = format!("/tmp/beam-run-{}", self.display_num);
            let _ = fs::remove_dir_all(&runtime_dir);
            fs::create_dir_all(&runtime_dir)
                .with_context(|| format!("Failed to create runtime dir: {runtime_dir}"))?;
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700));
            }

            let pulse_server = format!("unix:/tmp/beam-pulse-{}/native", self.display_num);
            let mut cmd = Command::new("/usr/bin/dbus-launch");
            cmd.arg("--exit-with-session")
                .arg("xfce4-session")
                .env("DISPLAY", &display)
                .env("PULSE_SERVER", &pulse_server)
                .env("XDG_CONFIG_HOME", &xfce_config_dir)
                .env("XDG_RUNTIME_DIR", &runtime_dir)
                .env("XDG_CURRENT_DESKTOP", "XFCE")
                .env("XDG_SESSION_DESKTOP", "xfce")
                .env("GVFS_DISABLE_FUSE", "1");

            // Set env vars as universal fallback for apps that check directly.
            if let Some(browser) = detected_browser {
                cmd.env("BROWSER", browser);
            }
            if let Some(term) = detected_terminal {
                cmd.env("TERMINAL", term);
            }

            let child = unsafe {
                cmd.stdout(Stdio::null())
                    .stderr(Stdio::null())
                    // Create a new session (process group) so we can kill all
                    // grandchildren (xfwm4, xfce4-panel, etc.) on cleanup.
                    .pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    })
                    .spawn()
                    .context("Failed to start XFCE4 desktop via dbus-launch")?
            };

            info!(
                display = self.display_num,
                pid = child.id(),
                "XFCE4 desktop started"
            );

            self.desktop_child = Some(child);

            // Background thread: start gnome-keyring on the session bus, and
            // on first session apply xfconf settings (compositor off, theme, etc.).
            // Subsequent sessions reuse persistent config from ~/.local/share/beam/.
            let display_for_xfconf = display.clone();
            std::thread::spawn(move || {
                // Poll for xfce4-panel to start (it needs xfwm4, xfdesktop first).
                // On fresh sessions with PAM/logind setup, XFCE can take 5-10s.
                let mut dbus_addr = None;
                for attempt in 1..=15 {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    dbus_addr = find_dbus_address_for_display(&display_for_xfconf);
                    if dbus_addr.is_some() {
                        debug!("Found DBUS session bus after {attempt}s");
                        break;
                    }
                }
                if dbus_addr.is_none() {
                    warn!(
                        "Could not find DBUS session bus after 15s, xfconf settings may not apply"
                    );
                }

                // Start gnome-keyring-daemon inside the D-Bus session so it
                // registers as org.freedesktop.secrets on the session bus.
                // VS Code and other apps use libsecret to talk to this service.
                //
                // Must use --foreground + separate --control-directory because
                // --start discovers the HOST's existing daemon via the shared
                // /run/user/ control socket and reuses it (which is on a
                // different D-Bus). A fresh daemon with its own control dir
                // registers on THIS session's bus.
                if let Some(ref addr) = dbus_addr {
                    let display_num = display_for_xfconf.trim_start_matches(':');

                    // Control socket: ephemeral per-session (Unix sockets can't
                    // live on NFS and must be unique per display).
                    let keyring_control_dir = format!("/tmp/beam-keyring-{display_num}");
                    let _ = fs::remove_dir_all(&keyring_control_dir);
                    let _ = fs::create_dir_all(&keyring_control_dir);

                    // Data dir: persistent at ~/.local/share/beam/keyring/ so
                    // stored passwords survive across sessions.
                    let home = std::env::var("HOME").unwrap_or_default();
                    let keyring_data_dir = format!("{home}/.local/share/beam/keyring");
                    let keyrings_dir = format!("{keyring_data_dir}/keyrings");
                    let _ = fs::create_dir_all(&keyrings_dir);

                    // Set the default keyring name if not already set (first session).
                    // Do NOT pre-create login.keyring: gnome-keyring uses a binary
                    // format and an empty file causes "invalid or unrecognized
                    // format" errors. The --unlock flag with empty stdin creates
                    // the keyring file in the correct format automatically.
                    let default_path = format!("{keyrings_dir}/default");
                    if !std::path::Path::new(&default_path).exists() {
                        let _ = fs::write(&default_path, "login");
                    }

                    // Use a shell pipe to reliably deliver the empty password
                    // to --unlock via stdin. Direct Stdio::piped() + drop has
                    // a race condition with --foreground (daemon may not have
                    // started reading stdin when we close the pipe).
                    let keyring_cmd = format!(
                        "echo '' | gnome-keyring-daemon --foreground --unlock \
                         --components=secrets --control-directory={}",
                        keyring_control_dir
                    );
                    match Command::new("sh")
                        .args(["-c", &keyring_cmd])
                        .env("DISPLAY", &display_for_xfconf)
                        .env("DBUS_SESSION_BUS_ADDRESS", addr)
                        .env("XDG_DATA_HOME", &keyring_data_dir)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                    {
                        Ok(child) => {
                            info!(
                                pid = child.id(),
                                "gnome-keyring-daemon started (secrets) on session bus"
                            );
                        }
                        Err(e) => {
                            warn!("Failed to start gnome-keyring-daemon: {e}");
                        }
                    }
                }

                // On first session, apply settings via xfconf-query to ensure
                // they take effect (xfconfd may override pre-seeded XML on startup).
                // On subsequent sessions, persistent config already has the right
                // values (including any user customizations), so skip this entirely.
                if is_first_session {
                    let settings: Vec<(&str, &str, &str, &str)> = vec![
                        // Disable compositor (biggest latency offender)
                        ("xfwm4", "/general/use_compositing", "bool", "false"),
                        // Disable workspace zoom animation
                        ("xfwm4", "/general/zoom_desktop", "bool", "false"),
                        // Full opacity during move/resize (no transparency)
                        ("xfwm4", "/general/popup_opacity", "int", "100"),
                        ("xfwm4", "/general/move_opacity", "int", "100"),
                        ("xfwm4", "/general/resize_opacity", "int", "100"),
                        // Disable GTK animations (menu fade-in/out ~200ms)
                        ("xsettings", "/Net/EnableAnimations", "bool", "false"),
                        // Zero delay on submenu popup/popdown (~225ms each)
                        ("xsettings", "/Gtk/MenuPopupDelay", "int", "0"),
                        ("xsettings", "/Gtk/MenuPopdownDelay", "int", "0"),
                        // Disable cursor blink (saves encode bandwidth)
                        ("xsettings", "/Gtk/CursorBlink", "bool", "false"),
                        // Arc-Dark: modern flat dark theme, well-maintained
                        ("xsettings", "/Net/ThemeName", "string", "Arc-Dark"),
                        // Papirus-Dark: comprehensive modern icon theme
                        ("xsettings", "/Net/IconThemeName", "string", "Papirus-Dark"),
                        // Match window manager theme
                        ("xfwm4", "/general/theme", "string", "Arc-Dark"),
                        // Disable screenshooter shortcuts (beam has its own screenshot,
                        // and xfce4-screenshooter may not be installed)
                        (
                            "xfce4-keyboard-shortcuts",
                            "/commands/custom/Print",
                            "string",
                            "",
                        ),
                        (
                            "xfce4-keyboard-shortcuts",
                            "/commands/custom/<Alt>Print",
                            "string",
                            "",
                        ),
                        (
                            "xfce4-keyboard-shortcuts",
                            "/commands/custom/<Shift>Print",
                            "string",
                            "",
                        ),
                    ];

                    for (channel, prop, typ, value) in settings {
                        let mut cmd = Command::new("xfconf-query");
                        cmd.env("DISPLAY", &display_for_xfconf)
                            .args(["-c", channel, "-p", prop, "-n", "-t", typ, "-s", value]);
                        if let Some(ref addr) = dbus_addr {
                            cmd.env("DBUS_SESSION_BUS_ADDRESS", addr);
                        }
                        match cmd.output() {
                            Ok(output) if output.status.success() => {
                                debug!(channel, prop, value, "xfconf setting applied");
                            }
                            Ok(output) => {
                                let stderr = String::from_utf8_lossy(&output.stderr);
                                warn!(channel, prop, "xfconf-query failed: {stderr}");
                            }
                            Err(e) => {
                                warn!(channel, prop, "Failed to run xfconf-query: {e}");
                            }
                        }
                    }

                    info!("XFCE settings applied (compositor off, animations off, theme)");
                }
            });

            return Ok(());
        }

        // Fallback: openbox minimal WM
        if which_exists("openbox") {
            let child = Command::new("openbox")
                .env("DISPLAY", &display)
                .env(
                    "PULSE_SERVER",
                    format!("unix:/tmp/beam-pulse-{}/native", self.display_num),
                )
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("Failed to start openbox")?;

            info!(
                display = self.display_num,
                pid = child.id(),
                "Openbox window manager started (XFCE4 not available)"
            );

            self.desktop_child = Some(child);

            let _ = Command::new("xsetroot")
                .env("DISPLAY", &display)
                .args(["-solid", "#2d3436"])
                .output();

            // Launch a terminal so the user has something to interact with
            if which_exists("xfce4-terminal") {
                let _ = Command::new("xfce4-terminal")
                    .env("DISPLAY", &display)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            } else if which_exists("xterm") {
                let _ = Command::new("xterm")
                    .env("DISPLAY", &display)
                    .args([
                        "-geometry",
                        "100x35+100+100",
                        "-fa",
                        "Monospace",
                        "-fs",
                        "14",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            }

            return Ok(());
        }

        bail!("No desktop environment found. Install xfce4 or openbox.");
    }

    /// Hide the X cursor on the virtual display so only the browser's
    /// native cursor is visible. This gives zero-latency mouse feedback
    /// since the local cursor moves instantly while the remote desktop
    /// content follows with slight network delay.
    ///
    /// Uses `unclutter` if available (best-effort, degrades gracefully).
    pub fn hide_cursor(&mut self) {
        let display = format!(":{}", self.display_num);

        // Prefer unclutter-xfixes: uses XFixes extension to set a transparent
        // cursor image. Unlike classic unclutter (which creates overlay windows
        // or changes cursor shapes), xfixes does NOT generate synthetic
        // Enter/Leave X events. This prevents hover detection issues in apps
        // like YouTube where rapid Enter/Leave causes UI overlay flicker.
        if which_exists("unclutter-xfixes") {
            match Command::new("unclutter-xfixes")
                .args(["--timeout", "0"])
                .env("DISPLAY", &display)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    info!(
                        display = self.display_num,
                        pid = child.id(),
                        "Cursor hidden via unclutter-xfixes"
                    );
                    self.cursor_child = Some(child);
                    return;
                }
                Err(e) => {
                    warn!("Failed to start unclutter-xfixes: {e}");
                }
            }
        }

        // Fallback to classic unclutter with a 1s idle timeout.
        // Using -idle 0 is too aggressive and causes synthetic Enter/Leave
        // events that break hover detection in web apps.
        if which_exists("unclutter") {
            match Command::new("unclutter")
                .args(["-idle", "1", "-root"])
                .env("DISPLAY", &display)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    info!(
                        display = self.display_num,
                        pid = child.id(),
                        "Cursor hidden via unclutter (classic fallback)"
                    );
                    self.cursor_child = Some(child);
                }
                Err(e) => {
                    warn!("Failed to start unclutter: {e}");
                }
            }
        } else {
            debug!("No unclutter variant available, remote cursor will be visible");
        }
    }

    /// Start a PulseAudio daemon for this display's user session.
    pub fn start_pulseaudio(&mut self) -> Result<()> {
        let runtime_dir = format!("/tmp/beam-pulse-{}", self.display_num);
        // Remove stale directory from previous sessions (may be owned by different user)
        let _ = fs::remove_dir_all(&runtime_dir);
        fs::create_dir_all(&runtime_dir)
            .with_context(|| format!("Failed to create PulseAudio dir: {runtime_dir}"))?;

        // Write a minimal PulseAudio config for virtual sessions.
        // Explicit socket path avoids conflict with user's existing PulseAudio.
        let pa_config_path = format!("/tmp/beam-pulse-{}.pa", self.display_num);
        fs::write(&pa_config_path, pa_config(&runtime_dir))
            .with_context(|| format!("Failed to write PA config to {pa_config_path}"))?;

        let child = Command::new("pulseaudio")
            .arg("--daemonize=no")
            .arg("--exit-idle-time=-1")
            .arg("-n") // Skip default.pa — only load modules from our -F script
            .arg("-F")
            .arg(&pa_config_path)
            // Fully isolate from user's existing PulseAudio instance:
            // - PULSE_RUNTIME_PATH: where our socket + pid file go
            // - PULSE_STATE_PATH: where state database goes
            // - XDG_RUNTIME_DIR: prevents discovery of existing PA via /run/user/<uid>/pulse/
            // - Remove DBUS_SESSION_BUS_ADDRESS: prevents "D-Bus name already taken" conflict
            .env("PULSE_RUNTIME_PATH", &runtime_dir)
            .env("PULSE_STATE_PATH", &runtime_dir)
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to start PulseAudio")?;

        info!(
            display = self.display_num,
            pid = child.id(),
            "PulseAudio started"
        );

        self.pulse_child = Some(child);
        Ok(())
    }
}

impl DisplayBackend for XorgVirtualDisplay {
    fn output_name(&self) -> &str {
        self.output_name()
    }

    fn start_desktop(&mut self) -> Result<()> {
        self.start_desktop()
    }

    fn start_pulseaudio(&mut self) -> Result<()> {
        self.start_pulseaudio()
    }

    fn hide_cursor(&mut self) {
        self.hide_cursor();
    }
}

impl Drop for XorgVirtualDisplay {
    fn drop(&mut self) {
        /// Gracefully stop a child process: check if still running before
        /// sending SIGTERM to avoid killing an unrelated process if the
        /// PID has been recycled.
        fn stop_child(child: &mut Child, name: &str, display_num: u32) {
            match child.try_wait() {
                Ok(Some(_)) => return, // already exited
                Ok(None) => {}         // still running
                Err(_) => return,
            }
            let pid = child.id();
            debug!(display = display_num, pid, name, "Stopping process");
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            let _ = child.wait();
        }

        /// Stop a desktop process group: sends SIGTERM to the entire process
        /// group (negative PID) to reach grandchildren (xfwm4, xfce4-panel,
        /// etc.) spawned by dbus-launch -> xfce4-session. Falls back to
        /// SIGKILL after a brief wait if processes are still alive.
        fn stop_desktop_group(child: &mut Child, display_num: u32) {
            match child.try_wait() {
                Ok(Some(_)) => return, // already exited
                Ok(None) => {}         // still running
                Err(_) => return,
            }
            let pid = child.id() as i32;
            debug!(display = display_num, pid, "Stopping desktop process group");
            // Send SIGTERM to the entire process group (negative PID)
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
            // Brief wait for graceful shutdown
            std::thread::sleep(std::time::Duration::from_millis(500));
            // Check if the lead process exited
            match child.try_wait() {
                Ok(Some(_)) => (),
                Ok(None) => {
                    // Still alive — escalate to SIGKILL on the group
                    debug!(
                        display = display_num,
                        pid, "Desktop group still alive, sending SIGKILL"
                    );
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                    let _ = child.wait();
                }
                Err(_) => {}
            }
        }

        // Stop cursor hider
        if let Some(ref mut child) = self.cursor_child {
            stop_child(child, "unclutter", self.display_num);
        }
        // Stop PulseAudio first
        if let Some(ref mut child) = self.pulse_child {
            stop_child(child, "pulseaudio", self.display_num);
        }
        // Stop desktop environment (kill entire process group)
        if let Some(ref mut child) = self.desktop_child {
            stop_desktop_group(child, self.display_num);
        }
        // Stop Xorg
        if let Some(ref mut child) = self.xorg_child {
            stop_child(child, "xorg", self.display_num);
        }
        if let Some(ref path) = self.cleanup_config {
            let _ = fs::remove_file(path);
        }
        if let Some(ref path) = self.cleanup_edid {
            let _ = fs::remove_file(path);
        }
        // Clean up ephemeral per-session directories.
        // NOTE: XFCE config and keyring data are NOT cleaned up — they persist
        // at ~/.local/share/beam/ across sessions.
        let _ = fs::remove_dir_all(format!("/tmp/beam-pulse-{}", self.display_num));
        let _ = fs::remove_file(format!("/tmp/beam-pulse-{}.pa", self.display_num));
        let _ = fs::remove_dir_all(format!("/tmp/beam-keyring-{}", self.display_num));
        let _ = fs::remove_dir_all(format!("/tmp/beam-run-{}", self.display_num));
    }
}

/// Clamp and normalize resize dimensions for safe use with xrandr and H.264.
/// Returns `None` if the dimensions are out of the valid range (320..=7680, 240..=4320).
/// Otherwise clamps to `max_width`/`max_height` (0 = unlimited, default 3840x2160),
/// enforces minimum 640x480, and rounds down to even numbers (H.264 requirement).
pub fn clamp_resize_dimensions(
    w: u32,
    h: u32,
    max_width: u32,
    max_height: u32,
) -> Option<(u32, u32)> {
    // Reject clearly invalid dimensions
    if !(320..=7680).contains(&w) || !(240..=4320).contains(&h) {
        return None;
    }

    // Apply max bounds (0 = unlimited)
    let cw = if max_width > 0 { w.min(max_width) } else { w };
    let ch = if max_height > 0 { h.min(max_height) } else { h };

    // Enforce minimum usable resolution
    let cw = cw.max(640);
    let ch = ch.max(480);

    // Round down to even (H.264 encoder requirement)
    let cw = cw & !1;
    let ch = ch & !1;

    Some((cw, ch))
}

/// Change display resolution using xrandr. Standalone function that only needs
/// the X display string (e.g. ":10"), so it can be called from the capture thread
/// without owning a XorgVirtualDisplay reference.
pub fn set_display_resolution(
    x_display: &str,
    width: u32,
    height: u32,
    output_name: &str,
) -> Result<()> {
    // Wait for X display to be connectable (xrandr can talk to it).
    // On arm64 (e.g. NVIDIA GB10), Xorg needs more than 500ms to fully
    // initialize. Without this, xrandr fails with "Can't open display".
    for attempt in 0..10 {
        let probe = Command::new("xrandr")
            .env("DISPLAY", x_display)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match probe {
            Ok(output) if output.status.success() => break,
            _ if attempt < 9 => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            _ => bail!("X display {x_display} not ready after 2 seconds"),
        }
    }

    let mode_name = format!("{width}x{height}");
    let modeline = generate_modeline(width, height, 60);

    // Try to add the mode (may already exist from a previous resize).
    // Log failures — these help diagnose xrandr issues.
    let newmode_output = Command::new("xrandr")
        .env("DISPLAY", x_display)
        .args(["--newmode", &mode_name])
        .args(modeline.split_whitespace())
        .output()
        .context("Failed to run xrandr --newmode")?;
    if !newmode_output.status.success() {
        let stderr = String::from_utf8_lossy(&newmode_output.stderr);
        // "already exists" is expected for repeated resizes
        if !stderr.contains("already exists") {
            warn!("xrandr --newmode {mode_name} failed: {stderr}");
        }
    }

    // Add mode to the output (may already be added)
    let addmode_output = Command::new("xrandr")
        .env("DISPLAY", x_display)
        .args(["--addmode", output_name, &mode_name])
        .output()
        .context("Failed to run xrandr --addmode")?;
    if !addmode_output.status.success() {
        let stderr = String::from_utf8_lossy(&addmode_output.stderr);
        if !stderr.contains("already exists") {
            warn!("xrandr --addmode {output_name} {mode_name} failed: {stderr}");
        }
    }

    // Switch to the new mode
    let output = Command::new("xrandr")
        .env("DISPLAY", x_display)
        .args(["--output", output_name, "--mode", &mode_name])
        .output()
        .context("Failed to run xrandr --output")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to set resolution to {mode_name}: {stderr}");
    }

    info!(
        x_display,
        width, height, "Display resolution changed via xrandr"
    );
    Ok(())
}

/// Detect the xrandr output name for a display.
/// Parses `xrandr --query` and returns the first connected output name.
/// Falls back to "DUMMY0" if detection fails.
fn detect_xrandr_output(x_display: &str) -> String {
    // Wait for xrandr to be ready (same retry logic as set_display_resolution)
    for attempt in 0..10 {
        let result = Command::new("xrandr")
            .env("DISPLAY", x_display)
            .arg("--query")
            .output();

        match result {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                // Parse lines like "DUMMY0 connected primary 1920x1080+0+0"
                // or "DFP-1 connected 1920x1080+0+0"
                for line in stdout.lines() {
                    if line.contains(" connected")
                        && let Some(name) = line.split_whitespace().next()
                    {
                        return name.to_string();
                    }
                }
                warn!(
                    x_display,
                    "No connected output found in xrandr, using DUMMY0"
                );
                return "DUMMY0".to_string();
            }
            _ if attempt < 9 => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            _ => break,
        }
    }
    warn!(
        x_display,
        "xrandr not ready after 2s, assuming DUMMY0 output"
    );
    "DUMMY0".to_string()
}

/// Generate an Xorg config for the NVIDIA proprietary driver.
/// Uses ConnectedMonitor + CustomEDID for headless virtual display.
fn generate_nvidia_xorg_config(bus_id: &str, dfp_output: &str, edid_path: &str) -> String {
    format!(
        r#"# Beam Virtual Display - NVIDIA GPU-accelerated
# Generated dynamically by beam-agent

Section "Device"
    Identifier  "Beam NVIDIA GPU"
    Driver      "nvidia"
    BusID       "{bus_id}"
    Option      "ConnectedMonitor" "{dfp_output}"
    Option      "CustomEDID" "{dfp_output}:{edid_path}"
    Option      "AllowEmptyInitialConfiguration" "True"
EndSection

Section "Monitor"
    Identifier  "Beam Monitor"
    HorizSync   1-200
    VertRefresh 1-200
EndSection

Section "Screen"
    Identifier  "Beam Screen"
    Device      "Beam NVIDIA GPU"
    Monitor     "Beam Monitor"
    DefaultDepth 24
    SubSection "Display"
        Depth   24
    EndSubSection
EndSection

Section "ServerFlags"
    Option "AutoAddDevices" "false"
    Option "AutoEnableDevices" "false"
    Option "AutoAddGPU" "false"
    Option "DontVTSwitch" "true"
EndSection

Section "ServerLayout"
    Identifier  "Beam Layout"
    Screen      "Beam Screen"
    Option "AutoAddDevices" "false"
EndSection
"#
    )
}

fn generate_xorg_config(width: u32, height: u32) -> String {
    // The dummy driver needs a Modeline for non-standard resolutions.
    // Without it, Xorg falls back to a default mode (e.g. 2048x1536)
    // when the requested resolution isn't a recognized standard mode.
    let modeline = generate_modeline(width, height, 60);
    // Allocate enough VRAM for up to 4K (3840x2160) so dynamic resolution
    // changes via xrandr don't fail with BadMatch. The dummy driver needs
    // VideoRam >= width*height*4/1024 for the LARGEST resolution, not just
    // the initial one. 256MB covers up to 8K.
    let vram: u32 = 262_144; // 256 MB in KB
    format!(
        r#"Section "Device"
    Identifier  "Beam Virtual GPU"
    Driver      "dummy"
    VideoRam    {vram}
EndSection

Section "Monitor"
    Identifier  "Beam Monitor"
    HorizSync   1-200
    VertRefresh 1-200
    Modeline    "{width}x{height}" {modeline}
EndSection

Section "Screen"
    Identifier  "Beam Screen"
    Device      "Beam Virtual GPU"
    Monitor     "Beam Monitor"
    DefaultDepth 24
    SubSection "Display"
        Depth   24
        Virtual 7680 4320
        Modes   "{width}x{height}"
    EndSubSection
EndSection

Section "ServerFlags"
    Option "AutoAddDevices" "false"
    Option "AutoEnableDevices" "false"
    Option "DontVTSwitch" "true"
EndSection

Section "ServerLayout"
    Identifier  "Beam Layout"
    Screen      "Beam Screen"
    Option "AutoAddDevices" "false"
EndSection
"#,
    )
}

fn generate_modeline(width: u32, height: u32, refresh: u32) -> String {
    // Simplified CVT modeline calculation
    let pixel_clock = (width as f64 * height as f64 * refresh as f64) / 1_000_000.0 * 1.2;
    format!(
        "{:.2} {} {} {} {} {} {} {} {} +hsync +vsync",
        pixel_clock,
        width,
        width + 48,
        width + 48 + 32,
        width + 48 + 32 + 80,
        height,
        height + 3,
        height + 3 + 5,
        height + 3 + 5 + 25,
    )
}

fn is_display_running(display_num: u32) -> bool {
    let lock_file = format!("/tmp/.X{display_num}-lock");
    // Read PID from lock file and verify the process is actually running
    // (handles stale lock files from crashed Xorg)
    match fs::read_to_string(&lock_file) {
        Ok(contents) => {
            if let Ok(pid) = contents.trim().parse::<i32>() {
                // signal 0 checks if process exists without signaling it
                unsafe { libc::kill(pid, 0) == 0 }
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

fn which_exists(program: &str) -> bool {
    Command::new("which")
        .arg(program)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Check if a binary is a snap package. Detects both direct snap binaries
/// (/snap/bin/...) and wrapper scripts at /usr/bin/ that delegate to snap.
/// Snap apps fail in Beam sessions because they require a logind session,
/// snap environment variables, and cgroup access that beam-agent doesn't have.
fn is_snap_binary(program: &str) -> bool {
    let path = Command::new("which")
        .arg(program)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|p| p.trim().to_string());

    match path {
        Some(p) if p.starts_with("/snap/") => true,
        Some(p) => {
            // Check if it's a wrapper script that invokes snap
            std::fs::read_to_string(&p)
                .map(|contents| contents.contains("/snap/bin/") || contents.contains("exec snap"))
                .unwrap_or(false)
        }
        None => false,
    }
}

/// Find the first non-snap binary from a list of candidates.
/// Snap apps fail in Beam sessions (no logind session, no snap env vars).
fn find_non_snap_app(candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|name| which_exists(name) && !is_snap_binary(name))
}

/// Discover DBUS_SESSION_BUS_ADDRESS for the current session.
/// Strategy 1: systemd user bus at /run/user/<uid>/bus (fast, reliable with PAM sessions).
/// Strategy 2: fall back to scanning /proc for xfce4-panel's environ.
fn find_dbus_address_for_display(x_display: &str) -> Option<String> {
    // Strategy 1: systemd user bus (created by pam_systemd)
    let uid = nix::unistd::getuid().as_raw();
    let bus_path = format!("/run/user/{uid}/bus");
    if std::path::Path::new(&bus_path).exists() {
        let addr = format!("unix:path={bus_path}");
        debug!(x_display, addr, "Using systemd user bus for DBUS");
        return Some(addr);
    }

    // Strategy 2: fall back to /proc scan
    let output = Command::new("pgrep")
        .arg("-x")
        .arg("xfce4-panel")
        .output()
        .ok()?;
    let pids = String::from_utf8_lossy(&output.stdout);
    for pid_str in pids.lines() {
        let pid = pid_str.trim();
        if pid.is_empty() {
            continue;
        }
        let Ok(environ) = fs::read(format!("/proc/{pid}/environ")) else {
            continue; // Permission denied for other users' processes — skip
        };
        let mut has_display = false;
        let mut dbus_addr = None;
        for var in environ.split(|&b| b == 0) {
            let var_str = String::from_utf8_lossy(var);
            if var_str == format!("DISPLAY={x_display}") {
                has_display = true;
            }
            if let Some(addr) = var_str.strip_prefix("DBUS_SESSION_BUS_ADDRESS=") {
                dbus_addr = Some(addr.to_string());
            }
        }
        if has_display {
            if let Some(ref addr) = dbus_addr {
                debug!(
                    x_display,
                    addr, "Found DBUS session address from panel process"
                );
            }
            return dbus_addr;
        }
    }
    warn!(
        x_display,
        "Could not find DBUS_SESSION_BUS_ADDRESS for display"
    );
    None
}

/// Ensure XFCE/GTK config directory exists with default settings.
/// Uses persistent storage at `~/.local/share/beam/config/` so desktop
/// customizations (theme, panel layout, stored passwords) survive across sessions.
/// Falls back to ephemeral `/tmp/beam-xfce-{display_num}` if persistent storage
/// is unavailable (e.g. NFS home unreachable).
/// Returns `(config_dir_path, is_first_session)`.
fn ensure_persistent_config(display_num: u32) -> (String, bool) {
    match try_persistent_config() {
        Ok(result) => result,
        Err(e) => {
            warn!("Persistent config unavailable, falling back to ephemeral: {e}");
            let fallback = format!("/tmp/beam-xfce-{display_num}");
            let _ = fs::remove_dir_all(&fallback);
            let _ = fs::create_dir_all(&fallback);
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&fallback, fs::Permissions::from_mode(0o700));
            }
            seed_default_config(&fallback);
            (fallback, true)
        }
    }
}

fn try_persistent_config() -> Result<(String, bool)> {
    let home = std::env::var("HOME").context("HOME not set")?;
    try_persistent_config_in(&home)
}

fn try_persistent_config_in(home: &str) -> Result<(String, bool)> {
    let beam_dir = format!("{home}/.local/share/beam");
    let config_dir = format!("{beam_dir}/config");
    let sentinel = format!("{beam_dir}/.initialized");

    if std::path::Path::new(&sentinel).exists() {
        return Ok((config_dir, false));
    }

    // First session: create directory structure and seed defaults
    fs::create_dir_all(&config_dir).with_context(|| format!("Failed to create {config_dir}"))?;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("Failed to set permissions on {config_dir}"))?;
    }

    seed_default_config(&config_dir);

    // Version file for future config migrations
    let _ = fs::write(format!("{beam_dir}/.config-version"), "1");
    // Sentinel written last — signals initialization completed successfully
    fs::write(&sentinel, "").context("Failed to write initialization sentinel")?;

    info!("Persistent desktop config initialized at {config_dir}");
    Ok((config_dir, true))
}

/// Seed all default XFCE/GTK configuration files for a fresh Beam desktop.
/// Covers: xfconf XML channels, GTK3 settings, autostart masks, default
/// browser/terminal helpers, and MIME type associations.
fn seed_default_config(config_dir: &str) {
    let xfconf_dir = format!("{config_dir}/xfce4/xfconf/xfce-perchannel-xml");
    let _ = fs::create_dir_all(&xfconf_dir);

    // xfwm4: disable compositor, workspace zoom animation, and pre-seed theme
    let _ = fs::write(
        format!("{xfconf_dir}/xfwm4.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfwm4" version="1.0">
  <property name="general" type="empty">
    <property name="use_compositing" type="bool" value="false"/>
    <property name="zoom_desktop" type="bool" value="false"/>
    <property name="popup_opacity" type="int" value="100"/>
    <property name="move_opacity" type="int" value="100"/>
    <property name="resize_opacity" type="int" value="100"/>
    <property name="theme" type="string" value="Arc-Dark"/>
  </property>
</channel>
"#,
    );

    // xsettings: disable GTK animations, pre-seed theme/icons/cursor settings
    let _ = fs::write(
        format!("{xfconf_dir}/xsettings.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xsettings" version="1.0">
  <property name="Gtk" type="empty">
    <property name="MenuPopupDelay" type="int" value="0"/>
    <property name="MenuPopdownDelay" type="int" value="0"/>
    <property name="CursorBlink" type="bool" value="false"/>
  </property>
  <property name="Net" type="empty">
    <property name="EnableAnimations" type="bool" value="false"/>
    <property name="ThemeName" type="string" value="Arc-Dark"/>
    <property name="IconThemeName" type="string" value="Papirus-Dark"/>
  </property>
</channel>
"#,
    );

    // xfce4-session: no splash screen
    let _ = fs::write(
        format!("{xfconf_dir}/xfce4-session.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-session" version="1.0">
  <property name="splash" type="empty">
    <property name="Engine" type="string" value=""/>
  </property>
</channel>
"#,
    );

    // Pre-seed panel config: use Whisker Menu (plugin-1) if available
    let panel_plugin_1 = if which_exists("xfce4-popup-whiskermenu") {
        "whiskermenu"
    } else {
        "applicationsmenu"
    };
    let _ = fs::write(
        format!("{xfconf_dir}/xfce4-panel.xml"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-panel" version="1.0">
  <property name="panels" type="array">
    <value type="int" value="1"/>
    <property name="panel-1" type="empty">
      <property name="plugin-ids" type="array">
        <value type="int" value="1"/>
        <value type="int" value="2"/>
        <value type="int" value="3"/>
        <value type="int" value="4"/>
        <value type="int" value="5"/>
        <value type="int" value="6"/>
      </property>
      <property name="position" type="string" value="p=6;x=0;y=0"/>
      <property name="position-locked" type="bool" value="true"/>
      <property name="size" type="uint" value="28"/>
    </property>
  </property>
  <property name="plugins" type="empty">
    <property name="plugin-1" type="string" value="{plugin_1}"/>
    <property name="plugin-2" type="string" value="tasklist"/>
    <property name="plugin-3" type="string" value="separator">
      <property name="expand" type="bool" value="true"/>
      <property name="style" type="uint" value="0"/>
    </property>
    <property name="plugin-4" type="string" value="systray"/>
    <property name="plugin-5" type="string" value="clock"/>
    <property name="plugin-6" type="string" value="actions"/>
  </property>
</channel>
"#,
            plugin_1 = panel_plugin_1
        ),
    );

    // Desktop wallpaper: XFCE shapes SVG (clean, lightweight)
    let _ = fs::write(
        format!("{xfconf_dir}/xfce4-desktop.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-desktop" version="1.0">
  <property name="backdrop" type="empty">
    <property name="screen0" type="empty">
      <property name="monitorDUMMY0" type="empty">
        <property name="workspace0" type="empty">
          <property name="last-image" type="string" value="/usr/share/backgrounds/xfce/xfce-shapes.svg"/>
          <property name="image-style" type="int" value="5"/>
          <property name="color-style" type="int" value="0"/>
        </property>
      </property>
    </property>
  </property>
</channel>
"#,
    );

    // Keyboard shortcuts: Alt+F2 for app finder search
    let _ = fs::write(
        format!("{xfconf_dir}/xfce4-keyboard-shortcuts.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-keyboard-shortcuts" version="1.0">
  <property name="commands" type="empty">
    <property name="custom" type="empty">
      <property name="&lt;Alt&gt;F2" type="string" value="xfce4-appfinder --collapsed"/>
    </property>
  </property>
</channel>
"#,
    );

    // GTK3 settings: disable animations, menu delays, cursor blink
    let gtk3_dir = format!("{config_dir}/gtk-3.0");
    let _ = fs::create_dir_all(&gtk3_dir);
    let _ = fs::write(
        format!("{gtk3_dir}/settings.ini"),
        "[Settings]\n\
         gtk-enable-animations=false\n\
         gtk-menu-popup-delay=0\n\
         gtk-menu-popdown-delay=0\n\
         gtk-cursor-blink=false\n",
    );

    // GTK3 CSS: kill ALL CSS transitions (GTK themes use 200ms+
    // transitions on buttons, menus, entries, hover states etc.).
    // gtk-enable-animations only affects GtkAnimation objects, NOT CSS
    // transitions — this override is required for instant menu hover.
    let _ = fs::write(
        format!("{gtk3_dir}/gtk.css"),
        "* { transition-duration: 0s !important; animation-duration: 0s !important; }\n",
    );

    // Mask autostart entries that fail or are useless in a virtual session.
    // XDG spec: user-level .desktop files in $XDG_CONFIG_HOME/autostart/
    // override system-level files in /etc/xdg/autostart/ by filename.
    let autostart_dir = format!("{config_dir}/autostart");
    let _ = fs::create_dir_all(&autostart_dir);
    for entry in [
        "update-notifier.desktop",                     // pkexec error dialogs
        "polkit-gnome-authentication-agent-1.desktop", // pkexec auth prompts
        "pulseaudio.desktop",                          // conflicts with our PulseAudio
        "tracker-miner-fs-3.desktop",                  // file indexer wastes CPU
        "snap-userd-autostart.desktop",                // snap UI daemon
        "spice-vdagent.desktop",                       // SPICE agent, not used
        "ubuntu-advantage-notification.desktop",       // Ubuntu Pro nag
        "ubuntu-report-on-upgrade.desktop",            // upgrade reporter
        "gnome-initial-setup-copy-worker.desktop",     // GNOME first-run
        "gnome-initial-setup-first-login.desktop",     // GNOME first-run
        "org.gnome.DejaDup.Monitor.desktop",           // backup monitor
        "org.gnome.Evolution-alarm-notify.desktop",    // calendar alarms
    ] {
        let _ = fs::write(
            format!("{autostart_dir}/{entry}"),
            "[Desktop Entry]\nHidden=true\n",
        );
    }

    // Configure default applications (browser + terminal).
    // helpers.rc: XFCE helper IDs for exo-open
    // mimeapps.list: XDG MIME type associations for xdg-open
    let helpers_dir = format!("{config_dir}/xfce4");
    let _ = fs::create_dir_all(&helpers_dir);

    let detected_browser = find_non_snap_app(&[
        "firefox-esr",
        "google-chrome-stable",
        "google-chrome",
        "chromium-browser",
        "firefox",
        "chromium",
        "epiphany-browser",
    ]);
    let detected_terminal = find_non_snap_app(&["xfce4-terminal", "gnome-terminal", "xterm"]);

    let mut helpers_rc = String::from("[Default]\n");
    if let Some(term) = detected_terminal {
        helpers_rc.push_str(&format!("TerminalEmulator={term}\n"));
    }
    if let Some(browser) = detected_browser {
        let helper_id = match browser {
            "firefox-esr" => "firefox-esr",
            "firefox" => "firefox",
            "google-chrome-stable" | "google-chrome" => "google-chrome",
            "chromium-browser" | "chromium" => "chromium",
            "epiphany-browser" => "epiphany",
            _ => browser,
        };
        helpers_rc.push_str(&format!("WebBrowser={helper_id}\n"));
    }
    let _ = fs::write(format!("{helpers_dir}/helpers.rc"), &helpers_rc);

    if let Some(browser) = detected_browser {
        let desktop_file = match browser {
            "firefox-esr" => "firefox-esr.desktop",
            "firefox" => "firefox.desktop",
            "google-chrome-stable" | "google-chrome" => "google-chrome.desktop",
            "chromium-browser" | "chromium" => "chromium-browser.desktop",
            "epiphany-browser" => "org.gnome.Epiphany.desktop",
            _ => "",
        };
        if !desktop_file.is_empty() {
            let content = format!(
                "[Default Applications]\n\
                 x-scheme-handler/http={d}\n\
                 x-scheme-handler/https={d}\n\
                 text/html={d}\n\
                 application/xhtml+xml={d}\n",
                d = desktop_file,
            );
            let _ = fs::write(format!("{config_dir}/mimeapps.list"), content);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xorg_config_has_generous_vram_for_dynamic_resize() {
        // Even at a small initial resolution, VRAM must be large enough
        // for fullscreen (e.g. 4K). Otherwise xrandr --output fails with
        // BadMatch when the user enters fullscreen.
        let config = generate_xorg_config(800, 600);
        assert!(
            config.contains("VideoRam    262144"),
            "VRAM should be 256MB"
        );
        // Check Virtual max size is set for dynamic resolution
        assert!(
            config.contains("Virtual 7680 4320"),
            "Virtual screen should support up to 8K"
        );
    }

    #[test]
    fn xorg_config_includes_initial_modeline() {
        let config = generate_xorg_config(1920, 1080);
        assert!(config.contains("Modeline    \"1920x1080\""));
        assert!(config.contains("Modes   \"1920x1080\""));
    }

    #[test]
    fn modeline_format_is_valid() {
        let ml = generate_modeline(1920, 1080, 60);
        let parts: Vec<&str> = ml.split_whitespace().collect();
        // Should be: clock h h_sync_start h_sync_end h_total v v_sync_start v_sync_end v_total +hsync +vsync
        assert_eq!(parts.len(), 11, "Modeline should have 11 fields: {ml}");
        // Pixel clock should be positive
        let clock: f64 = parts[0].parse().expect("clock should be a float");
        assert!(clock > 0.0, "Pixel clock should be positive");
        // h_total > width
        let h_total: u32 = parts[4].parse().unwrap();
        assert!(h_total > 1920, "h_total should be > width");
        // v_total > height
        let v_total: u32 = parts[8].parse().unwrap();
        assert!(v_total > 1080, "v_total should be > height");
        // Sync flags
        assert_eq!(parts[9], "+hsync");
        assert_eq!(parts[10], "+vsync");
    }

    #[test]
    fn modeline_dimensions_are_correct() {
        let ml = generate_modeline(1800, 1168, 60);
        let parts: Vec<&str> = ml.split_whitespace().collect();
        assert_eq!(parts[1], "1800", "hdisp should match width");
        assert_eq!(parts[5], "1168", "vdisp should match height");
    }

    #[test]
    fn clamp_resize_rejects_too_small() {
        assert_eq!(clamp_resize_dimensions(100, 100, 0, 0), None);
        assert_eq!(clamp_resize_dimensions(319, 480, 0, 0), None);
        assert_eq!(clamp_resize_dimensions(640, 239, 0, 0), None);
    }

    #[test]
    fn clamp_resize_rejects_too_large() {
        assert_eq!(clamp_resize_dimensions(7681, 1080, 0, 0), None);
        assert_eq!(clamp_resize_dimensions(1920, 4321, 0, 0), None);
    }

    #[test]
    fn clamp_resize_enforces_max_bounds() {
        // max_width=1920, max_height=1080
        let (w, h) = clamp_resize_dimensions(2560, 1440, 1920, 1080).unwrap();
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
    }

    #[test]
    fn clamp_resize_unlimited_max() {
        // max=0 means unlimited
        let (w, h) = clamp_resize_dimensions(3840, 2160, 0, 0).unwrap();
        assert_eq!(w, 3840);
        assert_eq!(h, 2160);
    }

    #[test]
    fn clamp_resize_enforces_min_640x480() {
        let (w, h) = clamp_resize_dimensions(320, 240, 0, 0).unwrap();
        assert_eq!(w, 640);
        assert_eq!(h, 480);
    }

    #[test]
    fn clamp_resize_enforces_even_dimensions() {
        // Odd dimensions should be rounded down to even
        let (w, h) = clamp_resize_dimensions(1921, 1081, 0, 0).unwrap();
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
    }

    #[test]
    fn clamp_resize_passthrough_normal() {
        let (w, h) = clamp_resize_dimensions(1920, 1080, 3840, 2160).unwrap();
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
    }

    #[test]
    fn clamp_resize_even_after_max_clamp() {
        // If max bound produces an odd number, still round to even
        let (w, h) = clamp_resize_dimensions(2000, 1200, 1921, 1081).unwrap();
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
    }

    #[test]
    fn find_dbus_prefers_user_bus() {
        // If /run/user/<uid>/bus exists, the function should return it
        let uid = nix::unistd::getuid().as_raw();
        let bus_path = format!("/run/user/{uid}/bus");
        if std::path::Path::new(&bus_path).exists() {
            let result = find_dbus_address_for_display(":99");
            assert_eq!(
                result,
                Some(format!("unix:path={bus_path}")),
                "Should prefer systemd user bus when it exists"
            );
        }
        // If the bus doesn't exist, this test is a no-op (CI environments)
    }

    #[test]
    fn find_dbus_returns_none_for_nonexistent_display() {
        // Use a display number that won't have any running processes.
        // If user bus exists, it returns that regardless of display, so only
        // test the fallback behavior when user bus is absent.
        let uid = nix::unistd::getuid().as_raw();
        let bus_path = format!("/run/user/{uid}/bus");
        if !std::path::Path::new(&bus_path).exists() {
            let result = find_dbus_address_for_display(":9999");
            assert!(
                result.is_none(),
                "Should return None for nonexistent display when no user bus"
            );
        }
    }

    #[test]
    fn seed_default_config_creates_correct_structure() {
        let dir = std::env::temp_dir().join(format!("beam-test-seed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dir_str = dir.to_str().unwrap();

        seed_default_config(dir_str);

        // Verify xfconf XML files exist
        let xfconf = dir.join("xfce4/xfconf/xfce-perchannel-xml");
        assert!(xfconf.join("xfwm4.xml").exists(), "xfwm4.xml missing");
        assert!(
            xfconf.join("xsettings.xml").exists(),
            "xsettings.xml missing"
        );
        assert!(
            xfconf.join("xfce4-session.xml").exists(),
            "xfce4-session.xml missing"
        );
        assert!(
            xfconf.join("xfce4-panel.xml").exists(),
            "xfce4-panel.xml missing"
        );
        assert!(
            xfconf.join("xfce4-desktop.xml").exists(),
            "xfce4-desktop.xml missing"
        );
        assert!(
            xfconf.join("xfce4-keyboard-shortcuts.xml").exists(),
            "keyboard shortcuts missing"
        );

        // Verify GTK3 config
        assert!(
            dir.join("gtk-3.0/settings.ini").exists(),
            "GTK settings missing"
        );
        assert!(dir.join("gtk-3.0/gtk.css").exists(), "GTK CSS missing");

        // Verify autostart masks
        assert!(
            dir.join("autostart/pulseaudio.desktop").exists(),
            "autostart mask missing"
        );

        // Verify helpers.rc exists
        assert!(dir.join("xfce4/helpers.rc").exists(), "helpers.rc missing");

        // Verify theme settings in XML
        let xsettings = fs::read_to_string(xfconf.join("xsettings.xml")).unwrap();
        assert!(
            xsettings.contains(r#""ThemeName" type="string" value="Arc-Dark""#),
            "xsettings.xml should pre-seed Arc-Dark theme"
        );
        assert!(
            xsettings.contains(r#""IconThemeName" type="string" value="Papirus-Dark""#),
            "xsettings.xml should pre-seed Papirus-Dark icons"
        );

        let xfwm4 = fs::read_to_string(xfconf.join("xfwm4.xml")).unwrap();
        assert!(
            xfwm4.contains(r#""use_compositing" type="bool" value="false""#),
            "xfwm4.xml should disable compositor"
        );
        assert!(
            xfwm4.contains(r#""theme" type="string" value="Arc-Dark""#),
            "xfwm4.xml should pre-seed Arc-Dark window theme"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_config_creates_sentinel_and_version() {
        let dir = std::env::temp_dir().join(format!("beam-test-persist-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let home = dir.to_str().unwrap();
        let beam_dir = dir.join(".local/share/beam");

        let result = try_persistent_config_in(home);
        assert!(result.is_ok(), "First call should succeed");
        let (config_dir, is_first) = result.unwrap();
        assert!(is_first, "First call should report is_first_session=true");
        assert!(
            config_dir.ends_with(".local/share/beam/config"),
            "Config dir should be under beam/"
        );

        // Verify sentinel and version
        assert!(beam_dir.join(".initialized").exists(), "Sentinel missing");
        assert!(
            beam_dir.join(".config-version").exists(),
            "Version file missing"
        );
        assert_eq!(
            fs::read_to_string(beam_dir.join(".config-version")).unwrap(),
            "1"
        );

        // Verify config files were seeded
        assert!(
            std::path::Path::new(&config_dir)
                .join("xfce4/xfconf/xfce-perchannel-xml/xfwm4.xml")
                .exists(),
            "Config files should be seeded"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_config_skips_on_subsequent_call() {
        let dir = std::env::temp_dir().join(format!("beam-test-skip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let home = dir.to_str().unwrap();

        // First call: seeds config
        let (_, is_first) = try_persistent_config_in(home).unwrap();
        assert!(is_first);

        // Modify a config file to verify it's not overwritten
        let xfwm4_path =
            dir.join(".local/share/beam/config/xfce4/xfconf/xfce-perchannel-xml/xfwm4.xml");
        fs::write(&xfwm4_path, "user-customized").unwrap();

        // Second call: should skip seeding
        let (_, is_first) = try_persistent_config_in(home).unwrap();
        assert!(
            !is_first,
            "Second call should report is_first_session=false"
        );

        // Verify user's customization was preserved
        let content = fs::read_to_string(&xfwm4_path).unwrap();
        assert_eq!(
            content, "user-customized",
            "User customizations should be preserved on subsequent sessions"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_config_fallback_on_error() {
        // try_persistent_config_in should fail for a non-writable path,
        // and ensure_persistent_config should fall back to ephemeral /tmp.
        let result = try_persistent_config_in("/proc/nonexistent");
        assert!(result.is_err(), "Should fail for non-writable path");

        // Test the full fallback via ensure_persistent_config by temporarily
        // setting HOME (safe in this specific test — no concurrent HOME readers).
        let original_home = std::env::var("HOME").unwrap();
        unsafe { std::env::set_var("HOME", "/proc/nonexistent") };

        let (config_dir, is_first) = ensure_persistent_config(9999);
        assert!(is_first, "Fallback should report first session");
        assert_eq!(
            config_dir, "/tmp/beam-xfce-9999",
            "Should fall back to ephemeral path"
        );

        // Verify fallback dir was created with config files
        assert!(
            std::path::Path::new("/tmp/beam-xfce-9999/xfce4/xfconf/xfce-perchannel-xml/xfwm4.xml")
                .exists(),
            "Fallback should seed config files"
        );

        unsafe { std::env::set_var("HOME", &original_home) };
        let _ = fs::remove_dir_all("/tmp/beam-xfce-9999");
    }

    #[test]
    fn seed_default_config_writes_gtk_css_transitions() {
        let dir = std::env::temp_dir().join(format!("beam-test-gtk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dir_str = dir.to_str().unwrap();

        seed_default_config(dir_str);

        let css = fs::read_to_string(dir.join("gtk-3.0/gtk.css")).unwrap();
        assert!(
            css.contains("transition-duration: 0s"),
            "GTK CSS should disable transitions"
        );
        assert!(
            css.contains("animation-duration: 0s"),
            "GTK CSS should disable animations"
        );

        let settings = fs::read_to_string(dir.join("gtk-3.0/settings.ini")).unwrap();
        assert!(
            settings.contains("gtk-enable-animations=false"),
            "GTK settings should disable animations"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_config_masks_autostart_entries() {
        let dir = std::env::temp_dir().join(format!("beam-test-auto-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dir_str = dir.to_str().unwrap();

        seed_default_config(dir_str);

        let autostart = dir.join("autostart");
        // Verify a sample of masked entries
        for entry in [
            "pulseaudio.desktop",
            "tracker-miner-fs-3.desktop",
            "update-notifier.desktop",
        ] {
            let path = autostart.join(entry);
            assert!(path.exists(), "{entry} should be masked");
            let content = fs::read_to_string(&path).unwrap();
            assert!(content.contains("Hidden=true"), "{entry} should be hidden");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_config_writes_compositor_disabled() {
        let dir = std::env::temp_dir().join(format!("beam-test-comp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dir_str = dir.to_str().unwrap();

        seed_default_config(dir_str);

        let xfwm4 =
            fs::read_to_string(dir.join("xfce4/xfconf/xfce-perchannel-xml/xfwm4.xml")).unwrap();
        assert!(
            xfwm4.contains(r#""use_compositing" type="bool" value="false""#),
            "Compositor should be disabled by default"
        );
        assert!(
            xfwm4.contains(r#""zoom_desktop" type="bool" value="false""#),
            "Workspace zoom should be disabled"
        );
        assert!(
            xfwm4.contains(r#""popup_opacity" type="int" value="100""#),
            "Popup opacity should be 100%"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_config_writes_panel_config() {
        let dir = std::env::temp_dir().join(format!("beam-test-panel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dir_str = dir.to_str().unwrap();

        seed_default_config(dir_str);

        let panel =
            fs::read_to_string(dir.join("xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml"))
                .unwrap();
        assert!(
            panel.contains("tasklist"),
            "Panel should have tasklist plugin"
        );
        assert!(panel.contains("clock"), "Panel should have clock plugin");
        assert!(
            panel.contains("systray"),
            "Panel should have systray plugin"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_config_sentinel_prevents_reseeding() {
        // Verify that even with a corrupt/partial config dir, the sentinel
        // prevents re-seeding (user customizations are preserved).
        let dir = std::env::temp_dir().join(format!("beam-test-sentinel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let home = dir.to_str().unwrap();
        let beam_dir = dir.join(".local/share/beam");
        let config_dir = beam_dir.join("config");

        // First call seeds everything
        let (_, is_first) = try_persistent_config_in(home).unwrap();
        assert!(is_first);

        // Delete some config files (simulating user removing them)
        let _ = fs::remove_file(config_dir.join("gtk-3.0/gtk.css"));

        // Second call should NOT re-create deleted files
        let (_, is_first) = try_persistent_config_in(home).unwrap();
        assert!(!is_first);
        assert!(
            !config_dir.join("gtk-3.0/gtk.css").exists(),
            "Deleted file should not be re-created after sentinel exists"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_config_version_file_content() {
        let dir = std::env::temp_dir().join(format!("beam-test-ver-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let home = dir.to_str().unwrap();

        let _ = try_persistent_config_in(home).unwrap();

        let version = fs::read_to_string(dir.join(".local/share/beam/.config-version")).unwrap();
        assert_eq!(version, "1", "Config version should be 1");

        let _ = fs::remove_dir_all(&dir);
    }
}
