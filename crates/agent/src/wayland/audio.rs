//! PipeWire-native audio capture for the Wayland backend.
//!
//! GStreamer pipeline:
//!
//! ```text
//! pipewiresrc target-object={node} client-name=beam-agent-audio
//!     ! audio/x-raw,rate=48000,channels=2
//!     ! audioconvert
//!     ! audioresample
//!     ! opusenc bitrate=128000 frame-size=20
//!     ! opusparse
//!     ! appsink name=audio_sink
//! ```
//!
//! Returns raw Opus packets in the same `Vec<u8>` shape the libpulse path
//! produces (~20 ms / 50 Hz, 48 kHz stereo, 128 kbps). The main audio send
//! loop in `main.rs` reads `capture_and_encode() -> Result<Vec<u8>>` and
//! writes the bytes to the WS — that loop is unchanged.
//!
//! ## Node selection
//!
//! `target-object` is left empty so pipewiresrc connects to the default
//! capture source. On a Wayland session that means the session's monitor
//! source (the system audio output), which mirrors the libpulse path's
//! `@DEFAULT_MONITOR@` semantics. If a custom source is needed, set
//! `BEAM_WAYLAND_AUDIO_TARGET` to the PipeWire node id (integer) or node
//! name (string).
//!
//! ## Flush semantics
//!
//! `flush()` sends a `FlushStart` + `FlushStop` event upstream, which drops
//! any buffered samples in the pipeline. Matches the existing `libpulse
//! Simple::flush()` behaviour: callers use it to drop startup desync.
//!
//! ## Wiring status
//!
//! Wired via `run_wayland_session` in `main.rs`.

use crate::audio::AudioBackend;
use anyhow::{Context, bail};
use gstreamer::prelude::*;
use gstreamer::{self as gst, ElementFactory};
use gstreamer_app::{AppSink, AppSinkCallbacks};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Runtime handle for a PipeWire audio capture + Opus encode pipeline.
///
/// Owns the GStreamer pipeline (pipewiresrc → audioconvert/resample → opusenc
/// → opusparse → appsink) and the encoded-frame channel. One Opus frame per
/// 20 ms in steady state.
pub struct WaylandAudio {
    pipeline: gst::Pipeline,
    encoded_rx: std::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    _bus_watch: gst::bus::BusWatchGuard,
    pipeline_error: Arc<AtomicBool>,
}

impl WaylandAudio {
    /// Open a PipeWire audio capture session on the compositor's session bus
    /// and build the Opus encode pipeline.
    ///
    /// `xdg_runtime_dir` should be the compositor's private runtime dir
    /// (set by `WaylandDisplay::start`) so we attach to the right PipeWire
    /// instance. `sample_rate` is typically 48000; `channels` is 2; both
    /// match what the existing Xorg libpulse path uses.
    pub fn new(xdg_runtime_dir: &str, sample_rate: u32, channels: u32) -> anyhow::Result<Self> {
        gst::init().context("Failed to init GStreamer")?;

        info!(
            xdg_runtime_dir,
            sample_rate, channels, "WaylandAudio::new — building pipewiresrc pipeline"
        );

        let pipeline = build_audio_pipeline(xdg_runtime_dir, sample_rate, channels)
            .context("Failed to build pipewiresrc → opus pipeline")?;

        // Wire appsink callbacks: pull encoded Opus packets into mpsc.
        let appsink_elem = pipeline
            .by_name("audio_sink")
            .context("appsink element 'audio_sink' not found in pipeline")?;
        let appsink = appsink_elem
            .dynamic_cast::<AppSink>()
            .map_err(|_| anyhow::anyhow!("Failed to cast 'audio_sink' to AppSink"))?;

        let (encoded_tx, encoded_rx) = mpsc::channel::<Vec<u8>>();
        appsink.set_callbacks(
            AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    use gst::FlowError;
                    let sample = sink.pull_sample().map_err(|_| FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| FlowError::Error)?;
                    let _ = encoded_tx.send(map.to_vec());
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        // Bus watch for pipeline error detection.
        let pipeline_error = Arc::new(AtomicBool::new(false));
        let err_flag = Arc::clone(&pipeline_error);
        let bus = pipeline.bus().context("Failed to get pipeline bus")?;
        let _bus_watch = bus
            .add_watch(move |_, msg| {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Error(e) => {
                        error!(
                            source = ?e.src().map(|s| s.name().to_string()),
                            error = %e.error(),
                            debug = ?e.debug(),
                            "WaylandAudio pipeline error"
                        );
                        err_flag.store(true, Ordering::Relaxed);
                    }
                    MessageView::Warning(w) => {
                        warn!(
                            source = ?w.src().map(|s| s.name().to_string()),
                            warning = %w.error(),
                            "WaylandAudio pipeline warning"
                        );
                    }
                    _ => {}
                }
                gst::glib::ControlFlow::Continue
            })
            .context("Failed to install bus watch")?;

        pipeline
            .set_state(gst::State::Playing)
            .context("Failed to start audio pipeline")?;

        info!("WaylandAudio pipeline running");

        Ok(WaylandAudio {
            pipeline,
            encoded_rx: std::sync::Mutex::new(encoded_rx),
            _bus_watch,
            pipeline_error,
        })
    }

    /// Returns true if the pipeline has hit a fatal error. Caller should
    /// drop and recreate the `WaylandAudio` (same recovery pattern as
    /// `WaylandCapture` and `Encoder`).
    pub fn has_error(&self) -> bool {
        self.pipeline_error.load(Ordering::Relaxed)
    }
}

impl AudioBackend for WaylandAudio {
    fn flush(&mut self) {
        // FlushStart + FlushStop drops anything buffered in the pipeline.
        // Mirrors `libpulse_simple::flush()` semantics that the Xorg path
        // uses to drop startup desync.
        let _ = self.pipeline.send_event(gst::event::FlushStart::new());
        let _ = self.pipeline.send_event(gst::event::FlushStop::new(false));
        debug!("WaylandAudio flushed");
    }

    fn capture_and_encode(&mut self) -> anyhow::Result<Vec<u8>> {
        // Pull one Opus packet. The pipeline runs autonomously so we just
        // dequeue. The libpulse path produces one frame per 20 ms; opusenc
        // with `frame-size=20` matches that cadence. Timeout slightly above
        // one Opus frame gives the pipeline room to deliver without us
        // spinning.
        if self.pipeline_error.load(Ordering::Relaxed) {
            bail!("WaylandAudio pipeline is in an error state; recreate it");
        }
        let rx = self
            .encoded_rx
            .lock()
            .map_err(|_| anyhow::anyhow!("audio encoded_rx mutex poisoned"))?;
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(packet) => Ok(packet),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // No packet ready — return empty so the audio send loop
                // doesn't stall. The libpulse path blocks but the encoded
                // bytes are still well-formed for the WS protocol.
                Ok(Vec::new())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("audio pipeline channel disconnected")
            }
        }
    }
}

impl Drop for WaylandAudio {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Construct the pipewiresrc → audioconvert → opusenc → appsink pipeline.
fn build_audio_pipeline(
    _xdg_runtime_dir: &str,
    sample_rate: u32,
    channels: u32,
) -> anyhow::Result<gst::Pipeline> {
    let pipeline = gst::Pipeline::new();

    let pwsrc = ElementFactory::make("pipewiresrc")
        .property("client-name", "beam-agent-audio")
        .build()
        .context("Failed to create pipewiresrc (install gstreamer1.0-pipewire)")?;
    // Optional explicit target node via env var. Empty means default capture.
    if let Ok(target) = std::env::var("BEAM_WAYLAND_AUDIO_TARGET")
        && !target.is_empty()
    {
        pwsrc.set_property_from_str("target-object", &target);
    }

    let caps_src = ElementFactory::make("capsfilter")
        .name("audio_src_caps")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("rate", sample_rate as i32)
                .field("channels", channels as i32)
                .build(),
        )
        .build()
        .context("Failed to create audio_src_caps")?;

    let audioconvert = ElementFactory::make("audioconvert")
        .build()
        .context("Failed to create audioconvert")?;
    let audioresample = ElementFactory::make("audioresample")
        .build()
        .context("Failed to create audioresample")?;

    // opusenc: 128 kbps, 20 ms frame size matches the existing audiopus path.
    let opusenc = ElementFactory::make("opusenc")
        .property("bitrate", 128_000i32)
        .property_from_str("frame-size", "20")
        .build()
        .context("Failed to create opusenc")?;

    let opusparse = ElementFactory::make("opusparse")
        .build()
        .context("Failed to create opusparse")?;

    let appsink = ElementFactory::make("appsink")
        .name("audio_sink")
        .property("sync", false)
        .property("async", false)
        .property("emit-signals", true)
        .property("max-buffers", 8u32)
        .property("drop", false)
        .build()
        .context("Failed to create audio appsink")?;

    pipeline.add_many([
        &pwsrc,
        &caps_src,
        &audioconvert,
        &audioresample,
        &opusenc,
        &opusparse,
        &appsink,
    ])?;
    gst::Element::link_many([
        &pwsrc,
        &caps_src,
        &audioconvert,
        &audioresample,
        &opusenc,
        &opusparse,
        &appsink,
    ])?;

    Ok(pipeline)
}
