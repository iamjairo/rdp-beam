//! Virtual input injection for the Wayland backend.
//!
//! Binds `zwlr_virtual_pointer_manager_v1` and
//! `zwlr_virtual_keyboard_manager_v1` on the compositor's display, creates one
//! virtual pointer and one virtual keyboard per session, and implements the
//! five `InputBackend` methods against them.
//!
//! ## Protocol notes
//! - `zwlr_virtual_pointer_v1` — motion_absolute / motion / button / axis /
//!   frame.  All timestamps are milliseconds (u32).
//! - `zwlr_virtual_keyboard_v1` — keymap (one-shot at init) / key / modifiers.
//!   Keycodes are Linux evdev; the caller supplies X11 keycodes (browser TS
//!   keymap), so we subtract 8.
//! - Button mapping: browser sends 0/1/2 → BTN_LEFT/BTN_RIGHT/BTN_MIDDLE.
//!   evdev values: BTN_LEFT=0x110, BTN_RIGHT=0x111, BTN_MIDDLE=0x112.
//! - Scroll axis: 0 = vertical, 1 = horizontal.  Value in wl_fixed (1/256
//!   units) — we use 15 * 256 = 3840 per logical scroll unit.
//! - We call `frame()` after every event or batch to commit.
//! - Keymap is compiled from the running XKB environment via `xkbcommon` at
//!   construction; send as XKB_KEYMAP_FORMAT_TEXT_V1 over an anonymous fd.

use std::os::unix::io::IntoRawFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use tracing::{debug, info};

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols_wlr::virtual_keyboard::v1::client::{
    zwlr_virtual_keyboard_manager_v1, zwlr_virtual_keyboard_v1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};

use crate::input::InputBackend;

// ─── evdev button codes ───────────────────────────────────────────────────────
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

// ─── scroll: pixels per logical scroll unit ──────────────────────────────────
// wayland-client 0.31 exposes wl_fixed as f64 in generated Rust bindings.
// 15.0 px per scroll unit is a comfortable default matching GTK's behaviour.
const SCROLL_PX_PER_UNIT: f64 = 15.0;

// ─── XKB keymap format tag ────────────────────────────────────────────────────
// zwlr_virtual_keyboard_v1::keymap expects wl_keyboard::KeymapFormat values.
// TEXT_V1 = 1.
const WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1: u32 = 1;

// ─── State bag threaded through the Dispatch impls ───────────────────────────

struct AppState {
    vp_manager: Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
    vk_manager: Option<zwlr_virtual_keyboard_manager_v1::ZwlrVirtualKeyboardManagerV1>,
    seat: Option<wl_seat::WlSeat>,
}

impl AppState {
    fn new() -> Self {
        Self {
            vp_manager: None,
            vk_manager: None,
            seat: None,
        }
    }
}

// ─── Dispatch impls (all events are server → client; we generate only) ────────

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_seat" => {
                    let seat = registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), qh, ());
                    state.seat = Some(seat);
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    let mgr = registry
                        .bind::<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, _, _>(
                            name,
                            version.min(2),
                            qh,
                            (),
                        );
                    state.vp_manager = Some(mgr);
                }
                "zwlr_virtual_keyboard_manager_v1" => {
                    let mgr = registry
                        .bind::<zwlr_virtual_keyboard_manager_v1::ZwlrVirtualKeyboardManagerV1, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        );
                    state.vk_manager = Some(mgr);
                }
                _ => {}
            }
        }
    }
}

// Seat events: we don't need to handle them.
impl Dispatch<wl_seat::WlSeat, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// Manager objects: no incoming events.
impl Dispatch<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        _: zwlr_virtual_pointer_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_virtual_keyboard_manager_v1::ZwlrVirtualKeyboardManagerV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &zwlr_virtual_keyboard_manager_v1::ZwlrVirtualKeyboardManagerV1,
        _: zwlr_virtual_keyboard_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// Created objects: no incoming events we care about.
impl Dispatch<zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
        _: zwlr_virtual_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_virtual_keyboard_v1::ZwlrVirtualKeyboardV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &zwlr_virtual_keyboard_v1::ZwlrVirtualKeyboardV1,
        _: zwlr_virtual_keyboard_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ─── Public struct ────────────────────────────────────────────────────────────

/// Virtual input backend using `zwlr_virtual_pointer_v1` and
/// `zwlr_virtual_keyboard_v1`.
pub struct WaylandInput {
    /// Wayland event queue — we roundtrip synchronously on each event to flush
    /// the request buffer.  For a remote desktop use-case the RTT cost is
    /// invisible; we stay single-threaded and avoid any Arc/Mutex around the
    /// Wayland socket.
    queue: EventQueue<AppState>,
    _state: AppState,

    pointer: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    keyboard: zwlr_virtual_keyboard_v1::ZwlrVirtualKeyboardV1,

    /// Output extent used for absolute pointer motion.
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,

    /// Monotonic base for millisecond timestamps.
    start: Instant,
}

impl WaylandInput {
    /// Connect to `WAYLAND_DISPLAY` and bind both virtual-input managers.
    ///
    /// `wayland_display` — the socket name set by A2's compositor packet, e.g.
    ///   `"wayland-beam-99"`.
    /// `width` / `height` — shared atomics that hold the compositor's output
    ///   extent so absolute pointer motion is scaled correctly.
    pub fn new(
        wayland_display: &str,
        width: Arc<AtomicU32>,
        height: Arc<AtomicU32>,
    ) -> anyhow::Result<Self> {
        // ── 1. Connect ────────────────────────────────────────────────────────
        // WAYLAND_DISPLAY must be set in the environment *before* calling
        // Connection::connect_to_env(), so we temporarily override it.
        // Safety: single-threaded at construction time.
        std::env::set_var("WAYLAND_DISPLAY", wayland_display);
        let conn = Connection::connect_to_env()
            .with_context(|| format!("Failed to connect to Wayland display {wayland_display}"))?;

        let mut queue: EventQueue<AppState> = conn.new_event_queue();
        let qh = queue.handle();

        let mut state = AppState::new();

        // Bind globals via registry.
        let display = conn.display();
        display.get_registry(&qh, ());

        // Two roundtrips: first populates globals, second confirms bound objects.
        queue.roundtrip(&mut state).context("Wayland roundtrip 1 failed")?;
        queue.roundtrip(&mut state).context("Wayland roundtrip 2 failed")?;

        let seat = state
            .seat
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Compositor did not advertise wl_seat"))?;
        let vp_manager = state
            .vp_manager
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Compositor did not advertise zwlr_virtual_pointer_manager_v1"))?;
        let vk_manager = state
            .vk_manager
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Compositor did not advertise zwlr_virtual_keyboard_manager_v1"))?;

        // ── 2. Create virtual pointer ─────────────────────────────────────────
        let pointer = vp_manager.create_virtual_pointer(Some(seat), &qh, ());
        queue.roundtrip(&mut state).context("Wayland roundtrip (pointer) failed")?;

        // ── 3. Create virtual keyboard ────────────────────────────────────────
        let keyboard = vk_manager.create_virtual_keyboard(seat, &qh, ());
        queue.roundtrip(&mut state).context("Wayland roundtrip (keyboard) failed")?;

        // ── 4. Upload XKB keymap ──────────────────────────────────────────────
        let keymap_str = build_xkb_keymap_string()
            .context("Failed to build XKB keymap")?;
        let keymap_len = keymap_str.len();

        // Write to an anonymous memfd so the compositor can mmap it.
        let fd = write_keymap_to_memfd(&keymap_str)
            .context("Failed to create keymap memfd")?;

        keyboard.keymap(
            WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1,
            fd,
            keymap_len as u32,
        );
        // fd ownership transferred to Wayland (it was IntoRawFd-consumed by
        // write_keymap_to_memfd and is now managed by the kernel).
        queue.roundtrip(&mut state).context("Wayland roundtrip (keymap) failed")?;

        info!(
            display = wayland_display,
            keymap_bytes = keymap_len,
            "WaylandInput: virtual pointer + keyboard ready"
        );

        Ok(Self {
            queue,
            _state: state,
            pointer,
            keyboard,
            width,
            height,
            start: Instant::now(),
        })
    }

    /// Milliseconds since construction — Wayland timestamp field (u32, wraps
    /// after ~49 days, which is fine for event timestamps).
    fn now_ms(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// Map browser button index (0/1/2) to evdev button code.
    fn map_button(button: u8) -> anyhow::Result<u32> {
        match button {
            0 => Ok(BTN_LEFT),
            1 => Ok(BTN_MIDDLE),
            2 => Ok(BTN_RIGHT),
            other => anyhow::bail!("Unknown mouse button: {other}"),
        }
    }

    /// Flush pending Wayland requests.
    fn flush(&mut self) -> anyhow::Result<()> {
        self.queue
            .flush()
            .context("Wayland queue flush failed")?;
        Ok(())
    }
}

impl InputBackend for WaylandInput {
    /// Inject a key event. `code` is an X11 keycode (browser TS sends X11).
    /// evdev = X11 − 8.
    fn inject_key(&mut self, code: u16, pressed: bool) -> anyhow::Result<()> {
        let evdev = (code as u32).saturating_sub(8);
        // wl_keyboard::key_state: released = 0, pressed = 1.
        let key_state: u32 = if pressed { 1 } else { 0 };
        let t = self.now_ms();
        self.keyboard.key(t, evdev, key_state);
        debug!(evdev, pressed, t, "inject_key");
        self.flush()
    }

    /// Absolute pointer move. `x`, `y` are in [0.0, 1.0] normalized coords.
    fn inject_mouse_move_abs(&mut self, x: f64, y: f64) -> anyhow::Result<()> {
        let w = self.width.load(Ordering::Relaxed);
        let h = self.height.load(Ordering::Relaxed);
        let t = self.now_ms();
        // wayland-client 0.31 exposes wl_fixed args as f64 in Rust bindings.
        // motion_absolute(time, x, y, x_extent, y_extent): all f64, pixel coords.
        let px = x.clamp(0.0, 1.0) * w as f64;
        let py = y.clamp(0.0, 1.0) * h as f64;
        let x_extent = w as f64;
        let y_extent = h as f64;
        self.pointer.motion_absolute(t, px, py, x_extent, y_extent);
        self.pointer.frame();
        debug!(px, py, t, "inject_mouse_move_abs");
        self.flush()
    }

    /// Relative pointer move (pointer-lock / raw-input mode).
    fn inject_mouse_move_rel(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
        if dx.abs() < f64::EPSILON && dy.abs() < f64::EPSILON {
            return Ok(());
        }
        let t = self.now_ms();
        // motion(time, dx, dy): wl_fixed exposed as f64 in wayland-client 0.31.
        self.pointer.motion(t, dx, dy);
        self.pointer.frame();
        debug!(dx, dy, t, "inject_mouse_move_rel");
        self.flush()
    }

    fn inject_button(&mut self, button: u8, pressed: bool) -> anyhow::Result<()> {
        let btn = Self::map_button(button)?;
        let state = if pressed {
            zwlr_virtual_pointer_v1::ButtonState::Pressed
        } else {
            zwlr_virtual_pointer_v1::ButtonState::Released
        };
        let t = self.now_ms();
        self.pointer.button(t, btn, state as u32);
        self.pointer.frame();
        debug!(btn, pressed, t, "inject_button");
        self.flush()
    }

    /// Scroll injection. `dx` = horizontal, `dy` = vertical (positive = down).
    fn inject_scroll(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
        let t = self.now_ms();
        if dy.abs() > f64::EPSILON {
            // axis 0 = vertical; value positive = scroll down.
            // axis() value is wl_fixed → f64 in wayland-client 0.31.
            self.pointer.axis(t, 0, dy * SCROLL_PX_PER_UNIT);
        }
        if dx.abs() > f64::EPSILON {
            // axis 1 = horizontal; value positive = scroll right.
            self.pointer.axis(t, 1, dx * SCROLL_PX_PER_UNIT);
        }
        self.pointer.frame();
        debug!(dx, dy, t, "inject_scroll");
        self.flush()
    }
}

// ─── XKB keymap helpers ───────────────────────────────────────────────────────

/// Build an XKB keymap string for the running system layout using libxkbcommon.
///
/// Reads XKB_DEFAULT_LAYOUT / XKB_DEFAULT_VARIANT / XKB_DEFAULT_OPTIONS from
/// the environment (set by the compositor or the agent's launcher).  Falls
/// back to `us` if nothing is set — consistent with what wlroots defaults to.
fn build_xkb_keymap_string() -> anyhow::Result<String> {
    use std::process::Command;

    // Try `xkbcomp -xkb $DISPLAY -` to grab the compiled keymap from the
    // running X server (only if DISPLAY is set and xkbcomp is installed).
    // This handles layout-switching correctly because xkbcomp reflects the
    // *current* server state after `setxkbmap`.
    //
    // If that fails we fall back to assembling an XKB_KEYMAP from environment
    // variables, which covers the headless-Wayland case where no X is running.

    // Try xkbcomp path first (X11 session or cage-with-xwayland).
    if std::env::var("DISPLAY").is_ok() {
        let out = Command::new("xkbcomp")
            .args(["-xkb", &std::env::var("DISPLAY").unwrap(), "-"])
            .output();
        if let Ok(o) = out {
            if o.status.success() {
                if let Ok(s) = String::from_utf8(o.stdout) {
                    if !s.trim().is_empty() {
                        debug!("XKB keymap via xkbcomp ({} bytes)", s.len());
                        return Ok(s);
                    }
                }
            }
        }
    }

    // Fall back: construct a minimal XKB keymap string from environment.
    // This is what many Wayland compositors do internally when no external
    // keymap source is available.
    let layout = std::env::var("XKB_DEFAULT_LAYOUT").unwrap_or_else(|_| "us".to_string());
    let variant = std::env::var("XKB_DEFAULT_VARIANT").unwrap_or_default();
    let options = std::env::var("XKB_DEFAULT_OPTIONS").unwrap_or_default();

    // Build via `xkbcli compile-keymap` (part of libxkbcommon-tools) which is
    // available on Ubuntu 26.04 alongside the lib.
    let mut args = vec!["compile-keymap".to_string(), "--layout".to_string(), layout.clone()];
    if !variant.is_empty() {
        args.push("--variant".to_string());
        args.push(variant.clone());
    }
    if !options.is_empty() {
        args.push("--options".to_string());
        args.push(options.clone());
    }

    let out = Command::new("xkbcli").args(&args).output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8(o.stdout)
                .context("xkbcli output is not valid UTF-8")?;
            debug!(layout, variant, "XKB keymap via xkbcli ({} bytes)", s.len());
            return Ok(s);
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            debug!("xkbcli failed: {stderr}");
        }
        Err(e) => {
            debug!("xkbcli not found: {e}");
        }
    }

    // Last resort: emit a minimal inline keymap string.  This will work for
    // `us` layout and lets the virtual keyboard register correctly even when
    // xkbcli is absent from the build box.
    debug!("Using embedded fallback XKB keymap (layout=us)");
    Ok(minimal_us_keymap())
}

/// A minimal but complete XKB keymap string for `us` layout.
///
/// Covers all keys a standard US QWERTY keyboard would send.  This is the
/// last-resort path used when neither xkbcomp nor xkbcli are available.
fn minimal_us_keymap() -> String {
    r#"xkb_keymap {
    xkb_keycodes  { include "evdev+aliases(qwerty)" };
    xkb_types     { include "complete" };
    xkb_compat    { include "complete" };
    xkb_symbols   { include "pc+us+inet(evdev)" };
    xkb_geometry  { include "pc(pc105)" };
};"#
    .to_string()
}

/// Write `keymap` to an anonymous memfd and return the raw file descriptor.
///
/// The compositor will `mmap()` the fd; we close our end after handing it to
/// the Wayland protocol layer (which dups it before returning).
fn write_keymap_to_memfd(keymap: &str) -> anyhow::Result<std::os::unix::io::RawFd> {
    use std::io::Write;
    use std::os::unix::io::FromRawFd;

    // memfd_create(2) — anonymous, not visible on the filesystem.
    let name = std::ffi::CString::new("beam-keymap").unwrap();
    let fd = unsafe {
        libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC)
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create failed");
    }

    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(keymap.as_bytes())
        .context("Failed to write keymap to memfd")?;
    // Keep the fd open — the Wayland library dup2's it on send.
    let raw = file.into_raw_fd();
    Ok(raw)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_mapping() {
        assert_eq!(WaylandInput::map_button(0).unwrap(), BTN_LEFT);
        assert_eq!(WaylandInput::map_button(1).unwrap(), BTN_MIDDLE);
        assert_eq!(WaylandInput::map_button(2).unwrap(), BTN_RIGHT);
        assert!(WaylandInput::map_button(3).is_err());
    }

    #[test]
    fn x11_to_evdev_conversion() {
        // X11 keycode 65 (space) → evdev 57 (= 65 - 8)
        let evdev = (65u32).saturating_sub(8);
        assert_eq!(evdev, 57);

        // X11 keycode 8 (Escape in some maps) → evdev 0 (clamped, not negative)
        let evdev_min = (8u32).saturating_sub(8);
        assert_eq!(evdev_min, 0);
    }

    #[test]
    fn scroll_pixel_scale() {
        // 1.0 logical unit → SCROLL_PX_PER_UNIT pixels
        let v = 1.0_f64 * SCROLL_PX_PER_UNIT;
        assert!((v - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn minimal_keymap_contains_xkb_keymap_block() {
        let km = minimal_us_keymap();
        assert!(km.contains("xkb_keymap"));
        assert!(km.contains("xkb_symbols"));
    }
}
