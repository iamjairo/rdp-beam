//! Wayland end-to-end integration test for the Beam agent.
//!
//! # What this covers
//! - cage compositor spawn + socket readiness
//! - WaylandCapture::new() portal negotiation (xdg-desktop-portal-wlr via gdbus)
//! - GStreamer pipewiresrc -> H.264 encoder pipeline
//! - Pulling >= 10 encoded H.264 frames within 2 seconds
//! - Inter-frame timing summary (avg gap between pull() calls that returned data)
//! - Clean teardown: SIGTERM cage, remove XDG_RUNTIME_DIR
//!
//! # What this does NOT cover
//! - Virtual input (A4 stubs bail) -- excluded by design
//! - Audio capture (A5 stubs bail) -- excluded by design
//! - Real desktop rendering inside cage -- empty compositor is sufficient
//!
//! # How to run
//! Must run on a host with: cage, pipewire, wireplumber, xdg-desktop-portal-wlr,
//! GStreamer 1.28 + gstreamer1.0-pipewire, libwayland-dev, libpipewire-0.3-dev.
//!
//!   cargo test -p beam-agent --features wayland --test wayland_e2e -- --ignored --nocapture
//!
//! Marked #[ignore] so normal CI (cargo test) never runs this.
//! Gated #[cfg(feature = "wayland")] so default builds skip compilation entirely.

#![cfg(feature = "wayland")]

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use beam_agent::wayland::WaylandCapture;

const XDG_RUNTIME_DIR: &str = "/tmp/beam-xdg-test";
const SOCKET_POLL_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(100);
const FRAME_BUDGET: Duration = Duration::from_secs(2);
const MIN_FRAMES: usize = 10;
const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const FRAMERATE: u32 = 30;
const BITRATE: u32 = 1_000_000;

/// Spawn a headless cage compositor.
///
/// cage 0.2.x ignores WAYLAND_DISPLAY as a compositor socket name; it picks
/// its own name (wayland-0, wayland-1, ...) inside XDG_RUNTIME_DIR.
/// cage also exits when its client exits, so we pass `sleep 120` as the
/// client to keep the compositor alive for the test duration.
///
/// NOTE: `WaylandInput::new` mutates `WAYLAND_DISPLAY` via `unsafe set_var`.
/// This test is single-process; that mutation is benign here but would race
/// if tests ran in parallel with other tests reading the same env var.
fn spawn_cage() -> Result<Child, String> {
    std::fs::create_dir_all(XDG_RUNTIME_DIR)
        .map_err(|e| format!("Failed to create XDG_RUNTIME_DIR {XDG_RUNTIME_DIR}: {e}"))?;
    std::fs::set_permissions(XDG_RUNTIME_DIR, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("Failed to chmod XDG_RUNTIME_DIR: {e}"))?;

    Command::new("cage")
        .arg("--")
        .arg("/bin/sleep")
        .arg("120")
        .env("XDG_RUNTIME_DIR", XDG_RUNTIME_DIR)
        .env("WLR_BACKENDS", "headless")
        .env("WLR_RENDERER", "pixman")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            format!(
                "STEP 1 FAILED -- compositor spawn: cage not found or spawn error: {e}\n\
             Fix: apt-get install cage"
            )
        })
}

/// Wait for cage's Wayland socket to appear in XDG_RUNTIME_DIR.
///
/// cage picks its own socket name (wayland-0 / wayland-1 / ...).
/// Returns the actual socket name (e.g. "wayland-0") on success.
fn wait_for_socket() -> Result<String, String> {
    let deadline = Instant::now() + SOCKET_POLL_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(XDG_RUNTIME_DIR) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                // Wayland socket names look like "wayland-0", "wayland-1", etc.
                if name_str.starts_with("wayland-")
                    && !name_str.ends_with(".lock")
                    && entry.path().exists()
                {
                    // Confirm it's a socket.
                    if let Ok(meta) = entry.metadata() {
                        if meta.file_type().is_socket() {
                            return Ok(name_str.to_string());
                        }
                    }
                }
            }
        }
        std::thread::sleep(SOCKET_POLL_INTERVAL);
    }
    Err(format!(
        "STEP 1 FAILED -- no wayland-* socket appeared in {XDG_RUNTIME_DIR} within {}s.\n\
         Likely causes:\n\
         - cage failed to start (check WLR_BACKENDS=headless, WLR_RENDERER=pixman)\n\
         - XDG_RUNTIME_DIR permissions wrong (needs 0700)\n\
         - Missing headless backend: apt-get install libwlroots-dev",
        SOCKET_POLL_TIMEOUT.as_secs()
    ))
}

fn teardown(mut cage: Child) {
    let _ = cage.kill();
    let _ = cage.wait();
    let _ = std::fs::remove_dir_all(XDG_RUNTIME_DIR);
}

#[test]
#[ignore]
fn wayland_capture_e2e() {
    eprintln!("[wayland_e2e] STEP 1: spawning cage compositor");
    let cage = match spawn_cage() {
        Ok(c) => c,
        Err(msg) => panic!("{msg}"),
    };

    let wayland_display = match wait_for_socket() {
        Ok(s) => s,
        Err(msg) => {
            teardown(cage);
            panic!("{msg}");
        }
    };
    eprintln!("[wayland_e2e] STEP 1 OK: socket {XDG_RUNTIME_DIR}/{wayland_display} is ready");

    eprintln!("[wayland_e2e] STEP 2: opening WaylandCapture (portal + pipewiresrc)");
    let capture = match WaylandCapture::new(
        &wayland_display,
        XDG_RUNTIME_DIR,
        WIDTH,
        HEIGHT,
        FRAMERATE,
        BITRATE,
        None,
    ) {
        Ok(c) => c,
        Err(e) => {
            teardown(cage);
            panic!(
                "STEP 2 FAILED -- WaylandCapture::new() error: {e:#}\n\
                 Likely causes:\n\
                 - xdg-desktop-portal-wlr not running on this session bus\n\
                   Fix: systemctl --user start xdg-desktop-portal-wlr\n\
                 - pipewiresrc GStreamer element missing\n\
                   Fix: apt-get install gstreamer1.0-pipewire\n\
                 - pipewire/wireplumber not running\n\
                   Fix: systemctl --user start pipewire pipewire-pulse wireplumber\n\
                 - gdbus not in PATH\n\
                   Fix: apt-get install libglib2.0-bin"
            );
        }
    };
    eprintln!("[wayland_e2e] STEP 2 OK: pipeline started");

    eprintln!(
        "[wayland_e2e] STEP 3: pulling >= {MIN_FRAMES} H.264 frames in {}s",
        FRAME_BUDGET.as_secs()
    );

    let mut frame_count = 0usize;
    let mut total_bytes = 0usize;
    let mut pull_times: Vec<Instant> = Vec::with_capacity(MIN_FRAMES + 4);
    let deadline = Instant::now() + FRAME_BUDGET;

    loop {
        if Instant::now() >= deadline {
            break;
        }
        if capture.has_error() {
            teardown(cage);
            panic!(
                "STEP 3 FAILED -- pipeline error after {frame_count} frames.\n\
                 Likely cause: PipeWire node dropped (compositor or portal died).\n\
                 Check: journalctl --user -xe | grep -E 'pipewire|portal|cage'"
            );
        }
        match capture.pull_encoded() {
            Ok(Some(data)) => {
                total_bytes += data.len();
                pull_times.push(Instant::now());
                frame_count += 1;
                if frame_count >= MIN_FRAMES {
                    break;
                }
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                teardown(cage);
                panic!(
                    "STEP 3 FAILED -- pull_encoded() returned error after {frame_count} frames: {e:#}\n\
                     Likely cause: GStreamer pipeline disconnected."
                );
            }
        }
    }

    if pull_times.len() >= 2 {
        let gaps: Vec<Duration> = pull_times
            .windows(2)
            .map(|w| w[1].duration_since(w[0]))
            .collect();
        let avg_gap_ms =
            gaps.iter().map(|d| d.as_secs_f64() * 1000.0).sum::<f64>() / gaps.len() as f64;
        let max_gap_ms = gaps
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .fold(0.0_f64, f64::max);
        eprintln!(
            "[wayland_e2e] STEP 4: {frame_count} frames, {total_bytes} bytes total, \
             avg inter-frame gap {avg_gap_ms:.1} ms, max {max_gap_ms:.1} ms"
        );
    } else {
        eprintln!(
            "[wayland_e2e] STEP 4: too few frames for latency summary ({frame_count} pulled)"
        );
    }

    teardown(cage);

    assert!(
        frame_count >= MIN_FRAMES,
        "STEP 3 FAILED -- only {frame_count}/{MIN_FRAMES} H.264 frames received in {}s.\n\
         Possible causes:\n\
         - Portal negotiation succeeded but pipewiresrc produced no buffers\n\
           (check WLR_RENDERER=pixman, node ID valid, PipeWire link established)\n\
         - H.264 encoder took > {}s to produce first output\n\
           (try a smaller resolution or x264enc fallback)\n\
         - PipeWire node created but not linked to encoder\n\
           Check: pw-dump | grep -A5 'pipewiresrc'",
        FRAME_BUDGET.as_secs(),
        FRAME_BUDGET.as_secs()
    );

    eprintln!("[wayland_e2e] PASS: {frame_count} frames captured, {total_bytes} bytes encoded");
}
