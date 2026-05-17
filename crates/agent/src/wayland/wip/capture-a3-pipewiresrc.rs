/// PipeWire screencast capture for the Wayland backend.
///
/// # Architecture chosen: A — pipewiresrc direct into H.264 encoder pipeline
///
/// The GStreamer pipeline is:
///
/// ```text
/// pipewiresrc path={node_id} do-timestamp=true
///     ! videoconvert
///     ! <H.264 encoder (nvh264enc / nvcudah264enc / vah264enc / x264enc)>
///     ! capsfilter profile=main
///     ! h264parse config-interval=-1
///     ! capsfilter stream-format=byte-stream alignment=au
///     ! appsink
/// ```
///
/// Frames flow from PipeWire (compositor → PipeWire graph → pipewiresrc) directly
/// into the H.264 encoder without any Rust-side buffer copy. Where the compositor
/// and the encoder share a DMA-BUF allocator, pipewiresrc performs zero-copy GPU
/// memory passthrough.
///
/// # Protocol chosen: xdg-desktop-portal-wlr (D-Bus ScreenCast portal)
///
/// Rationale: `wlr-screencopy-unstable-v1` (the wlroots Wayland extension) gives
/// us raw `wl_buffer` handles. Feeding those into GStreamer would require either
/// a custom appsrc (Architecture B: extra copy, higher latency) or a non-existent
/// `wlr-screencopy` GStreamer source element. The xdg-desktop-portal-wlr daemon
/// bridges `wlr-screencopy` to a PipeWire stream and exposes it via the standard
/// `org.freedesktop.portal.ScreenCast` D-Bus interface. The GStreamer `pipewiresrc`
/// element (from `gstreamer1.0-pipewire`) consumes that stream directly, eliminating
/// the extra copy. D-Bus overhead is one-time at startup (200–500 ms for portal
/// negotiation + PipeWire link establishment); steady-state overhead is zero because
/// the pipeline runs autonomously once started.
///
/// The portal's "permission dialog" is suppressed because xdg-desktop-portal-wlr
/// auto-approves requests that arrive on a trusted compositor session bus
/// (WAYLAND_DISPLAY is set to the compositor's private socket). No interactive
/// dialog appears for headless sessions.
///
/// # Node disappear / pipeline error handling
///
/// When the compositor dies mid-stream, PipeWire removes the node and
/// `pipewiresrc` emits a GStreamer `Error` message on the bus.
/// `has_error()` returns `true`; the caller (main.rs capture loop or
/// the A2 compositor lifecycle manager) recreates `WaylandCapture`.
/// The existing encoder-reset machinery in main.rs handles this path
/// without modification.
use crate::capture::{PooledFrame, ScreenCaptureBackend};
use crate::encoder::{EncoderType, build_h264_pipeline_from_src};
use anyhow::{Context, bail};
use gstreamer::prelude::*;
use gstreamer::{self as gst, ElementFactory};
use gstreamer_app::{AppSink, AppSinkCallbacks};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use tracing::{debug, error, info, warn};
use zbus::blocking::Connection as ZbusConnection;
use zbus::blocking::Proxy as ZbusProxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

/// Runtime handle for a PipeWire screencast capture pipeline.
///
/// Owns the GStreamer pipeline (pipewiresrc → videoconvert → h264enc → appsink),
/// the portal D-Bus session (kept alive so xdg-desktop-portal-wlr maintains the
/// screencast stream), and the encoded-frame channel.
pub struct WaylandCapture {
    pipeline: gst::Pipeline,
    encoded_rx: std::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    _bus_watch: gst::bus::BusWatchGuard,
    pipeline_error: Arc<AtomicBool>,
    width: u32,
    height: u32,
    /// Portal session object path: kept alive to maintain the screencast stream.
    /// Dropping this would cause xdg-desktop-portal-wlr to close the PipeWire node.
    _portal_session: PortalSession,
}

/// Represents an active xdg-desktop-portal ScreenCast session.
/// Drop closes the session (stops the PipeWire stream from the compositor side).
struct PortalSession {
    _conn: ZbusConnection,
    session_path: OwnedObjectPath,
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        // Best-effort close — compositor may already be gone
        if let Ok(proxy) = ZbusProxy::new_blocking(
            &self._conn,
            "org.freedesktop.portal.Desktop",
            self.session_path.as_ref(),
            "org.freedesktop.portal.Session",
        ) {
            let _: Result<(), _> = proxy.call("Close", &());
        }
    }
}

impl WaylandCapture {
    /// Open a screencast session on the compositor's session bus and build the
    /// GStreamer encode pipeline.
    ///
    /// `wayland_display` is e.g. `"wayland-beam-99"` (just the socket name, not the
    /// full path — the portal reads `XDG_RUNTIME_DIR` itself).
    /// `xdg_runtime_dir` is e.g. `"/tmp/beam-xdg-99"`.
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
            width, height, framerate, bitrate,
            "WaylandCapture::new — opening portal session"
        );

        // --- 1. Acquire a PipeWire node ID via xdg-desktop-portal-wlr -----------
        let (portal_session, node_id) = open_portal_session(wayland_display, xdg_runtime_dir)
            .context("Failed to open xdg-desktop-portal ScreenCast session")?;
        info!(node_id, "PipeWire screencast node acquired");

        // --- 2. Build GStreamer pipeline -----------------------------------------
        let (enc_type, enc_name) = crate::encoder::detect_encoder_type(preferred_encoder)?;
        info!(?enc_type, enc_name, "Building Wayland encode pipeline");

        let pipeline = build_wayland_pipeline(
            node_id, enc_type, &enc_name, width, height, framerate, bitrate,
        )
        .context("Failed to build PipeWire→H.264 pipeline")?;

        // --- 3. Wire appsink callbacks -------------------------------------------
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
                    use gstreamer_app::prelude::*;
                    use gstreamer::FlowError;
                    let sample = sink.pull_sample().map_err(|_| FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| FlowError::Error)?;
                    let _ = encoded_tx.send(map.to_vec());
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        // --- 4. Bus watch for error monitoring -----------------------------------
        let pipeline_error = Arc::new(AtomicBool::new(false));
        let pipeline_error_flag = Arc::clone(&pipeline_error);
        let bus = pipeline.bus().context("Failed to get pipeline bus")?;
        let _bus_watch = bus
            .add_watch(move |_, msg| {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Error(err) => {
                        error!(
                            source = ?err.src().map(|s| s.name().to_string()),
                            error = %err.error(),
                            debug = ?err.debug(),
                            "WaylandCapture GStreamer pipeline error"
                        );
                        pipeline_error_flag.store(true, Ordering::Relaxed);
                    }
                    MessageView::Warning(warn) => {
                        warn!(
                            source = ?warn.src().map(|s| s.name().to_string()),
                            warning = %warn.error(),
                            "WaylandCapture GStreamer pipeline warning"
                        );
                    }
                    MessageView::StateChanged(state)
                        if state
                            .src()
                            .map(|s| s.name().as_str().starts_with("pipeline"))
                            .unwrap_or(false) =>
                    {
                        debug!(
                            old = ?state.old(),
                            new = ?state.current(),
                            "WaylandCapture pipeline state changed"
                        );
                    }
                    _ => {}
                }
                gst::glib::ControlFlow::Continue
            })
            .context("Failed to add bus watch")?;

        // --- 5. Start pipeline ---------------------------------------------------
        pipeline
            .set_state(gst::State::Playing)
            .context("Failed to set Wayland pipeline to Playing")?;

        info!(node_id, width, height, framerate, bitrate, "WaylandCapture pipeline started");

        Ok(Self {
            pipeline,
            encoded_rx: std::sync::Mutex::new(encoded_rx),
            _bus_watch,
            pipeline_error,
            width,
            height,
            _portal_session: portal_session,
        })
    }

    /// Pull the next encoded H.264 AU from the pipeline, if available.
    /// Non-blocking; returns `Ok(None)` if no frame is ready yet.
    pub fn pull_encoded(&self) -> anyhow::Result<Option<Vec<u8>>> {
        let rx = self.encoded_rx.lock().unwrap_or_else(|e| e.into_inner());
        match rx.try_recv() {
            Ok(data) => Ok(Some(data)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                bail!("WaylandCapture encoder pipeline disconnected")
            }
        }
    }

    /// Returns true if the GStreamer pipeline has encountered an error
    /// (e.g. the PipeWire node disappeared because the compositor died).
    /// The caller should drop and recreate `WaylandCapture`.
    pub fn has_error(&self) -> bool {
        self.pipeline_error.load(Ordering::Relaxed)
    }

    /// Force the encoder to emit a keyframe (IDR) on the next frame.
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
    /// Not used in Architecture A — the pipeline drives itself.
    /// Callers using `WaylandCapture` must use `pull_encoded()` directly.
    fn capture_frame(&mut self) -> anyhow::Result<PooledFrame> {
        bail!(
            "WaylandCapture::capture_frame must not be called in Architecture A. \
             Use pull_encoded() to drain the self-driving GStreamer pipeline."
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
        info!("WaylandCapture::drop — setting pipeline to Null");
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

// ---------------------------------------------------------------------------
// Portal session negotiation (xdg-desktop-portal ScreenCast, D-Bus)
// ---------------------------------------------------------------------------

/// Open an xdg-desktop-portal ScreenCast session on the compositor's session bus.
/// Returns a `PortalSession` (keep-alive) and the PipeWire node ID for `pipewiresrc`.
///
/// The portal auto-approves requests when WAYLAND_DISPLAY matches a trusted
/// compositor socket controlled by the same UID — no interactive dialog appears.
fn open_portal_session(
    wayland_display: &str,
    xdg_runtime_dir: &str,
) -> anyhow::Result<(PortalSession, u32)> {
    // Point xdg-desktop-portal-wlr at the private compositor socket.
    // The portal reads WAYLAND_DISPLAY and XDG_RUNTIME_DIR at request time.
    std::env::set_var("WAYLAND_DISPLAY", wayland_display);
    std::env::set_var("XDG_RUNTIME_DIR", xdg_runtime_dir);
    // DBUS_SESSION_BUS_ADDRESS: if running inside a per-session dbus-daemon
    // (as A2's compositor module should arrange), this env var points to it.
    // If not set, zbus falls back to the standard socket path under XDG_RUNTIME_DIR.
    let dbus_socket = format!("unix:path={}/bus", xdg_runtime_dir);
    if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &dbus_socket);
        debug!(dbus_socket, "DBUS_SESSION_BUS_ADDRESS not set, using XDG_RUNTIME_DIR bus");
    }

    let conn = ZbusConnection::session().context(
        "Failed to connect to session D-Bus. \
         Ensure dbus-daemon is running inside the compositor session \
         (DBUS_SESSION_BUS_ADDRESS must point to the compositor's private bus).",
    )?;

    let portal = ZbusProxy::new_blocking(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
    )
    .context("Failed to create ScreenCast portal proxy")?;

    // --- CreateSession -------------------------------------------------------
    // cursor_mode=1 (hidden), source_type=1 (MONITOR)
    let session_handle_token = format!("beam_{}", std::process::id());
    let handle_token = format!("beam_req_{}", std::process::id());

    let mut create_options: std::collections::HashMap<&str, OwnedValue> =
        std::collections::HashMap::new();
    create_options.insert(
        "session_handle_token",
        OwnedValue::from(Value::from(session_handle_token.as_str())),
    );
    create_options.insert(
        "handle_token",
        OwnedValue::from(Value::from(handle_token.as_str())),
    );

    let request_path: OwnedObjectPath = portal
        .call("CreateSession", &(create_options,))
        .context("CreateSession call failed")?;
    debug!(?request_path, "CreateSession request submitted");

    // Wait for the Request.Response signal (synchronous receive)
    let response = wait_for_response(&conn, &request_path)
        .context("CreateSession response failed")?;
    let session_path: OwnedObjectPath = response
        .get("session_handle")
        .and_then(|v| v.downcast_ref::<OwnedObjectPath>().ok().cloned())
        .context("CreateSession response missing session_handle")?;
    info!(?session_path, "ScreenCast session created");

    // --- SelectSources -------------------------------------------------------
    // source_type=1 (MONITOR), cursor_mode=1 (HIDDEN), multiple=false
    let mut select_options: std::collections::HashMap<&str, OwnedValue> =
        std::collections::HashMap::new();
    select_options.insert(
        "handle_token",
        OwnedValue::from(Value::from(format!("beam_sel_{}", std::process::id()).as_str())),
    );
    select_options.insert(
        "types",
        OwnedValue::from(Value::from(1u32)), // MONITOR
    );
    select_options.insert(
        "multiple",
        OwnedValue::from(Value::from(false)),
    );
    select_options.insert(
        "cursor_mode",
        OwnedValue::from(Value::from(1u32)), // HIDDEN
    );

    let sel_request_path: OwnedObjectPath = portal
        .call("SelectSources", &(session_path.as_ref(), select_options))
        .context("SelectSources call failed")?;
    wait_for_response(&conn, &sel_request_path).context("SelectSources response failed")?;
    info!("ScreenCast sources selected");

    // --- Start ---------------------------------------------------------------
    let mut start_options: std::collections::HashMap<&str, OwnedValue> =
        std::collections::HashMap::new();
    start_options.insert(
        "handle_token",
        OwnedValue::from(Value::from(format!("beam_start_{}", std::process::id()).as_str())),
    );

    // parent_window is empty string for headless sessions
    let start_request_path: OwnedObjectPath = portal
        .call("Start", &(session_path.as_ref(), "", start_options))
        .context("Start call failed")?;
    let start_response = wait_for_response(&conn, &start_request_path)
        .context("Start response failed")?;

    // Extract PipeWire node ID from the streams array
    // Response format: { "streams": [(node_id: u32, props: {})], ... }
    let node_id = extract_node_id(&start_response)
        .context("Failed to extract PipeWire node ID from Start response")?;

    info!(node_id, "PipeWire ScreenCast stream started");

    Ok((
        PortalSession {
            _conn: conn,
            session_path,
        },
        node_id,
    ))
}

/// Block until the portal Request object at `request_path` emits a Response signal.
/// Returns the response results dict on success (response code 0),
/// or an error for response codes 1 (user cancelled) or 2 (other error).
fn wait_for_response(
    conn: &ZbusConnection,
    request_path: &OwnedObjectPath,
) -> anyhow::Result<std::collections::HashMap<String, OwnedValue>> {
    use zbus::blocking::MessageIterator;

    // Subscribe to signals from the specific Request object path.
    // We use a match rule so we only wake on this request's Response.
    let match_rule = format!(
        "type='signal',interface='org.freedesktop.portal.Request',\
         member='Response',path='{}'",
        request_path.as_str()
    );
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "AddMatch",
        &(match_rule.as_str(),),
    )
    .context("Failed to add D-Bus match rule")?;

    // Poll the message stream with a timeout
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let iter = MessageIterator::from(conn);
    for msg in iter {
        if std::time::Instant::now() > deadline {
            bail!("Timeout waiting for portal Request.Response from {request_path}");
        }
        let msg = msg.context("D-Bus message error")?;
        // Check this is the Response signal from our request path
        let hdr = msg.header();
        if hdr.member().map(|m| m.as_str()) != Some("Response") {
            continue;
        }
        if hdr.path().map(|p| p.as_str()) != Some(request_path.as_str()) {
            continue;
        }
        // Body: (response_code: u32, results: a{sv})
        let (response_code, results): (u32, std::collections::HashMap<String, OwnedValue>) =
            msg.body().deserialize().context("Failed to deserialize Response body")?;
        match response_code {
            0 => return Ok(results),
            1 => bail!("Portal request cancelled by user (xdg-desktop-portal-wlr auto-approve may not be configured)"),
            code => bail!("Portal request failed with code {code}"),
        }
    }
    bail!("D-Bus message stream ended before Response arrived")
}

/// Extract the PipeWire node ID from a portal Start response.
/// The response contains `"streams": Array<(u32, Dict<String, Variant>)>`.
fn extract_node_id(
    results: &std::collections::HashMap<String, OwnedValue>,
) -> anyhow::Result<u32> {
    // The streams field is an array of structs: [(node_id: u32, props: a{sv})]
    // zvariant represents this as Value::Array of Value::Structure.
    let streams_val = results
        .get("streams")
        .context("Start response missing 'streams' key")?;

    // Attempt to downcast to Array then extract first element's node_id
    use zbus::zvariant::Value;
    let inner: &Value = streams_val.as_ref();
    if let Value::Array(arr) = inner {
        if let Some(first) = arr.first() {
            if let Value::Structure(s) = first {
                let fields = s.fields();
                if let Some(Value::U32(node_id)) = fields.first() {
                    return Ok(*node_id);
                }
            }
        }
    }

    bail!(
        "Could not parse PipeWire node ID from streams value: {:?}",
        streams_val
    )
}

// ---------------------------------------------------------------------------
// GStreamer pipeline construction
// ---------------------------------------------------------------------------

/// Build the pipewiresrc→videoconvert→H.264→appsink pipeline.
///
/// Layout mirrors the Xorg encoder pipeline in encoder.rs, with `pipewiresrc`
/// replacing `appsrc`. `do-timestamp=true` instructs pipewiresrc to attach
/// pipeline clock timestamps to each buffer, which is required for the encoder's
/// PTS-based DTS calculation to be correct.
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

    // pipewiresrc: connects to the compositor's PipeWire node.
    // do-timestamp=true: attach GStreamer clock timestamps to buffers (required
    //   for correct PTS/DTS computation in downstream encoder elements).
    // path: the PipeWire node id as string — pipewiresrc uses "path" property.
    let pipewiresrc = ElementFactory::make("pipewiresrc")
        .name("src")
        .property("path", node_id.to_string())
        .property("do-timestamp", true)
        .build()
        .context("Failed to create pipewiresrc. Install gstreamer1.0-pipewire.")?;

    // videoconvert: handles PipeWire's native pixel format (typically BGRx or
    // NV12 from the compositor) → format expected by the H.264 encoder.
    // For nvidia/cuda encoders the downstream capsfilter will negotiate BGRA/NV12.
    // For VA-API and software the capsfilter negotiates NV12/I420.
    let videoconvert = ElementFactory::make("videoconvert")
        .build()
        .context("Failed to create videoconvert")?;

    // Build encoder and downstream parser + capsfilters using the shared helper.
    let (encoder_elem, profile_capsfilter, parser, parse_capsfilter, appsink) =
        build_h264_pipeline_from_src(encoder_type, encoder_name, width, height, framerate, bitrate)
            .context("Failed to build H.264 pipeline elements")?;

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
    .context("Failed to link Wayland pipeline elements")?;

    info!(
        node_id,
        "Wayland pipeline: pipewiresrc({node_id}) → videoconvert → {encoder_name} \
         → capsfilter(main) → h264parse → appsink"
    );

    Ok(pipeline)
}

// ---------------------------------------------------------------------------
// Latency measurement (opt-in via BEAM_WAYLAND_LATENCY_MEASURE=1)
// ---------------------------------------------------------------------------

/// Measure and log PTS deltas between consecutive encoded frames.
/// Enable with BEAM_WAYLAND_LATENCY_MEASURE=1. Overhead is negligible —
/// one AtomicU64 load/store per encoded frame.
pub(crate) struct LatencyMeter {
    last_pts_ns: std::sync::atomic::AtomicU64,
    sample_count: std::sync::atomic::AtomicU64,
    enabled: bool,
}

impl LatencyMeter {
    pub fn new() -> Self {
        Self {
            last_pts_ns: std::sync::atomic::AtomicU64::new(0),
            sample_count: std::sync::atomic::AtomicU64::new(0),
            enabled: std::env::var("BEAM_WAYLAND_LATENCY_MEASURE")
                .map(|v| v == "1")
                .unwrap_or(false),
        }
    }

    /// Record a buffer's PTS and log the delta. Call from appsink new_sample callback.
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
                "WaylandCapture frame PTS delta (encoder input latency proxy)"
            );
        }
    }
}
