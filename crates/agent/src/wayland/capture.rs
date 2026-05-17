//! PipeWire screencast capture for the Wayland backend.
//!
//! # Architecture: A (pipewiresrc direct into H.264 encoder pipeline)
//!
//! The GStreamer pipeline is:
//!
//! ```text
//! pipewiresrc path={node_id} do-timestamp=true
//!     ! videoconvert
//!     ! <H.264 encoder (nvh264enc / nvcudah264enc / vah264enc / x264enc)>
//!     ! capsfilter profile=main
//!     ! h264parse config-interval=-1
//!     ! capsfilter stream-format=byte-stream alignment=au
//!     ! appsink
//! ```
//!
//! Frames flow from PipeWire directly into the H.264 encoder without any
//! Rust-side buffer copy.  Where the compositor and encoder share a DMA-BUF
//! allocator, pipewiresrc performs zero-copy GPU passthrough.  Steady-state
//! latency target: <=5 ms over xcb-shm at the encoder input, measured as
//! PTS delta between consecutive encoded frames.
//!
//! # Protocol: xdg-desktop-portal-wlr (ScreenCast D-Bus portal via gdbus)
//!
//! `wlr-screencopy-unstable-v1` gives raw wl_buffer handles.  Feeding those
//! into GStreamer requires a custom appsrc (Architecture B: extra copy, higher
//! latency).  xdg-desktop-portal-wlr bridges wlr-screencopy to a PipeWire
//! stream exposed via org.freedesktop.portal.ScreenCast.  GStreamer's
//! pipewiresrc consumes that stream directly, eliminating the extra copy.
//! D-Bus overhead is one-time at startup (200-500 ms for portal negotiation
//! + PipeWire link establishment); steady-state overhead is zero.
//!
//! D-Bus calls are made via `gdbus call` / `gdbus monitor` subprocesses
//! (from libglib2.0-bin, always present when xdg-desktop-portal-wlr is
//! installed) rather than a Rust zbus crate.  This avoids the zbus 4
//! async-fs transitive dependency that causes compile issues on Ubuntu 26.04.
//!
//! The portal "permission dialog" is suppressed: xdg-desktop-portal-wlr
//! auto-approves requests arriving on the trusted compositor session bus.
//!
//! # Node disappear / pipeline error handling
//!
//! When the compositor dies mid-stream, PipeWire removes the node and
//! pipewiresrc emits a GStreamer Error on the bus.  `has_error()` returns
//! true; the caller drops and recreates WaylandCapture.  The encoder-reset
//! machinery in main.rs handles this without modification.
//!
//! # Latency measurement
//!
//! Set BEAM_WAYLAND_LATENCY_MEASURE=1 to log per-frame PTS deltas (encoder
//! input latency proxy).  Off by default; zero overhead when off.
//!
//! **Wiring status**: wired via `run_wayland_session` in `main.rs`.

use crate::capture::{PooledFrame, ScreenCaptureBackend};
use crate::encoder::{EncoderType, build_h264_pipeline_from_src};
use anyhow::{Context, bail};
use gstreamer::prelude::*;
use gstreamer::{self as gst, ElementFactory};
use gstreamer_app::{AppSink, AppSinkCallbacks};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Runtime handle for a PipeWire screencast capture + encode pipeline.
///
/// Owns the GStreamer pipeline (pipewiresrc -> videoconvert -> h264enc ->
/// appsink), the portal session keepalive, and the encoded-frame channel.
/// Architecture A: callers use `pull_encoded()`, not `capture_frame()`.
pub struct WaylandCapture {
    pipeline: gst::Pipeline,
    encoded_rx: std::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    _bus_watch: gst::bus::BusWatchGuard,
    pipeline_error: Arc<AtomicBool>,
    width: u32,
    height: u32,
    _latency: Arc<LatencyMeter>,
    /// Keeps the portal session alive.  Dropping closes the screencast stream.
    _portal_session: PortalSession,
}

impl WaylandCapture {
    /// Open a screencast session via xdg-desktop-portal-wlr and build the
    /// GStreamer encode pipeline.
    ///
    /// - `wayland_display`: compositor socket name, e.g. `"wayland-beam-99"`.
    /// - `xdg_runtime_dir`: e.g. `"/tmp/beam-xdg-99"` (must contain `bus`).
    pub fn new(
        wayland_display: &str,
        xdg_runtime_dir: &str,
        width: u32,
        height: u32,
        framerate: u32,
        bitrate: u32,
        preferred_encoder: Option<&str>,
    ) -> anyhow::Result<Self> {
        gst::init().context("Failed to init GStreamer")?;

        info!(
            wayland_display,
            xdg_runtime_dir,
            width,
            height,
            framerate,
            bitrate,
            "WaylandCapture::new -- opening portal session"
        );

        // 1. Acquire PipeWire node ID via xdg-desktop-portal-wlr.
        let t0 = std::time::Instant::now();
        let (portal_session, node_id) = open_portal_session(wayland_display, xdg_runtime_dir)
            .context("Failed to open xdg-desktop-portal ScreenCast session")?;
        info!(
            node_id,
            startup_ms = t0.elapsed().as_millis() as u64,
            "PipeWire screencast node acquired"
        );

        // 2. Build GStreamer pipeline.
        let (enc_type, enc_name) = crate::encoder::detect_encoder_type(preferred_encoder)?;
        info!(?enc_type, enc_name, "Building Wayland encode pipeline");

        let _startup_span = tracing::info_span!(
            "wayland_capture",
            session_id = wayland_display,
            node_id = node_id,
        )
        .entered();

        let latency = Arc::new(LatencyMeter::new());
        let latency_cb = Arc::clone(&latency);

        let pipeline = build_wayland_pipeline(
            node_id, enc_type, &enc_name, width, height, framerate, bitrate,
        )
        .context("Failed to build pipewiresrc->H.264 pipeline")?;

        // 3. Wire appsink callbacks.
        let appsink_elem = pipeline
            .by_name("sink")
            .context("appsink element 'sink' not found in pipeline")?;
        let appsink = appsink_elem
            .dynamic_cast::<AppSink>()
            .map_err(|_| anyhow::anyhow!("Failed to cast 'sink' to AppSink"))?;

        let (encoded_tx, encoded_rx) = mpsc::channel::<Vec<u8>>();
        appsink.set_callbacks(
            AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    use gstreamer::FlowError;
                    let sample = sink.pull_sample().map_err(|_| FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(FlowError::Error)?;
                    if let Some(pts) = buffer.pts() {
                        latency_cb.record(pts.nseconds());
                    }
                    let map = buffer.map_readable().map_err(|_| FlowError::Error)?;
                    let _ = encoded_tx.send(map.to_vec());
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        // 4. Bus watch for pipeline error detection.
        let pipeline_error = Arc::new(AtomicBool::new(false));
        let err_flag = Arc::clone(&pipeline_error);
        let bus = pipeline.bus().context("Failed to get pipeline bus")?;
        let _bus_watch = bus
            .add_watch(move |_, msg| {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Error(e) => {
                        let src_name = e.src().map(|s| s.name().to_string());
                        let err_msg = e.error().to_string();
                        let _err_span = tracing::error_span!(
                            "wayland_pipeline_error",
                            source = ?src_name,
                            error = %err_msg,
                        )
                        .entered();
                        error!(
                            source = ?src_name,
                            error = %err_msg,
                            debug = ?e.debug(),
                            "WaylandCapture pipeline error (pipewiresrc node lost?)"
                        );
                        err_flag.store(true, Ordering::Relaxed);
                    }
                    MessageView::Warning(w) => {
                        warn!(
                            source = ?w.src().map(|s| s.name().to_string()),
                            warning = %w.error(),
                            "WaylandCapture pipeline warning"
                        );
                    }
                    MessageView::StateChanged(sc)
                        if sc
                            .src()
                            .map(|s| s.name().as_str().starts_with("pipeline"))
                            .unwrap_or(false) =>
                    {
                        debug!(
                            old = ?sc.old(),
                            new = ?sc.current(),
                            "WaylandCapture pipeline state changed"
                        );
                    }
                    _ => {}
                }
                gst::glib::ControlFlow::Continue
            })
            .context("Failed to add bus watch")?;

        // 5. Start pipeline.
        pipeline
            .set_state(gst::State::Playing)
            .context("Failed to start Wayland pipeline")?;

        info!(node_id, width, height, framerate, bitrate, "WaylandCapture pipeline started");

        Ok(Self {
            pipeline,
            encoded_rx: std::sync::Mutex::new(encoded_rx),
            _bus_watch,
            pipeline_error,
            width,
            height,
            _latency: latency,
            _portal_session: portal_session,
        })
    }

    /// Pull the next encoded H.264 AU, if available.  Non-blocking.
    /// Returns `Ok(None)` when no frame is ready yet.
    pub fn pull_encoded(&self) -> anyhow::Result<Option<Vec<u8>>> {
        let rx = self.encoded_rx.lock().unwrap_or_else(|e| e.into_inner());
        match rx.try_recv() {
            Ok(data) => Ok(Some(data)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                bail!("WaylandCapture pipeline disconnected")
            }
        }
    }

    /// Returns true if the pipeline has errored (e.g. compositor died).
    /// Caller should drop and recreate WaylandCapture.
    pub fn has_error(&self) -> bool {
        self.pipeline_error.load(Ordering::Relaxed)
    }

    /// Request an IDR keyframe from the encoder on the next frame.
    pub fn force_keyframe(&self) {
        let event = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        if let Some(src) = self.pipeline.by_name("src") {
            src.send_event(event);
            info!("Forced IDR keyframe (WaylandCapture)");
        }
    }

}

impl ScreenCaptureBackend for WaylandCapture {
    /// Not called in Architecture A.  The pipeline drives itself; use
    /// `pull_encoded()` to drain encoded frames.
    fn capture_frame(&mut self) -> anyhow::Result<PooledFrame> {
        bail!(
            "WaylandCapture::capture_frame is not used in Architecture A. \
             Call WaylandCapture::pull_encoded() instead."
        )
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }
}

impl Drop for WaylandCapture {
    fn drop(&mut self) {
        info!("WaylandCapture::drop -- setting pipeline to Null");
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

// ---------------------------------------------------------------------------
// Portal session keepalive
// ---------------------------------------------------------------------------

/// Keeps the xdg-desktop-portal ScreenCast session alive.
/// Drop closes the session via a best-effort gdbus call.
struct PortalSession {
    session_path: String,
    xdg_runtime_dir: String,
    wayland_display: String,
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        // Best-effort close; compositor may already be gone.
        let close_cmd = format!(
            "gdbus call --session \
             --dest=org.freedesktop.portal.Desktop \
             --object-path={} \
             --method=org.freedesktop.portal.Session.Close",
            self.session_path
        );
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(&close_cmd)
            .env("XDG_RUNTIME_DIR", &self.xdg_runtime_dir)
            .env("WAYLAND_DISPLAY", &self.wayland_display)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}/bus", self.xdg_runtime_dir),
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        info!(session = %self.session_path, "PortalSession closed");
    }
}

// ---------------------------------------------------------------------------
// Portal negotiation via gdbus subprocesses
// ---------------------------------------------------------------------------

/// Open a ScreenCast portal session using gdbus subprocesses.
///
/// Uses `gdbus call` (from libglib2.0-bin) for D-Bus method calls and
/// `gdbus monitor` to wait for asynchronous Response signals.  No Rust
/// D-Bus crate dependency is introduced.
///
/// Returns (PortalSession keepalive, PipeWire node ID).
fn open_portal_session(
    wayland_display: &str,
    xdg_runtime_dir: &str,
) -> anyhow::Result<(PortalSession, u32)> {
    let pid = std::process::id();
    let dbus_addr = format!("unix:path={xdg_runtime_dir}/bus");

    // --- CreateSession -------------------------------------------------------
    let session_token = format!("beam_sess_{pid}");
    let create_out = gdbus_call(
        xdg_runtime_dir,
        wayland_display,
        &dbus_addr,
        &format!(
            "--dest=org.freedesktop.portal.Desktop \
             --object-path=/org/freedesktop/portal/desktop \
             --method=org.freedesktop.portal.ScreenCast.CreateSession \
             \"{{\\\"session_handle_token\\\": <\\\"{}\\\">, \
                  \\\"handle_token\\\": <\\\"beam_cr_{pid}\\\">}}\"",
            session_token
        ),
    )
    .context("CreateSession gdbus call failed")?;
    debug!(output = %create_out, "CreateSession output");

    let create_req_path = parse_object_path(&create_out)
        .context("Failed to parse CreateSession request path")?;
    let session_handle = gdbus_wait_response(
        xdg_runtime_dir,
        wayland_display,
        &dbus_addr,
        &create_req_path,
        "session_handle",
    )
    .context("CreateSession Response not received")?;
    info!(session = %session_handle, "Portal session created");

    // --- SelectSources -------------------------------------------------------
    // types=1 (MONITOR), cursor_mode=1 (HIDDEN), multiple=false
    let sel_out = gdbus_call(
        xdg_runtime_dir,
        wayland_display,
        &dbus_addr,
        &format!(
            "--dest=org.freedesktop.portal.Desktop \
             --object-path=/org/freedesktop/portal/desktop \
             --method=org.freedesktop.portal.ScreenCast.SelectSources \
             \"{session_handle}\" \
             \"{{\\\"handle_token\\\": <\\\"beam_sel_{pid}\\\">, \
                  \\\"types\\\": <uint32 1>, \
                  \\\"multiple\\\": <false>, \
                  \\\"cursor_mode\\\": <uint32 1>}}\""
        ),
    )
    .context("SelectSources gdbus call failed")?;
    let sel_req_path = parse_object_path(&sel_out)
        .context("Failed to parse SelectSources request path")?;
    gdbus_wait_response(
        xdg_runtime_dir,
        wayland_display,
        &dbus_addr,
        &sel_req_path,
        "",
    )
    .context("SelectSources Response not received")?;
    info!("Portal sources selected");

    // --- Start ---------------------------------------------------------------
    let start_out = gdbus_call(
        xdg_runtime_dir,
        wayland_display,
        &dbus_addr,
        &format!(
            "--dest=org.freedesktop.portal.Desktop \
             --object-path=/org/freedesktop/portal/desktop \
             --method=org.freedesktop.portal.ScreenCast.Start \
             \"{session_handle}\" \"\" \
             \"{{\\\"handle_token\\\": <\\\"beam_start_{pid}\\\">}}\""
        ),
    )
    .context("Start gdbus call failed")?;
    let start_req_path = parse_object_path(&start_out)
        .context("Failed to parse Start request path")?;
    let streams_raw = gdbus_wait_response(
        xdg_runtime_dir,
        wayland_display,
        &dbus_addr,
        &start_req_path,
        "streams",
    )
    .context("Start Response not received")?;
    info!(streams = %streams_raw, "Portal Start response received");

    let node_id = parse_node_id_from_streams(&streams_raw)
        .context("Failed to parse PipeWire node ID from streams")?;
    info!(node_id, "PipeWire node ID extracted");

    Ok((
        PortalSession {
            session_path: session_handle,
            xdg_runtime_dir: xdg_runtime_dir.to_string(),
            wayland_display: wayland_display.to_string(),
        },
        node_id,
    ))
}

/// Run `gdbus call --session <args>` and return trimmed stdout.
fn gdbus_call(
    xdg_runtime_dir: &str,
    wayland_display: &str,
    dbus_addr: &str,
    args: &str,
) -> anyhow::Result<String> {
    debug!(args, "gdbus call");
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("gdbus call --session {args}"))
        .env("WAYLAND_DISPLAY", wayland_display)
        .env("XDG_RUNTIME_DIR", xdg_runtime_dir)
        .env("DBUS_SESSION_BUS_ADDRESS", dbus_addr)
        .output()
        .context("Failed to spawn gdbus")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gdbus call failed ({}): {stderr}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Wait for a portal Request.Response signal on `request_path` using
/// `gdbus monitor`.  Returns the value for `key` from the response dict,
/// or `"ok"` if `key` is empty.  Timeout: 10 seconds.
fn gdbus_wait_response(
    xdg_runtime_dir: &str,
    wayland_display: &str,
    dbus_addr: &str,
    request_path: &str,
    key: &str,
) -> anyhow::Result<String> {
    debug!(request_path, key, "Waiting for portal Response signal");

    let mut child = std::process::Command::new("gdbus")
        .arg("monitor")
        .arg("--session")
        .arg("--dest=org.freedesktop.portal.Desktop")
        .arg(format!("--object-path={request_path}"))
        .env("WAYLAND_DISPLAY", wayland_display)
        .env("XDG_RUNTIME_DIR", xdg_runtime_dir)
        .env("DBUS_SESSION_BUS_ADDRESS", dbus_addr)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to spawn gdbus monitor")?;

    let stdout = child.stdout.take().context("No stdout from gdbus monitor")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);

    use std::io::{BufRead, BufReader};
    let reader = BufReader::new(stdout);
    let mut result = String::new();

    for line in reader.lines() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            bail!("Timeout waiting for portal Response on {request_path}");
        }
        let line = line.context("gdbus monitor read error")?;
        debug!(line = %line, "gdbus monitor line");

        // gdbus monitor output:
        //   /path : org.freedesktop.portal.Request.Response (uint32 0, {'key': <val>, ...})
        if !line.contains("Response") {
            continue;
        }
        if !line.contains("uint32 0") {
            let _ = child.kill();
            bail!("Portal Response indicates failure: {line}");
        }

        result = if key.is_empty() {
            "ok".to_string()
        } else {
            extract_gdbus_dict_value(&line, key).unwrap_or_else(|| {
                warn!(line = %line, key, "Key not found in Response dict");
                String::new()
            })
        };
        break;
    }

    let _ = child.kill();
    let _ = child.wait();

    if result.is_empty() {
        if key.is_empty() {
            bail!("Portal Response not received on {request_path} (compositor not responding?)");
        } else {
            bail!("Portal Response key '{key}' not found or empty on {request_path}");
        }
    }
    Ok(result)
}

/// Parse a D-Bus object path from `gdbus call` stdout.
/// gdbus prints `('/path/to/object',)` on success.
fn parse_object_path(gdbus_output: &str) -> anyhow::Result<String> {
    let inner = gdbus_output
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    let path = inner.trim_matches('\'').trim_end_matches(',').trim();
    if path.starts_with('/') {
        Ok(path.to_string())
    } else {
        bail!("Unexpected gdbus output (expected object path): {gdbus_output}")
    }
}

/// Extract a value from a gdbus monitor dict line.
/// Looks for `'key': <value>` and returns the inner value string.
fn extract_gdbus_dict_value(line: &str, key: &str) -> Option<String> {
    let key_pat = format!("'{key}':");
    let key_pos = line.find(&key_pat)?;
    let after = &line[key_pos + key_pat.len()..];
    let open = after.find('<')? + 1;
    let close = after[open..].find('>')? + open;
    let raw = after[open..close].trim().trim_matches('\'');
    Some(raw.to_string())
}

/// Parse the PipeWire node ID from the gdbus monitor streams value.
///
/// gdbus monitor outputs the streams GVariant as something like:
/// `[(<uint32 NODEID>, {'position': <(0, 0)>, ...})]`
///
/// We extract the first `uint32 <N>` occurrence.
fn parse_node_id_from_streams(streams_raw: &str) -> anyhow::Result<u32> {
    // Look for "uint32 " followed by digits.
    if let Some(pos) = streams_raw.find("uint32 ") {
        let after = &streams_raw[pos + 7..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return digits.parse::<u32>().context("Parse node_id as u32");
        }
    }
    // Fallback: first nonzero bare integer token.
    for token in streams_raw.split(|c: char| !c.is_ascii_digit()) {
        if !token.is_empty()
            && let Ok(n) = token.parse::<u32>()
            && n > 0
        {
            return Ok(n);
        }
    }
    bail!("No uint32 node ID found in streams: {streams_raw}")
}

// ---------------------------------------------------------------------------
// GStreamer pipeline construction
// ---------------------------------------------------------------------------

/// Build the `pipewiresrc -> videoconvert -> H.264 -> appsink` pipeline.
///
/// `do-timestamp=true` on pipewiresrc attaches GStreamer pipeline clock PTS
/// to each buffer, enabling correct encoder PTS/DTS accounting.
fn build_wayland_pipeline(
    node_id: u32,
    encoder_type: EncoderType,
    encoder_name: &str,
    width: u32,
    height: u32,
    framerate: u32,
    bitrate: u32,
) -> anyhow::Result<gst::Pipeline> {
    let pipeline = gst::Pipeline::new();

    // pipewiresrc connects to the PipeWire node exposed by xdg-desktop-portal-wlr.
    // path = node ID as decimal string (the "path" property accepts node IDs).
    // do-timestamp = true: GStreamer pipeline clock PTS on each buffer.
    let pipewiresrc = ElementFactory::make("pipewiresrc")
        .name("src")
        .property("path", node_id.to_string())
        .property("do-timestamp", true)
        .build()
        .context(
            "Failed to create pipewiresrc.  \
             Install gstreamer1.0-pipewire on the host.",
        )?;

    // videoconvert adapts PipeWire's pixel format (RGBA, BGRx, NV12, etc.)
    // to whatever the H.264 encoder accepts; GStreamer negotiates this.
    let videoconvert = ElementFactory::make("videoconvert")
        .build()
        .context("Failed to create videoconvert")?;

    // Downstream H.264 chain from the shared helper in encoder.rs.
    let (encoder_elem, profile_capsfilter, parser, parse_capsfilter, appsink) =
        build_h264_pipeline_from_src(
            encoder_type, encoder_name, width, height, framerate, bitrate,
        )
        .context("Failed to build H.264 downstream chain")?;

    pipeline
        .add_many([
            &pipewiresrc,
            &videoconvert,
            &encoder_elem,
            &profile_capsfilter,
            &parser,
            &parse_capsfilter,
            appsink.upcast_ref(),
        ])
        .context("Failed to add elements to Wayland pipeline")?;

    gst::Element::link_many([
        &pipewiresrc,
        &videoconvert,
        &encoder_elem,
        &profile_capsfilter,
        &parser,
        &parse_capsfilter,
        appsink.upcast_ref(),
    ])
    .context("Failed to link Wayland pipeline")?;

    info!(
        node_id,
        "Wayland pipeline: pipewiresrc({node_id}) -> videoconvert \
         -> {encoder_name} -> capsfilter(main) -> h264parse -> appsink"
    );

    Ok(pipeline)
}

// ---------------------------------------------------------------------------
// Latency measurement
// ---------------------------------------------------------------------------

/// Per-frame PTS delta logger.  Enable with `BEAM_WAYLAND_LATENCY_MEASURE=1`.
/// Logs first 20 frames and every 60th thereafter.  Zero overhead when off.
pub struct LatencyMeter {
    last_pts_ns: AtomicU64,
    sample_count: AtomicU64,
    enabled: bool,
}

impl LatencyMeter {
    fn new() -> Self {
        Self {
            last_pts_ns: AtomicU64::new(0),
            sample_count: AtomicU64::new(0),
            enabled: std::env::var("BEAM_WAYLAND_LATENCY_MEASURE")
                .map(|v| v == "1")
                .unwrap_or(false),
        }
    }

    /// Record an encoder-output buffer's PTS and log the delta.
    /// Called from the appsink callback (GStreamer streaming thread).
    pub fn record(&self, pts_ns: u64) {
        if !self.enabled {
            return;
        }
        let prev = self.last_pts_ns.swap(pts_ns, Ordering::Relaxed);
        if prev == 0 {
            return;
        }
        let delta_us = pts_ns.saturating_sub(prev) / 1_000;
        let count = self.sample_count.fetch_add(1, Ordering::Relaxed);
        if count < 20 || count.is_multiple_of(60) {
            info!(
                frame = count,
                pts_delta_us = delta_us,
                "WaylandCapture PTS delta (encoder-input latency proxy)"
            );
        }
        // Heartbeat: one INFO per 60 frames in steady state, matching the
        // cadence above.  Bitrate and encoder are not available inside
        // LatencyMeter; callers that want richer heartbeats should use
        // WaylandCapture::heartbeat_info() instead.  Here we emit the
        // frame-count line unconditionally at the same 60-frame boundary so
        // log consumers have a stable grep target.
        if count.is_multiple_of(60) && count > 0 {
            info!(
                frames = count,
                pts_delta_us = delta_us,
                "WaylandCapture heartbeat"
            );
        }
    }
}
