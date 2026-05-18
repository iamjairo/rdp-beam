//! Wayland end-to-end integration test for the Beam agent.
//!
//! # What this covers
//! - `WaylandDisplay::start()` end-to-end: cage compositor + per-session
//!   `dbus-daemon --session` + `xdg-desktop-portal-wlr`
//! - `WaylandCapture::new()` portal negotiation (gdbus → ScreenCast)
//! - GStreamer pipewiresrc → H.264 encoder pipeline
//! - Pulling ≥ 10 encoded H.264 frames within 2 seconds
//! - Inter-frame timing summary (avg gap between pull() calls that returned data)
//! - Clean teardown: drop tears down portal, bus, compositor and removes XDG_RUNTIME_DIR
//!
//! # What this does NOT cover
//! - Virtual input — not exercised here, see input.rs unit tests for shape
//! - Audio capture — `WaylandAudio` lives in its own test path
//! - Real desktop rendering inside cage — empty compositor is sufficient
//!
//! # How to run
//! Must run on a host with: cage, dbus-daemon, xdg-desktop-portal-wlr,
//! pipewire, wireplumber, GStreamer 1.28 + gstreamer1.0-pipewire,
//! libwayland-dev, libpipewire-0.3-dev, gdbus (from libglib2.0-bin).
//!
//!   cargo test -p beam-agent --features wayland --test wayland_e2e -- --ignored --nocapture
//!
//! Marked #[ignore] so normal CI (cargo test) never runs this.
//! Gated #[cfg(feature = "wayland")] so default builds skip compilation entirely.

#![cfg(feature = "wayland")]

use std::time::{Duration, Instant};

use beam_agent::wayland::{WaylandCapture, WaylandDisplay, WaylandDisplayConfig};

const FRAME_BUDGET: Duration = Duration::from_secs(2);
const MIN_FRAMES: usize = 10;
const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const FRAMERATE: u32 = 30;
const BITRATE: u32 = 1_000_000;

/// Unique display number per test process so concurrent runs don't collide
/// on `/tmp/beam-xdg-{N}`. Caps at 999 so the dir name stays short.
fn pick_display_num() -> u32 {
    900 + (std::process::id() % 100)
}

#[test]
#[ignore]
fn wayland_capture_e2e() {
    let display_num = pick_display_num();
    eprintln!(
        "[wayland_e2e] STEP 1: starting WaylandDisplay (cage + dbus-daemon + xdg-desktop-portal-wlr) on display {display_num}"
    );
    let display = match WaylandDisplay::start(WaylandDisplayConfig {
        display_num,
        width: WIDTH,
        height: HEIGHT,
    }) {
        Ok(d) => d,
        Err(e) => panic!(
            "STEP 1 FAILED -- WaylandDisplay::start error: {e:#}\n\
             Likely causes:\n\
             - cage not installed: apt-get install cage\n\
             - dbus-daemon not installed: apt-get install dbus\n\
             - xdg-desktop-portal-wlr not installed: apt-get install xdg-desktop-portal-wlr\n\
             - gdbus not in PATH: apt-get install libglib2.0-bin"
        ),
    };
    let wayland_display = display.wayland_display().to_string();
    let xdg_runtime_dir = display.xdg_runtime_dir().to_string();
    eprintln!(
        "[wayland_e2e] STEP 1 OK: compositor socket {xdg_runtime_dir}/{wayland_display}, portal on bus"
    );

    eprintln!("[wayland_e2e] STEP 2: opening WaylandCapture (gdbus → ScreenCast)");
    let capture = match WaylandCapture::new(
        &wayland_display,
        &xdg_runtime_dir,
        WIDTH,
        HEIGHT,
        FRAMERATE,
        BITRATE,
        None,
    ) {
        Ok(c) => c,
        Err(e) => panic!(
            "STEP 2 FAILED -- WaylandCapture::new() error: {e:#}\n\
             Likely causes:\n\
             - pipewiresrc GStreamer element missing: apt-get install gstreamer1.0-pipewire\n\
             - pipewire/wireplumber not running: systemctl --user start pipewire wireplumber\n\
             - portal session bus address mismatch (verify dbus-daemon child is alive)"
        ),
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

    while Instant::now() < deadline {
        if capture.has_error() {
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
            Err(e) => panic!(
                "STEP 3 FAILED -- pull_encoded() error after {frame_count} frames: {e:#}\n\
                 Likely cause: GStreamer pipeline disconnected."
            ),
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

    drop(capture);
    drop(display);

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
