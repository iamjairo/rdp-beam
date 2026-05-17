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
/// Frames flow from PipeWire (compositor -> PipeWire graph -> pipewiresrc) directly
/// into the H.264 encoder without any Rust-side buffer copy. Where the compositor
/// and the encoder share a DMA-BUF allocator, pipewiresrc performs zero-copy GPU
/// memory passthrough. Steady-state latency target: <= 5 ms over xcb-shm at the
/// encoder input, measured as PTS delta between consecutive encoded frames.
///
/// # Protocol chosen: xdg-desktop-portal-wlr (D-Bus ScreenCast portal)
///
/// Rationale: `wlr-screencopy-unstable-v1` (the wlroots Wayland extension) gives
/// us raw `wl_buffer` handles. Feeding those into GStreamer would require either
/// a custom appsrc (Architecture B: extra Rust-side copy, higher latency) or a
/// non-existent `wlr-screencopy` GStreamer source element. The xdg-desktop-portal-wlr
/// daemon bridges `wlr-screencopy` to a PipeWire stream and exposes it via the
/// standard `org.freedesktop.portal.ScreenCast` D-Bus interface. The GStreamer
/// `pipewiresrc` element (gstreamer1.0-pipewire) consumes that stream directly,
/// eliminating the extra copy. D-Bus overhead is one-time at startup (200-500 ms
/// for portal negotiation + PipeWire link establishment); steady-state overhead is
/// zero because the pipeline runs autonomously once started.
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
/// `has_error()` returns `true`; the caller recreates `WaylandCapture`.
/// The existing encoder-reset machinery in main.rs handles this path
/// without modification.
///
/// # Latency measurement
///
/// Set BEAM_WAYLAND_LATENCY_MEASURE=1 to log per-frame PTS deltas (encoder
/// input latency proxy). Enable for profiling; off by default.
use crate::capture::{PooledFrame, ScreenCaptureBackend};
use crate::encoder::{EncoderType, build_h264_pipeline_from_src};
use anyhow::{Context, bail};
use gstreamer::prelude::*;
use gstreamer::{self as gst, ElementFactory};
use gstreamer_app::{AppSink, AppSinkCallbacks};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use tracing::{debug, error, info, warn};
use zbus::blocking::Connection as ZbusConnection;
use zbus::blocking::Proxy as ZbusProxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

/// Runtime handle for a PipeWire screencast capture pipeline.
///
/// Owns the GStreamer pipeline (pipewiresrc -> videoconvert -> h264enc -> appsink),
/// the portal D-Bus session (kept alive so xdg-desktop-portal-wlr maintains the
/// screencast stream), and the encoded-frame channel.
pub struct WaylandCapture {
    pipeline: gst::Pipeline,
    encoded_rx: std::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    _bus_watch: gst::bus::BusWatchGuard,
    pipeline_error: Arc<AtomicBool>,
    width: u32,
    height: u32,
    latency: Arc<LatencyMeter>,
    /// Portal session object path: kept alive to maintain the screencast stream.
    /// Dropping this causes xdg-desktop-portal-wlr to close the PipeWire node.
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
        // Best-effort close — compositor may already be gone.
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
    /// `wayland_display` is e.g. `"wayland-beam-99"` (socket name only).
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
            width,
            height,
            framerate,
            bitrate,
            "WaylandCapture::new -- opening portal session"
        );

        // 1. Acquire a PipeWire node ID via xdg-desktop-portal-wlr.
        let t0 = std::time::Instant::now();
        let (portal_session, node_id) = open_portal_session(wayland_display, xdg_runtime_dir)
            .context("Failed to open xdg-desktop-portal ScreenCast session")?;
        info!(
            node_id,
            startup_ms = t0.elapsed().as_millis() as u64,
            "PipeWire screencast node acquired"
        );

        // 2. Detect encoder and build GStreamer pipeline.
        let (enc_type, enc_name) = crate::encoder::detect_encoder_type(preferred_encoder)?;
        info!(?enc_type, enc_name, "Building Wayland encode pipeline");

        let latency = Arc::new(LatencyMeter::new());
        let latency_for_cb = Arc::clone(&latency);

        let pipeline =
            build_wayland_pipeline(node_id, enc_type, &enc_name, width, height, framerate, bitrate)
                .context("Failed to build PipeWire->H.264 pipeline")?;

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
                    use gstreamer_app::prelude::*;
                    let sample = sink.pull_sample().map_err(|_| FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(FlowError::Error)?;
                    // Record PTS for latency measurement.
                    if let Some(pts) = buffer.pts() {
                        latency_for_cb.record(pts.nseconds());
                    }
                    let map = buffer.map_readable().map_err(|_| FlowError::Error)?;
                    let _ = encoded_tx.send(map.to_vec());
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        // 4. Bus watch for error monitoring.
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
                            "WaylandCapture pipeline error (pipewiresrc node lost?)"
                        );
                        pipeline_error_flag.store(true, Ordering::Relaxed);
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
            .context("Failed to set Wayland pipeline to Playing")?;

        info!(
            node_id,
            width,
            height,
            framerate,
            bitrate,
            "WaylandCapture pipeline started"
        );

        Ok(Self {
            pipeline,
            encoded_rx: std::sync::Mutex::new(encoded_rx),
            _bus_watch,
            pipeline_error,
            width,
            height,
            latency,
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

    /// Latency meter (for diagnostic logging).
    pub fn latency_meter(&self) -> &LatencyMeter {
        &self.latency
    }
}

impl ScreenCaptureBackend for WaylandCapture {
    /// Not used in Architecture A -- the pipeline drives itself.
    /// Callers on the Wayland path must use `pull_encoded()` directly.
    fn capture_frame(&mut self) -> anyhow::Result<PooledFrame> {
        bail!(
            "WaylandCapture::capture_frame must not be called in Architecture A. \
             Use WaylandCapture::pull_encoded() to drain the self-driving GStreamer pipeline."
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
// Portal session negotiation (xdg-desktop-portal ScreenCast, D-Bus)
// ---------------------------------------------------------------------------

/// Open an xdg-desktop-portal ScreenCast session on the compositor's session bus.
/// Returns a `PortalSession` (keep-alive) and the PipeWire node ID for pipewiresrc.
///
/// The portal auto-approves requests when WAYLAND_DISPLAY matches a trusted
/// compositor socket controlled by the same UID -- no interactive dialog appears
/// for headless sessions.
fn open_portal_session(
    wayland_display: &str,
    xdg_runtime_dir: &str,
) -> anyhow::Result<(PortalSession, u32)> {
    // Set env vars so xdg-desktop-portal-wlr knows which compositor to connect to.
    // SAFETY: single-threaded at this point in agent startup; no other thread
    // reads WAYLAND_DISPLAY/XDG_RUNTIME_DIR concurrently.
    // TODO: replace with set_var_locked when stabilized (rust-lang/rust #105715).
    #[allow(deprecated)]
    {
        std::env::set_var("WAYLAND_DISPLAY", wayland_display);
        std::env::set_var("XDG_RUNTIME_DIR", xdg_runtime_dir);
    }

    // If no session bus address is set, use the standard path under XDG_RUNTIME_DIR.
    // A2's compositor module should have started dbus-daemon and set this.
    if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
        let dbus_socket = format!("unix:path={xdg_runtime_dir}/bus");
        debug!(dbus_socket, "DBUS_SESSION_BUS_ADDRESS not set, using XDG_RUNTIME_DIR/bus");
        #[allow(deprecated)]
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &dbus_socket);
    }

    let conn = ZbusConnection::session().context(
        "Failed to connect to session D-Bus. \
         A2's compositor module must start dbus-daemon and set \
         DBUS_SESSION_BUS_ADDRESS before WaylandCapture::new is called.",
    )?;

    let portal = ZbusProxy::new_blocking(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
    )
    .context("Failed to create ScreenCast portal proxy")?;

    // Unique token per request/session to avoid collisions if agent restarts fast.
    let pid = std::process::id();

    // --- CreateSession -------------------------------------------------------
    let session_handle_token = format!("beam_sess_{pid}");
    let handle_token = format!("beam_create_{pid}");

    let mut create_opts: std::collections::HashMap<&str, OwnedValue> =
        std::collections::HashMap::new();
    create_opts.insert(
        "session_handle_token",
        OwnedValue::try_from(Value::from(session_handle_token.as_str()))
            .context("session_handle_token OwnedValue")?,
    );
    create_opts.insert(
        "handle_token",
        OwnedValue::try_from(Value::from(handle_token.as_str()))
            .context("handle_token OwnedValue")?,
    );

    let create_request: OwnedObjectPath = portal
        .call("CreateSession", &(create_opts,))
        .context("CreateSession D-Bus call failed")?;
    debug!(?create_request, "CreateSession submitted");

    let create_resp = wait_for_response(&conn, &create_request)
        .context("CreateSession response failed")?;
    let session_path: OwnedObjectPath = create_resp
        .get("session_handle")
        .and_then(|v| {
            let inner: &Value = v.as_ref();
            if let Value::ObjectPath(p) = inner {
                Some(OwnedObjectPath::from(p.clone()))
            } else {
                None
            }
        })
        .context("CreateSession response missing 'session_handle'")?;
    info!(?session_path, "ScreenCast session created");

    // --- SelectSources -------------------------------------------------------
    // types=1 (MONITOR), cursor_mode=1 (HIDDEN), multiple=false
    let select_token = format!("beam_sel_{pid}");
    let mut sel_opts: std::collections::HashMap<&str, OwnedValue> =
        std::collections::HashMap::new();
    sel_opts.insert(
        "handle_token",
        OwnedValue::try_from(Value::from(select_token.as_str()))
            .context("select handle_token")?,
    );
    sel_opts.insert(
        "types",
        OwnedValue::try_from(Value::from(1u32)).context("types")?,
    );
    sel_opts.insert(
        "multiple",
        OwnedValue::try_from(Value::from(false)).context("multiple")?,
    );
    sel_opts.insert(
        "cursor_mode",
        OwnedValue::try_from(Value::from(1u32)).context("cursor_mode")?,
    );

    let sel_request: OwnedObjectPath = portal
        .call("SelectSources", &(session_path.as_ref(), sel_opts))
        .context("SelectSources D-Bus call failed")?;
    wait_for_response(&conn, &sel_request).context("SelectSources response failed")?;
    info!("ScreenCast sources selected");

    // --- Start ---------------------------------------------------------------
    let start_token = format!("beam_start_{pid}");
    let mut start_opts: std::collections::HashMap<&str, OwnedValue> =
        std::collections::HashMap::new();
    start_opts.insert(
        "handle_token",
        OwnedValue::try_from(Value::from(start_token.as_str()))
            .context("start handle_token")?,
    );

    // parent_window is empty string for headless sessions (no parent window).
    let start_request: OwnedObjectPath = portal
        .call("Start", &(session_path.as_ref(), "", start_opts))
        .context("Start D-Bus call failed")?;
    let start_resp = wait_for_response(&conn, &start_request).context("Start response failed")?;

    let node_id =
        extract_node_id(&start_resp).context("Failed to extract PipeWire node ID from Start response")?;
    info!(node_id, "PipeWire ScreenCast stream started");

    Ok((
        PortalSession {
            _conn: conn,
            session_path,
        },
        node_id,
    ))
}

/// Block until the portal Request object at `request_path` emits a `Response` signal.
/// Returns the response results dict on success (code 0).
/// Errors on code 1 (cancelled) or 2 (error), or on timeout (10 s).
fn wait_for_response(
    conn: &ZbusConnection,
    request_path: &OwnedObjectPath,
) -> anyhow::Result<std::collections::HashMap<String, OwnedValue>> {
    // Register a match rule so the connection receives signals from this path.
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
    .context("Failed to add D-Bus match rule for portal response")?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);

    // Iterate incoming messages; filter for our specific Response signal.
    for msg_result in conn.inner().receive_message_impl(
        std::time::Duration::from_millis(100)
    ) {
        if std::time::Instant::now() > deadline {
            bail!("Timeout waiting for portal Response on {}", request_path.as_str());
        }
        let msg = match msg_result {
            Ok(m) => m,
            Err(zbus::Error::InputOutput(_)) => continue, // transient I/O
            Err(e) => return Err(e).context("D-Bus receive error"),
        };
        let hdr = msg.header();
        if hdr.member().map(|m| m.as_str()) != Some("Response") {
            continue;
        }
        if hdr.path().map(|p| p.as_str()) != Some(request_path.as_str()) {
            continue;
        }
        // Body: (u response_code, a{sv} results)
        let (code, results): (u32, std::collections::HashMap<String, OwnedValue>) = msg
            .body()
            .deserialize()
            .context("Failed to deserialize portal Response body")?;
        return match code {
            0 => Ok(results),
            1 => bail!("Portal request cancelled (xdg-desktop-portal-wlr auto-approve not configured?)"),
            c => bail!("Portal request failed with response code {c}"),
        };
    }
    bail!("D-Bus message stream ended before portal Response arrived")
}

/// Extract the PipeWire node ID from a portal `Start` response.
///
/// The `"streams"` field is an array of `(u32 node_id, a{sv} props)` structs.
fn extract_node_id(
    results: &std::collections::HashMap<String, OwnedValue>,
) -> anyhow::Result<u32> {
    let streams_val = results
        .get("streams")
        .context("Start response missing 'streams' key")?;

    // zvariant represents a{rv} / a(ua{sv}) as Value::Array of Value::Structure.
    let inner: &Value = streams_val.as_ref();
    if let Value::Array(arr) = inner {
        for item in arr.iter() {
            if let Value::Structure(s) = item {
                let fields = s.fields();
                if let Some(Value::U32(node_id)) = fields.first() {
                    return Ok(*node_id);
                }
            }
        }
    }

    bail!(
        "Could not parse PipeWire node ID from Start response 'streams': {:?}",
        streams_val
    )
}

// ---------------------------------------------------------------------------
// GStreamer pipeline construction
// ---------------------------------------------------------------------------

/// Build the `pipewiresrc -> videoconvert -> H.264 -> appsink` pipeline.
///
/// `do-timestamp=true` on pipewiresrc attaches pipeline clock PTS to each buffer,
/// enabling correct encoder PTS/DTS accounting without an external clock source.
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

    // pipewiresrc: source element that connects to the PipeWire node exposed by
    // xdg-desktop-portal-wlr. `path` is the node ID as a string.
    // `do-timestamp=true` attaches GStreamer pipeline clock timestamps to buffers.
    let pipewiresrc = ElementFactory::make("pipewiresrc")
        .name("src")
        .property("path", node_id.to_string())
        .property("do-timestamp", true)
        .build()
        .context(
            "Failed to create pipewiresrc element. \
             Ensure gstreamer1.0-pipewire (or gstreamer1.0-plugins-good with PipeWire) \
             is installed on the host.",
        )?;

    // videoconvert: adapts PipeWire's output pixel format (commonly RGBA, BGRx, or
    // NV12 depending on the compositor's DRM modifier) to whatever the H.264 encoder
    // prefers. GStreamer negotiates the exact conversion chain at caps-negotiation time.
    let videoconvert = ElementFactory::make("videoconvert")
        .build()
        .context("Failed to create videoconvert")?;

    // Downstream chain (encoder + parser + appsink) via shared helper from encoder.rs.
    let (encoder_elem, profile_capsfilter, parser, parse_capsfilter, appsink) =
        build_h264_pipeline_from_src(encoder_type, encoder_name, width, height, framerate, bitrate)
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
    .context("Failed to link Wayland pipeline elements")?;

    info!(
        node_id,
        "Wayland pipeline: pipewiresrc({node_id}) -> videoconvert -> {encoder_name} \
         -> capsfilter(main) -> h264parse -> appsink"
    );

    Ok(pipeline)
}

// ---------------------------------------------------------------------------
// Latency measurement (opt-in, zero overhead when disabled)
// ---------------------------------------------------------------------------

/// Per-frame PTS delta logger. Enable with BEAM_WAYLAND_LATENCY_MEASURE=1.
///
/// Call `record(pts_ns)` from the appsink callback. Logs first 20 frames
/// and then every 60th frame. Overhead when disabled: one `AtomicBool` load.
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

    /// Record a frame's encoder-output PTS and log the delta from the previous frame.
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
        if count < 20 || count % 60 == 0 {
            info!(
                frame = count,
                pts_delta_us = delta_us,
                "WaylandCapture PTS delta (encoder-input latency proxy)"
            );
        }
    }
}
