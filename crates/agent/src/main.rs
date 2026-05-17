mod audio;
mod capture;
mod cli;
mod clipboard;
mod clipboard_sync;
mod cursor;
mod display;
mod encoder;
mod file_transfer_task;
mod filetransfer;
mod gpu;
mod h264;
mod input;
mod signaling;
mod video;
#[cfg(feature = "wayland")]
mod wayland;

use anyhow::Context;
use audio::XorgAudio;
use beam_protocol::InputEvent;
// `ScreenCaptureBackend` brings `capture_frame` into scope. The other
// backend traits' methods are also available on the concrete Xorg* types
// as inherent methods, so no trait import is needed for them in the
// default (non-wayland) build.
use capture::{ScreenCaptureBackend, XorgCapture};
use cli::DEFAULT_FRAMERATE;
use clipboard::ClipboardBridge;
use encoder::Encoder;
use input::XorgInput;
use signaling::SignalingCtx;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

/// Commands sent from async tasks to the capture thread.
/// Using a command channel lets the capture thread exclusively own (and recreate)
/// the Encoder and ScreenCapture during dynamic resolution changes.
pub(crate) enum CaptureCommand {
    Resize {
        width: u32,
        height: u32,
    },
    /// Recreate the encoder pipeline to guarantee a fresh IDR frame.
    ResetEncoder,
}

/// Shared context for building the input event callback.
struct InputCallbackCtx {
    injector: Arc<Mutex<XorgInput>>,
    clipboard: Arc<Mutex<ClipboardBridge>>,
    file_transfer: Arc<Mutex<filetransfer::FileTransferManager>>,
    resize_tx: mpsc::Sender<(u32, u32)>,
    last_input_time: Arc<AtomicU64>,
    clipboard_read_tx: mpsc::Sender<()>,
    download_request_tx: mpsc::Sender<String>,
    capture_wake: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    capture_cmd_tx: std::sync::mpsc::Sender<CaptureCommand>,
    tab_backgrounded: Arc<AtomicBool>,
    video_needs_keyframe: Arc<AtomicBool>,
    display: String,
    max_width: u32,
    max_height: u32,
}

/// Build the reusable input event callback that dispatches input events
/// to the appropriate subsystem (XTEST, clipboard, resize, layout, quality).
fn build_input_callback(ctx: InputCallbackCtx) -> Arc<dyn Fn(InputEvent) + Send + Sync> {
    let InputCallbackCtx {
        injector,
        clipboard,
        file_transfer,
        resize_tx,
        last_input_time,
        clipboard_read_tx,
        download_request_tx,
        capture_wake,
        capture_cmd_tx,
        tab_backgrounded,
        video_needs_keyframe,
        display,
        max_width,
        max_height,
    } = ctx;
    let ctrl_down = Arc::new(AtomicBool::new(false));
    let last_layout = Arc::new(std::sync::Mutex::new(String::new()));

    Arc::new(move |event: InputEvent| {
        // Update last input timestamp for idle detection
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        last_input_time.store(now_ms, Ordering::Relaxed);

        // Wake capture thread if it's sleeping in idle mode
        {
            let (lock, cvar) = &*capture_wake;
            let mut woken = lock.lock().unwrap_or_else(|e| e.into_inner());
            *woken = true;
            cvar.notify_one();
        }

        // Clear backgrounded flag on user-interactive input events
        match &event {
            InputEvent::Key { .. }
            | InputEvent::MouseMove { .. }
            | InputEvent::RelativeMouseMove { .. }
            | InputEvent::Button { .. }
            | InputEvent::Scroll { .. }
                if tab_backgrounded.swap(false, Ordering::Relaxed) =>
            {
                debug!("Input received while backgrounded, clearing flag");
            }
            _ => {}
        }

        match event {
            InputEvent::Key { c, d } => {
                if c == 29 || c == 97 {
                    ctrl_down.store(d, Ordering::Relaxed);
                }
                if !d && (c == 46 || c == 45) && ctrl_down.load(Ordering::Relaxed) {
                    let _ = clipboard_read_tx.try_send(());
                }
                if let Err(e) = injector
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .inject_key(c, d)
                {
                    warn!("Key inject error: {e:#}");
                }
            }
            InputEvent::MouseMove { x, y } => {
                if let Err(e) = injector
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .inject_mouse_move_abs(x, y)
                {
                    warn!("Mouse move inject error: {e:#}");
                }
            }
            InputEvent::RelativeMouseMove { dx, dy } => {
                if dx.is_finite()
                    && dy.is_finite()
                    && (-10000.0..=10000.0).contains(&dx)
                    && (-10000.0..=10000.0).contains(&dy)
                    && let Err(e) = injector
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .inject_mouse_move_rel(dx, dy)
                {
                    warn!("Relative mouse move inject error: {e:#}");
                }
            }
            InputEvent::Button { b, d } => {
                if let Err(e) = injector
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .inject_button(b, d)
                {
                    warn!("Button inject error: {e:#}");
                }
            }
            InputEvent::Scroll { dx, dy } => {
                if dx.is_finite()
                    && dy.is_finite()
                    && (-10000.0..=10000.0).contains(&dx)
                    && (-10000.0..=10000.0).contains(&dy)
                    && let Err(e) = injector
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .inject_scroll(dx, dy)
                {
                    warn!("Scroll inject error: {e:#}");
                }
            }
            InputEvent::Clipboard { ref text } => {
                const MAX_CLIPBOARD_BYTES: usize = 1_048_576;
                if text.len() > MAX_CLIPBOARD_BYTES {
                    warn!(
                        len = text.len(),
                        max = MAX_CLIPBOARD_BYTES,
                        "Clipboard text too large, ignoring"
                    );
                } else if let Err(e) = clipboard
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_text(text)
                {
                    warn!("Clipboard set error: {e:#}");
                }
            }
            InputEvent::ClipboardPrimary { ref text } => {
                const MAX_CLIPBOARD_BYTES: usize = 1_048_576;
                if text.len() > MAX_CLIPBOARD_BYTES {
                    warn!(
                        len = text.len(),
                        max = MAX_CLIPBOARD_BYTES,
                        "Primary clipboard text too large, ignoring"
                    );
                } else if let Err(e) = clipboard
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_primary_text(text)
                {
                    warn!("Primary clipboard set error: {e:#}");
                }
            }
            InputEvent::Resize { w, h } => {
                if let Some((cw, ch)) =
                    display::clamp_resize_dimensions(w, h, max_width, max_height)
                {
                    let _ = resize_tx.try_send((cw, ch));
                } else {
                    warn!(w, h, "Ignoring invalid resize dimensions");
                }
            }
            InputEvent::Layout { ref layout } => {
                if layout.len() <= 20
                    && !layout.is_empty()
                    && layout
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    let mut prev = last_layout.lock().unwrap_or_else(|e| e.into_inner());
                    if *prev == *layout {
                        return;
                    }
                    *prev = layout.clone();
                    drop(prev);

                    let display_str = display.clone();
                    let layout = layout.clone();
                    std::thread::spawn(move || {
                        match std::process::Command::new("setxkbmap")
                            .arg(&layout)
                            .env("DISPLAY", &display_str)
                            .output()
                        {
                            Ok(output) if output.status.success() => {
                                info!(layout = %layout, "Keyboard layout set via setxkbmap");
                            }
                            Ok(output) => {
                                let stderr = String::from_utf8_lossy(&output.stderr);
                                warn!(layout = %layout, "setxkbmap failed: {stderr}");
                            }
                            Err(e) => {
                                warn!(layout = %layout, "Failed to run setxkbmap: {e}");
                            }
                        }
                    });
                } else {
                    warn!(layout = %layout, "Invalid keyboard layout name, ignoring");
                }
            }
            InputEvent::Quality { .. } => {
                // Quality selector removed — bitrate/framerate set by config
            }
            InputEvent::VisibilityState { visible } => {
                info!(visible, "Browser tab visibility changed");
                tab_backgrounded.store(!visible, Ordering::Relaxed);
                if visible {
                    // Reset encoder pipeline to guarantee a fresh IDR frame.
                    // GStreamer's ForceKeyUnit event is unreliable with nvcudah264enc
                    // (it logs "Forced IDR" but the encoded frame isn't actually an
                    // IDR). Pipeline recreation always starts with a real IDR.
                    // video_needs_keyframe gates the video send loop to drop stale
                    // P-frames until the IDR from the new pipeline arrives.
                    info!("Resetting encoder for browser reconnect");
                    video_needs_keyframe.store(true, Ordering::Relaxed);
                    let _ = capture_cmd_tx.send(CaptureCommand::ResetEncoder);
                    // Wake capture thread immediately to restore full framerate
                    let (lock, cvar) = &*capture_wake;
                    let mut woken = lock.lock().unwrap_or_else(|e| e.into_inner());
                    *woken = true;
                    cvar.notify_one();
                }
            }
            InputEvent::FileStart {
                ref id,
                ref name,
                size,
            } => {
                if let Err(e) = file_transfer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .handle_file_start(id, name, size)
                {
                    warn!(id, name, "File transfer start error: {e:#}");
                }
            }
            InputEvent::FileChunk { ref id, ref data } => {
                if let Err(e) = file_transfer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .handle_file_chunk(id, data)
                {
                    warn!(id, "File chunk error: {e:#}");
                }
            }
            InputEvent::FileDone { ref id } => {
                if let Err(e) = file_transfer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .handle_file_done(id)
                {
                    warn!(id, "File done error: {e:#}");
                }
            }
            InputEvent::FileDownloadRequest { ref path } => {
                if let Err(e) = download_request_tx.try_send(path.clone()) {
                    warn!(path, "File download request dropped: {e:#}");
                }
            }
        }
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install rustls crypto provider (needed for TLS WebSocket to server)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    gstreamer::init().context("Failed to initialize GStreamer")?;

    let args = cli::parse_args()?;
    info!(
        display = %args.display,
        session_id = %args.session_id,
        server_url = %args.server_url,
        backend = %args.backend,
        "Starting beam-agent"
    );

    // Validate backend and fail fast for unimplemented paths.
    // The Wayland runtime path is not yet wired up; bail with a clear message
    // rather than silently degrading to Xorg, which would hide a
    // misconfiguration on a 26.04 host.
    match args.backend.as_str() {
        "xorg" => {} // continue below
        "wayland" => {
            #[cfg(feature = "wayland")]
            {
                // Delegate to the Wayland stub — it bails with the same message.
                wayland::WaylandDisplay::start(wayland::WaylandDisplayConfig {
                    display_num: 0,
                    width: 0,
                    height: 0,
                })?;
            }
            #[cfg(not(feature = "wayland"))]
            anyhow::bail!(
                "backend = \"wayland\" is not yet implemented in beam-agent. \
                 Set `[session] backend = \"xorg\"` in /etc/beam/beam.toml as a workaround. \
                 Tracking issue: https://github.com/frecar/beam/issues (wayland backend)"
            );
        }
        other => {
            anyhow::bail!(
                "Unknown backend '{}'. Expected one of: xorg, wayland, auto.",
                other
            );
        }
    }

    // PulseAudio server path — derived from display number regardless of new/existing display
    let mut pulse_server: Option<String> = None;
    let display_num: u32 = args.display.trim_start_matches(':').parse().unwrap_or(10);

    // Try to connect to the display; if it doesn't exist, start a virtual one
    let mut virtual_display = match XorgCapture::new(&args.display) {
        Ok(_) => {
            info!(display = %args.display, "Connected to existing display");
            // Session reuse: PulseAudio should already be running for this display
            let pulse_path = format!("/tmp/beam-pulse-{display_num}/native");
            if std::path::Path::new(&pulse_path).exists() {
                pulse_server = Some(format!("unix:{pulse_path}"));
                info!(%pulse_path, "Found existing PulseAudio socket for reused display");
            } else {
                warn!(%pulse_path, "No PulseAudio socket found for reused display, audio may not work");
            }
            None
        }
        Err(e) => {
            warn!(display = %args.display, "Display not available ({e:#}), starting virtual display");
            match display::XorgVirtualDisplay::start(
                display_num,
                args.width,
                args.height,
                &args.gpu_driver,
                args.display_start,
            ) {
                Ok(mut vd) => {
                    info!(display = %args.display, "Virtual display started");

                    // Start PulseAudio BEFORE desktop so apps inherit PULSE_SERVER
                    if let Err(e) = vd.start_pulseaudio() {
                        warn!("Failed to start PulseAudio: {e:#}");
                    }
                    let pulse_path = format!("/tmp/beam-pulse-{display_num}/native");
                    pulse_server = Some(format!("unix:{pulse_path}"));
                    for _ in 0..20 {
                        if std::path::Path::new(&pulse_path).exists() {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }

                    // Start desktop AFTER PulseAudio
                    if let Err(e) = vd.start_desktop() {
                        warn!("Failed to start desktop: {e:#}");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    Some(vd)
                }
                Err(e) => {
                    return Err(e).context("Failed to start virtual display");
                }
            }
        }
    };

    // Get the xrandr output name for resize operations.
    // Virtual displays know their output name; existing displays detect it.
    let output_name = match &virtual_display {
        Some(vd) => vd.output_name().to_string(),
        None => "DUMMY0".to_string(),
    };

    // Create screen capture (now the display should be available)
    let mut screen_capture =
        XorgCapture::new(&args.display).context("Failed to initialize screen capture")?;
    let width = screen_capture.width();
    let height = screen_capture.height();

    // Detect encoder type first to determine framerate/bitrate caps.
    // Software x264enc ultrafast on ARM64 can only sustain ~60fps at 1080p.
    // Attempting 120fps causes the appsrc queue to grow faster than the
    // encoder drains it, leading to OOM.
    let encoder_pref = args.encoder.clone();
    let (encoder_type, _) = encoder::detect_encoder_type(args.encoder.as_deref())?;
    let config_framerate;
    let config_bitrate;
    if matches!(encoder_type, encoder::EncoderType::Software) && args.framerate > 60 {
        config_framerate = 60;
        config_bitrate = args.bitrate.min(20_000);
        warn!(
            requested_fps = args.framerate,
            capped_fps = config_framerate,
            capped_bitrate = config_bitrate,
            "Software encoder: capping framerate to 60fps and bitrate to 20Mbps"
        );
    } else {
        config_framerate = args.framerate;
        config_bitrate = args.bitrate;
    }

    let encoder = Encoder::with_encoder_preference(
        width,
        height,
        config_framerate,
        config_bitrate,
        args.encoder.as_deref(),
    )
    .context("Failed to initialize encoder")?;

    // Channel for encoded video frames: capture thread -> async write loop
    let (encoded_tx, mut encoded_rx) = mpsc::channel::<Vec<u8>>(2);

    // Channel for encoded audio frames: audio thread -> async write loop.
    // Keep _audio_tx_keepalive so the channel stays open even if the audio
    // thread gives up (prevents the select! loop from exiting prematurely).
    let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(8);
    let _audio_tx_keepalive = audio_tx.clone();

    // Shared WebSocket outbox: video, audio, clipboard, cursor, file download all send here.
    // The signaling loop drains this and writes to the actual WS connection.
    // Capacity 32: enough for signaling + data messages. Video binary frames use try_send
    // with drop-on-full semantics to avoid backpressure from slow WS.
    let (ws_outbox_tx, mut ws_outbox_rx) = mpsc::channel::<Message>(32);

    let session_id = args.session_id;

    // Create input injector (uses XTEST extension -- no uinput needed)
    let input_width = Arc::new(std::sync::atomic::AtomicU32::new(args.width));
    let input_height = Arc::new(std::sync::atomic::AtomicU32::new(args.height));
    let injector = Arc::new(Mutex::new(
        XorgInput::new(
            &args.display,
            Arc::clone(&input_width),
            Arc::clone(&input_height),
        )
        .context("Failed to create input injector")?,
    ));

    // Create clipboard bridge
    let clipboard = Arc::new(Mutex::new(
        ClipboardBridge::new(&args.display).context("Failed to create clipboard bridge")?,
    ));

    // Force-keyframe flag: set from signaling handler (on reconnect),
    // cleared by capture thread each frame.
    let force_keyframe = Arc::new(AtomicBool::new(false));

    // Video-side IDR gate: set from input callback (on reconnect), cleared
    // by the video send loop when the IDR frame arrives. Separate from
    // force_keyframe because the capture thread clears that one immediately
    // (via swap) before the video send loop can see it.
    let video_needs_keyframe = Arc::new(AtomicBool::new(false));

    // Command channel for non-latency-critical capture thread operations
    let (capture_cmd_tx, capture_cmd_rx) = std::sync::mpsc::channel::<CaptureCommand>();

    // Resize request channel
    let (resize_tx, mut resize_rx) = mpsc::channel::<(u32, u32)>(4);

    // Idle detection
    let last_input_time = Arc::new(AtomicU64::new(0));
    let last_input_for_capture = Arc::clone(&last_input_time);

    // Wake signal for capture thread
    let capture_wake = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let capture_wake_for_input = Arc::clone(&capture_wake);

    // Clipboard read requests
    let (clipboard_read_tx, mut clipboard_read_rx) = mpsc::channel::<()>(4);

    // File download requests
    let (download_request_tx, mut download_request_rx) = mpsc::channel::<String>(4);

    // Cursor shape monitor
    let mut cursor_rx = cursor::spawn_cursor_monitor(&args.display);
    if cursor_rx.is_none() {
        warn!("Cursor monitor failed to start, falling back to unclutter");
        if let Some(ref mut vd) = virtual_display {
            vd.hide_cursor();
        }
    }

    // Tab backgrounded flag
    let tab_backgrounded = Arc::new(AtomicBool::new(false));
    let tab_backgrounded_for_capture = Arc::clone(&tab_backgrounded);

    // File transfer manager
    let home_dir = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
    let file_transfer = Arc::new(Mutex::new(filetransfer::FileTransferManager::new(home_dir)));
    let file_transfer_for_download = Arc::clone(&file_transfer);

    // Build input callback
    let input_callback = build_input_callback(InputCallbackCtx {
        injector: Arc::clone(&injector),
        clipboard: Arc::clone(&clipboard),
        file_transfer,
        resize_tx: resize_tx.clone(),
        last_input_time: Arc::clone(&last_input_time),
        clipboard_read_tx: clipboard_read_tx.clone(),
        download_request_tx,
        capture_wake: Arc::clone(&capture_wake_for_input),
        capture_cmd_tx: capture_cmd_tx.clone(),
        tab_backgrounded: Arc::clone(&tab_backgrounded),
        video_needs_keyframe: Arc::clone(&video_needs_keyframe),
        display: args.display.clone(),
        max_width: args.max_width,
        max_height: args.max_height,
    });

    // Shutdown flag for capture/audio threads
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_capture = Arc::clone(&shutdown);
    let shutdown_for_audio = Arc::clone(&shutdown);

    // Capture + encode thread
    const IDLE_TIMEOUT_MS: u64 = 300_000;
    const IDLE_FRAMERATE: u32 = 5;
    const BACKGROUND_FRAMERATE: u32 = 1;
    const ENCODER_RESET_COOLDOWN: Duration = Duration::from_secs(5);

    let display_for_capture = args.display.clone();
    let output_name_for_capture = output_name.clone();
    let kf_flag_for_capture = Arc::clone(&force_keyframe);
    let capture_wake_for_thread = Arc::clone(&capture_wake);
    let input_width_for_capture = Arc::clone(&input_width);
    let input_height_for_capture = Arc::clone(&input_height);

    let capture_handle = std::thread::Builder::new()
        .name("capture-encode".into())
        .spawn(move || {
            // Elevate to real-time priority for consistent frame pacing
            #[cfg(target_os = "linux")]
            {
                let param = libc::sched_param { sched_priority: 50 };
                let ret = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
                if ret != 0 {
                    warn!("Could not set SCHED_FIFO (need CAP_SYS_NICE): {}",
                        std::io::Error::last_os_error());
                } else {
                    info!("Capture thread elevated to SCHED_FIFO priority 50");
                }
            }

            let mut encoder = encoder;
            let current_bitrate = config_bitrate;
            let current_framerate = config_framerate;
            let active_frame_duration_ns = 1_000_000_000u64 / config_framerate as u64;
            let idle_frame_duration_ns = 1_000_000_000u64 / IDLE_FRAMERATE as u64;
            let background_frame_duration_ns = 1_000_000_000u64 / BACKGROUND_FRAMERATE as u64;
            let mut frame_count: u64 = 0;
            let mut encoded_count: u64 = 0;
            let start = Instant::now();
            let mut was_idle = false;
            let mut was_backgrounded = false;
            let mut first_capture_logged = false;
            let mut first_encode_logged = false;
            let mut last_encoder_reset = Instant::now() - ENCODER_RESET_COOLDOWN;
            let mut consecutive_capture_errors: u64 = 0;
            let mut last_capture_heartbeat = Instant::now();

            loop {
                if shutdown_for_capture.load(Ordering::Relaxed) {
                    info!("Capture thread shutting down");
                    break;
                }

                // Process commands from async tasks
                enum EncoderRecreate { None, Reset, Resize }
                let mut recreate = EncoderRecreate::None;
                while let Ok(cmd) = capture_cmd_rx.try_recv() {
                    match cmd {
                        CaptureCommand::Resize { width, height } => {
                            if width == screen_capture.width() && height == screen_capture.height() {
                                debug!(width, height, "Resize skipped (same dimensions)");
                                continue;
                            }
                            info!(width, height, "Processing resize request");

                            if let Err(e) = display::set_display_resolution(
                                &display_for_capture,
                                width,
                                height,
                                &output_name_for_capture,
                            ) {
                                warn!("xrandr resize failed: {e:#}");
                                continue;
                            }

                            for _ in 0..20 {
                                std::thread::sleep(Duration::from_millis(10));
                                if shutdown_for_capture.load(Ordering::Relaxed) {
                                    return;
                                }
                            }

                            let new_capture = match XorgCapture::new(&display_for_capture) {
                                Ok(cap) => cap,
                                Err(e) => {
                                    error!("Failed to recreate capture after resize: {e:#}");
                                    return;
                                }
                            };
                            screen_capture = new_capture;
                            recreate = EncoderRecreate::Resize;
                            break;
                        }
                        CaptureCommand::ResetEncoder => {
                            let elapsed = last_encoder_reset.elapsed();
                            if elapsed < ENCODER_RESET_COOLDOWN {
                                debug!(
                                    cooldown_remaining_ms = (ENCODER_RESET_COOLDOWN - elapsed).as_millis() as u64,
                                    "ResetEncoder throttled, sending force_keyframe instead"
                                );
                                encoder.force_keyframe();
                            } else {
                                recreate = EncoderRecreate::Reset;
                                break;
                            }
                        }
                    }
                }

                match recreate {
                    EncoderRecreate::None => {}
                    EncoderRecreate::Reset => {
                        info!("Dropping old encoder to free NVENC session");
                        drop(encoder);
                        info!("Old encoder dropped, creating new pipeline");
                        encoder = match Encoder::with_encoder_preference(
                            screen_capture.width(),
                            screen_capture.height(),
                            current_framerate,
                            current_bitrate,
                            encoder_pref.as_deref(),
                        ) {
                            Ok(enc) => enc,
                            Err(e) => {
                                error!("Failed to recreate encoder: {e:#}");
                                break;
                            }
                        };
                        first_encode_logged = false;
                        last_encoder_reset = Instant::now();
                        info!("Encoder pipeline recreated (next frame will be IDR)");
                    }
                    EncoderRecreate::Resize => {
                        let new_w = screen_capture.width();
                        let new_h = screen_capture.height();
                        info!(width = new_w, height = new_h, "Dropping old encoder for resize");
                        drop(encoder);
                        info!("Old encoder dropped, creating new pipeline for resize");
                        encoder = match Encoder::with_encoder_preference(
                            new_w, new_h, DEFAULT_FRAMERATE, current_bitrate,
                            encoder_pref.as_deref(),
                        ) {
                            Ok(enc) => enc,
                            Err(e) => {
                                error!("Failed to recreate encoder after resize: {e:#}");
                                break;
                            }
                        };

                        encoder.force_keyframe();
                        first_capture_logged = false;
                        first_encode_logged = false;

                        input_width_for_capture.store(new_w, Ordering::Relaxed);
                        input_height_for_capture.store(new_h, Ordering::Relaxed);

                        info!(
                            width = new_w, height = new_h,
                            "Resize complete, capture and encoder recreated"
                        );
                    }
                }

                // Check force-keyframe flag
                if kf_flag_for_capture.swap(false, Ordering::Relaxed) {
                    encoder.force_keyframe();
                    if tab_backgrounded_for_capture.swap(false, Ordering::Relaxed) {
                        warn!("Keyframe forced while backgrounded — clearing flag");
                    }
                }

                let frame_start = Instant::now();
                let pts = start.elapsed().as_nanos() as u64;

                let is_backgrounded = tab_backgrounded_for_capture.load(Ordering::Relaxed);

                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let last_input_ms = last_input_for_capture.load(Ordering::Relaxed);
                let is_idle = last_input_ms > 0 && (now_ms - last_input_ms) > IDLE_TIMEOUT_MS;

                let frame_duration_ns = if is_backgrounded {
                    background_frame_duration_ns
                } else if is_idle {
                    idle_frame_duration_ns
                } else {
                    active_frame_duration_ns
                };

                if is_backgrounded != was_backgrounded {
                    if is_backgrounded {
                        debug!("Tab backgrounded, reducing to {BACKGROUND_FRAMERATE}fps");
                    } else {
                        debug!(fps = current_framerate, "Tab foregrounded, restoring framerate");
                    }
                    was_backgrounded = is_backgrounded;
                }

                if is_idle != was_idle && !is_backgrounded {
                    if is_idle {
                        debug!("Entering idle mode ({IDLE_FRAMERATE}fps)");
                    } else {
                        debug!(fps = current_framerate, "Resuming active mode");
                    }
                    was_idle = is_idle;
                }

                // Auto-recover from GStreamer pipeline errors
                if encoder.has_error() {
                    warn!("GStreamer pipeline error detected, dropping encoder");
                    drop(encoder);
                    match Encoder::with_encoder_preference(
                        screen_capture.width(), screen_capture.height(),
                        current_framerate, current_bitrate,
                        encoder_pref.as_deref(),
                    ) {
                        Ok(enc) => {
                            encoder = enc;
                            first_encode_logged = false;
                            info!("Encoder auto-recovered from pipeline error");
                        }
                        Err(e) => {
                            error!("Failed to recreate encoder after pipeline error: {e:#}");
                            break;
                        }
                    }
                }

                match screen_capture.capture_frame() {
                    Ok(frame) => {
                        if consecutive_capture_errors > 0 {
                            info!(
                                recovered_after = consecutive_capture_errors,
                                "Capture recovered after consecutive errors"
                            );
                            consecutive_capture_errors = 0;
                        }
                        if !first_capture_logged {
                            info!(size = frame.len(), "First frame captured from X display");
                            first_capture_logged = true;
                        }
                        if let Err(e) = encoder.encode_frame(frame, pts) {
                            error!("Encode error: {e:#}");
                            break;
                        }
                    }
                    Err(e) => {
                        consecutive_capture_errors += 1;
                        if consecutive_capture_errors <= 3 || consecutive_capture_errors.is_multiple_of(100) {
                            warn!(
                                consecutive_errors = consecutive_capture_errors,
                                "Capture frame failed: {e:#}"
                            );
                        }
                        if consecutive_capture_errors >= 300 {
                            error!(
                                consecutive_errors = consecutive_capture_errors,
                                "Capture failing persistently, breaking capture loop"
                            );
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                }

                // Drain encoded frames
                let drain_deadline = Instant::now() + Duration::from_millis(2);
                let mut drained_any = false;
                loop {
                    match encoder.pull_encoded() {
                        Ok(Some(data)) => {
                            drained_any = true;
                            encoded_count += 1;
                            if !first_encode_logged {
                                info!(size = data.len(), "First H.264 frame from encoder");
                                first_encode_logged = true;
                            }
                            match encoded_tx.try_send(data) {
                                Ok(()) => {}
                                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                    debug!("Dropping encoded frame (channel full, prioritizing latency)");
                                }
                                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                    info!("Encoded frame channel closed, stopping capture");
                                    return;
                                }
                            }
                        }
                        Ok(None) => {
                            if drained_any || Instant::now() >= drain_deadline {
                                break;
                            }
                            std::hint::spin_loop();
                        }
                        Err(e) => {
                            error!("Pull encoded error: {e:#}");
                            return;
                        }
                    }
                }

                frame_count += 1;

                if last_capture_heartbeat.elapsed() >= Duration::from_secs(5) {
                    let elapsed = start.elapsed().as_secs_f64();
                    info!(
                        captured = frame_count,
                        encoded = encoded_count,
                        fps = format!("{:.1}", frame_count as f64 / elapsed),
                        is_idle, is_backgrounded,
                        "Capture heartbeat"
                    );
                    last_capture_heartbeat = Instant::now();
                }

                // Frame pacing
                let target = Duration::from_nanos(frame_duration_ns);
                let elapsed = frame_start.elapsed();
                if elapsed < target {
                    let remaining = target - elapsed;
                    if is_idle || is_backgrounded {
                        let (lock, cvar) = &*capture_wake_for_thread;
                        let mut woken = lock.lock().unwrap_or_else(|e| e.into_inner());
                        *woken = false;
                        let result = cvar.wait_timeout(woken, remaining)
                            .unwrap_or_else(|e| e.into_inner());
                        if *result.0 {
                            debug!("Capture thread woken by input/visibility change");
                        }
                    } else {
                        if remaining > Duration::from_millis(2) {
                            std::thread::sleep(remaining - Duration::from_millis(1));
                        }
                        while frame_start.elapsed() < target {
                            std::hint::spin_loop();
                        }
                    }
                }
            }
        })
        .context("Failed to spawn capture thread")?;

    // Audio capture thread — retries PulseAudio connection in background (non-blocking)
    let pulse_server_clone = pulse_server.clone();
    let audio_handle = std::thread::Builder::new()
        .name("audio-capture".into())
        .spawn(move || {
            // Retry PulseAudio connection with backoff: 500ms for first 20 attempts (10s),
            // then 2s indefinitely. PulseAudio can take 10-15s on some hosts.
            let mut audio_capture = None;
            for attempt in 0u32.. {
                if shutdown_for_audio.load(Ordering::Relaxed) {
                    return;
                }
                match XorgAudio::new(48000, 2, pulse_server_clone.as_deref()) {
                    Ok(capture) => {
                        info!(attempt, "Audio capture initialized");
                        audio_capture = Some(capture);
                        break;
                    }
                    Err(e) => {
                        let delay = if attempt < 20 { 500 } else { 2000 };
                        if attempt < 20 || attempt % 10 == 0 {
                            info!(
                                attempt = attempt + 1,
                                delay_ms = delay,
                                "PulseAudio not ready, retrying..."
                            );
                        }
                        if attempt > 60 {
                            warn!("Audio capture unavailable after 60 attempts: {e:#}. Giving up.");
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(delay));
                    }
                }
            }
            let Some(mut audio_capture) = audio_capture else {
                return;
            };
            // Discard any buffered silence from PulseAudio startup to avoid audio/video desync
            audio_capture.flush();
            info!("Audio capture thread started");
            loop {
                if shutdown_for_audio.load(Ordering::Relaxed) {
                    info!("Audio thread shutting down");
                    return;
                }
                match audio_capture.capture_and_encode() {
                    Ok(opus_data) => {
                        if audio_tx.blocking_send(opus_data).is_err() {
                            info!("Audio channel closed, stopping audio capture");
                            return;
                        }
                    }
                    Err(e) => {
                        error!("Audio capture error: {e:#}");
                        return;
                    }
                }
            }
        })
        .context("Failed to spawn audio capture thread")?;
    let audio_handle = Some(audio_handle);

    // Set up SIGTERM handler
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let server_url = args.server_url.clone();
    let kf_flag_for_signal = Arc::clone(&force_keyframe);
    let cmd_tx_for_signal = capture_cmd_tx.clone();
    let cmd_tx_for_video = capture_cmd_tx.clone();
    let cmd_tx_for_resize = capture_cmd_tx;
    let clipboard_for_sync = Arc::clone(&clipboard);

    // WS sender clones for tasks that need to send messages
    let ws_tx_for_cursor = ws_outbox_tx.clone();

    let signaling_ctx = SignalingCtx {
        server_url: &server_url,
        session_id,
        agent_token: args.agent_token.as_deref(),
        tls_cert_path: args.tls_cert_path.as_deref(),
        force_keyframe: kf_flag_for_signal,
        input_callback: Arc::clone(&input_callback),
        capture_cmd_tx: &cmd_tx_for_signal,
        tab_backgrounded: Arc::clone(&tab_backgrounded),
    };

    tokio::select! {
        // Write encoded video frames as WebSocket binary
        _ = video::run_video_send_loop(
            &mut encoded_rx,
            &ws_outbox_tx,
            &force_keyframe,
            &video_needs_keyframe,
            &cmd_tx_for_video,
            &input_width,
            &input_height,
        ) => {}

        // Write encoded audio frames as WebSocket binary
        _ = video::run_audio_send_loop(
            &mut audio_rx,
            &ws_outbox_tx,
        ) => {}

        // Handle signaling WebSocket (also drains ws_outbox_rx)
        _ = signaling::run_signaling(
            &signaling_ctx,
            &mut ws_outbox_rx,
        ) => {}

        // Forward resize requests to capture thread
        _ = async {
            while let Some((w, h)) = resize_rx.recv().await {
                info!(w, h, "Resize requested, forwarding to capture thread");
                let _ = cmd_tx_for_resize.send(CaptureCommand::Resize { width: w, height: h });
            }
        } => {}

        // Clipboard sync: after Ctrl+C/X, read X11 clipboard and send to browser
        _ = clipboard_sync::run_clipboard_sync(
            &mut clipboard_read_rx,
            &clipboard_for_sync,
            &ws_outbox_tx,
        ) => {}

        // File download: stream chunks via WebSocket text
        _ = file_transfer_task::run_file_download_loop(
            &mut download_request_rx,
            &file_transfer_for_download,
            &ws_outbox_tx,
        ) => {}

        // Cursor shape passthrough via WebSocket text
        _ = async {
            if let Some(ref mut rx) = cursor_rx {
                while let Some(css) = rx.recv().await {
                    let msg = serde_json::json!({ "t": "cur", "css": css }).to_string();
                    if let Err(e) = ws_tx_for_cursor.send(Message::Text(msg.into())).await {
                        debug!("Failed to send cursor shape to browser: {e}");
                    }
                }
            } else {
                std::future::pending::<()>().await;
            }
        } => {}

        // Handle shutdown signals
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down");
        }
        _ = sigterm.recv() => {
            info!("Received SIGTERM, shutting down");
        }
    }

    // Signal capture threads to stop before dropping VirtualDisplay
    shutdown.store(true, Ordering::Relaxed);
    drop(encoded_rx);
    drop(audio_rx);
    if let Err(e) = capture_handle.join() {
        warn!("Capture thread panicked: {e:?}");
    }
    if let Some(handle) = audio_handle
        && let Err(e) = handle.join()
    {
        warn!("Audio thread panicked: {e:?}");
    }

    info!("Agent shutdown complete");
    Ok(())
}
