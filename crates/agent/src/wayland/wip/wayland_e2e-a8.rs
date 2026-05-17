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
//!   cargo test -p beam-agent --features wayland -- --ignored --test wayland_e2e --nocapture
//!
//! Marked #[ignore] so normal CI (cargo test) never runs this.
//! Gated #[cfg(feature = "wayland")] so default builds skip compilation entirely.
//!
//! Once A2 (WaylandDisplay::start()) lands, replace the manual cage spawn below
//! with a call to WaylandDisplay::start() and update the teardown accordingly.

#![cfg(feature = "wayland")]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use beam_agent::wayland::WaylandCapture;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const WAYLAND_DISPLAY: &str = "wayland-beam-test";
const XDG_RUNTIME_DIR: &str = "/tmp/beam-xdg-test";
const SOCKET_POLL_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(100);
const FRAME_BUDGET: Duration = Duration::from_secs(2);
const MIN_FRAMES: usize = 10;
/// Encoder resolution — small to keep startup fast.
const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const FRAMERATE: u32 = 30;
const BITRATE: u32 = 1_000_000;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Spawn a cage compositor with a private Wayland socket.
///
/// cage needs at least one executable as its argument.  We pass `/bin/true`
/// so it has something to run; cage keeps the compositor alive even after
/// the child exits (it waits for the next client).
fn spawn_cage() -> Result<Child, String> {
    // Ensure XDG_RUNTIME_DIR exists with correct permissions (0700).
    std::fs::create_dir_all(XDG_RUNTIME_DIR)
        .map_err(|e| format!("Failed to create XDG_RUNTIME_DIR {XDG_RUNTIME_DIR}: {e}"))?;
    std::fs::set_permissions(
        XDG_RUNTIME_DIR,
        std::fs::Permissions::from_mode(0o700),
    )
    .map_err(|e| format!("Failed to chmod XDG_RUNTIME_DIR: {e}"))?;

    Command::new("cage")
        .arg("--")
        .arg("/bin/true")
        .env("WAYLAND_DISPLAY", WAYLAND_DISPLAY)
        .env("XDG_RUNTIME_DIR", XDG_RUNTIME_DIR)
        // Suppress cage's own rendering noise on headless hosts.
        .env("WLR_BACKENDS", "headless")
        .env("WLR_RENDERER", "pixman")
        // Prevent cage from looking for a nested Wayland compositor.
        .env_remove("WAYLAND_DISPLAY_PARENT")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!(
            "STEP 1 FAILED -- compositor spawn: cage not found or spawn error: {e}\n\
             Fix: apt-get install cage"
        ))
}

/// Poll for the Wayland socket to appear (up to SOCKET_POLL_TIMEOUT).
fn wait_for_socket() -> Result<(), String> {
    let socket_path = format!("{XDG_RUNTIME_DIR}/{WAYLAND_DISPLAY}");
    let deadline = Instant::now() + SOCKET_POLL_TIMEOUT;
    while Instant::now() < deadline {
        if Path::new(&socket_path).exists() {
            return Ok(());
        }
        std::thread::sleep(SOCKET_POLL_INTERVAL);
    }
    Err(format!(
        "STEP 1 FAILED -- compositor socket never appeared at {socket_path} within {}s.\n\
         Likely causes:\n\
         - cage failed to start (check WLR_BACKENDS=headless, WLR_RENDERER=pixman)\n\
         - XDG_RUNTIME_DIR permissions wrong (needs 0700)\n\
         - Missing headless backend: apt-get install libwlroots-dev",
        SOCKET_POLL_TIMEOUT.as_secs()
    ))
}

/// Terminate cage and clean up the XDG_RUNTIME_DIR.
fn teardown(mut cage: Child) {
    // SIGTERM the compositor; ignore errors (already dead is fine).
    let _ = cage.kill();
    let _ = cage.wait();
    // Best-effort cleanup; leave no state for next run.
    let _ = std::fs::remove_dir_all(XDG_RUNTIME_DIR);
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn wayland_capture_e2e() {
    // STEP 1: spawn cage.
    eprintln!("[wayland_e2e] STEP 1: spawning cage compositor");
    let cage = match spawn_cage() {
        Ok(c) => c,
        Err(msg) => panic!("{msg}"),
    };

    // Wait for socket readiness.
    if let Err(msg) = wait_for_socket() {
        teardown(cage);
        panic!("{msg}");
    }
    eprintln!("[wayland_e2e] STEP 1 OK: socket {XDG_RUNTIME_DIR}/{WAYLAND_DISPLAY} is ready");

    // STEP 2: open WaylandCapture (portal negotiation + pipeline start).
    eprintln!("[wayland_e2e] STEP 2: opening WaylandCapture (portal + pipewiresrc)");
    let capture = match WaylandCapture::new(
        WAYLAND_DISPLAY,
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

    // STEP 3: pull at least MIN_FRAMES encoded H.264 frames within FRAME_BUDGET.
    eprintln!("[wayland_e2e] STEP 3: pulling >= {MIN_FRAMES} H.264 frames in {}s", FRAME_BUDGET.as_secs());

    let mut frame_count = 0usize;
    let mut total_bytes = 0usize;
    // We track wall-clock timestamps of each successful pull to compute inter-frame gaps.
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
                // No frame ready yet; yield briefly.
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

    // STEP 4: latency summary.
    if pull_times.len() >= 2 {
        let gaps: Vec<Duration> = pull_times
            .windows(2)
            .map(|w| w[1].duration_since(w[0]))
            .collect();
        let avg_gap_ms = gaps.iter().map(|d| d.as_secs_f64() * 1000.0).sum::<f64>()
            / gaps.len() as f64;
        let max_gap_ms = gaps
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .fold(0.0_f64, f64::max);
        eprintln!(
            "[wayland_e2e] STEP 4: latency summary -- {frame_count} frames, \
             {total_bytes} bytes total, avg inter-frame gap {avg_gap_ms:.1} ms, \
             max {max_gap_ms:.1} ms"
        );
    } else {
        eprintln!("[wayland_e2e] STEP 4: too few frames for latency summary ({frame_count} pulled)");
    }

    // STEP 5: assertion + teardown.
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

    eprintln!(
        "[wayland_e2e] PASS: {frame_count} frames captured, {total_bytes} bytes encoded"
    );
}
