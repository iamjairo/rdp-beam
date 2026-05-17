//! Virtual input injection for the Wayland backend.
//!
//! Binds `zwlr_virtual_pointer_manager_v1` and
//! `zwlr_virtual_keyboard_manager_v1` on the compositor's Wayland display,
//! creates one virtual pointer and one virtual keyboard per session, and
//! implements the five `InputBackend` methods against them.
//!
//! ## Protocol summary
//!
//! `zwlr_virtual_pointer_v1` request signatures (from protocol XML):
//! - `motion(time: u32, dx: f64, dy: f64)` — fixed-point, pixels
//! - `motion_absolute(time: u32, x: f64, y: f64, x_extent: u32, y_extent: u32)`
//!   — x/y are fixed-point pixels; extents are integer pixel dimensions
//! - `button(time: u32, button: u32, state: u32)` — evdev button, wl_pointer state
//! - `axis(time: u32, axis: u32, value: f64)` — axis 0=vertical, 1=horizontal
//! - `frame()` — commits the event batch
//!
//! `zwlr_virtual_keyboard_v1`:
//! - `keymap(format: u32, fd: RawFd, size: u32)` — XKB_V1 = 1
//! - `key(time: u32, key: u32, state: u32)` — evdev code, released=0/pressed=1
//! - `modifiers(depressed: u32, latched: u32, locked: u32, group: u32)`
//!
//! Keycode translation: browser TS sends X11 keycodes → evdev = X11 − 8.
//! Button mapping: browser 0/1/2 → BTN_LEFT(0x110)/BTN_MIDDLE(0x112)/BTN_RIGHT(0x111).
//! Scroll: 15 px per logical unit (matches GTK defaults).

use std::ffi::CString;
use std::io::Write as _;
use std::os::unix::io::{FromRawFd as _, IntoRawFd as _, RawFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use tracing::{debug, info};

use wayland_client::{
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, QueueHandle,
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

// ─── scroll magnitude ────────────────────────────────────────────────────────
// wl_fixed axis values are pixels (f64 in wayland-client 0.31 bindings).
// 15 px per logical unit matches GTK3/4 and most Wayland compositor defaults.
const SCROLL_PX_PER_UNIT: f64 = 15.0;

// ─── XKB keymap format ───────────────────────────────────────────────────────
// wl_keyboard::KeymapFormat::XkbV1 = 1
const WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1: u32 = 1;

// ─── Wayland state bag ───────────────────────────────────────────────────────

struct WlState {
    vp_manager: Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
    vk_manager: Option<zwlr_virtual_keyboard_manager_v1::ZwlrVirtualKeyboardManagerV1>,
    seat: Option<wl_seat::WlSeat>,
}

impl WlState {
    fn new() -> Self {
        Self { vp_manager: None, vk_manager: None, seat: None }
    }
}

// ─── Dispatch impls ───────────────────────────────────────────────────────────
// We only generate requests; every incoming event handler is a no-op.

impl Dispatch<wl_registry::WlRegistry, ()> for WlState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else {
            return;
        };
        match interface.as_str() {
            "wl_seat" => {
                let seat =
                    registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), qh, ());
                state.seat = Some(seat);
            }
            "zwlr_virtual_pointer_manager_v1" => {
                let mgr = registry.bind::<
                    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
                    _,
                    _,
                >(name, version.min(2), qh, ());
                state.vp_manager = Some(mgr);
            }
            "zwlr_virtual_keyboard_manager_v1" => {
                let mgr = registry.bind::<
                    zwlr_virtual_keyboard_manager_v1::ZwlrVirtualKeyboardManagerV1,
                    _,
                    _,
                >(name, version.min(1), qh, ());
                state.vk_manager = Some(mgr);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WlState {
    fn event(
        _: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, ()> for WlState {
    fn event(
        _: &mut Self,
        _: &zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        _: zwlr_virtual_pointer_manager_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_virtual_keyboard_manager_v1::ZwlrVirtualKeyboardManagerV1, ()> for WlState {
    fn event(
        _: &mut Self,
        _: &zwlr_virtual_keyboard_manager_v1::ZwlrVirtualKeyboardManagerV1,
        _: zwlr_virtual_keyboard_manager_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1, ()> for WlState {
    fn event(
        _: &mut Self,
        _: &zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
        _: zwlr_virtual_pointer_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_virtual_keyboard_v1::ZwlrVirtualKeyboardV1, ()> for WlState {
    fn event(
        _: &mut Self,
        _: &zwlr_virtual_keyboard_v1::ZwlrVirtualKeyboardV1,
        _: zwlr_virtual_keyboard_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
    }
}

// ─── Public struct ────────────────────────────────────────────────────────────

/// Virtual input backend: one virtual pointer + one virtual keyboard per session.
pub struct WaylandInput {
    /// Wayland event queue.  We flush after each request batch.  Single-
    /// threaded: no locking on the socket.
    queue: EventQueue<WlState>,
    // WlState owns the Wayland proxy objects; must outlive the protocol objects
    // below.  `_` prefix suppresses the dead-field lint while we hold the state
    // alive.
    _state: WlState,

    pointer: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    keyboard: zwlr_virtual_keyboard_v1::ZwlrVirtualKeyboardV1,

    /// Compositor output dimensions for absolute pointer scaling.
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,

    /// Monotonic base for millisecond timestamps in protocol fields.
    start: Instant,
}

impl WaylandInput {
    /// Connect to `wayland_display` and bind both virtual-input managers.
    ///
    /// `wayland_display` is the socket name produced by A2's compositor
    /// packet, e.g. `"wayland-beam-99"`.  `width`/`height` are shared
    /// atomics updated by the capture path with the compositor output size.
    pub fn new(
        wayland_display: &str,
        width: Arc<AtomicU32>,
        height: Arc<AtomicU32>,
    ) -> anyhow::Result<Self> {
        // Override WAYLAND_DISPLAY so connect_to_env() picks up our socket.
        // Safe: agent main thread; no other thread touches the env here.
        #[allow(deprecated)]
        std::env::set_var("WAYLAND_DISPLAY", wayland_display);

        let conn = Connection::connect_to_env()
            .with_context(|| format!("connect to {wayland_display}"))?;

        let mut queue: EventQueue<WlState> = conn.new_event_queue();
        let qh = queue.handle();
        let mut state = WlState::new();

        conn.display().get_registry(&qh, ());

        // Two roundtrips: first delivers Global advertisements, second
        // confirms the bound proxy objects are ready.
        queue.roundtrip(&mut state).context("roundtrip 1")?;
        queue.roundtrip(&mut state).context("roundtrip 2")?;

        let seat = state
            .seat
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("compositor did not advertise wl_seat"))?;
        let vp_manager = state.vp_manager.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "compositor did not advertise zwlr_virtual_pointer_manager_v1; \
                 ensure wlroots compositor is running"
            )
        })?;
        let vk_manager = state.vk_manager.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "compositor did not advertise zwlr_virtual_keyboard_manager_v1; \
                 ensure wlroots compositor is running"
            )
        })?;

        let pointer = vp_manager.create_virtual_pointer(Some(seat), &qh, ());
        queue.roundtrip(&mut state).context("roundtrip (pointer)")?;

        let keyboard = vk_manager.create_virtual_keyboard(seat, &qh, ());
        queue.roundtrip(&mut state).context("roundtrip (keyboard)")?;

        // Send the XKB keymap once at construction.  The compositor must
        // receive this before any key events.
        let keymap_str = build_xkb_keymap_string().context("build XKB keymap")?;
        let keymap_bytes = keymap_str.len() as u32;
        let fd = write_keymap_to_memfd(&keymap_str).context("write keymap memfd")?;
        keyboard.keymap(WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, fd, keymap_bytes);
        // The Wayland library dups the fd before returning; close our copy now.
        // SAFETY: fd is valid; we just created it and have not closed it.
        unsafe { libc::close(fd) };
        queue.roundtrip(&mut state).context("roundtrip (keymap)")?;

        info!(display = wayland_display, keymap_bytes, "WaylandInput ready");

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

    /// Milliseconds elapsed since construction.  Wraps at ~49 days — fine for
    /// event timestamps.
    fn now_ms(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// Browser button index → evdev button code.
    ///
    /// Browser: 0 = left, 1 = middle, 2 = right.
    fn map_button(button: u8) -> anyhow::Result<u32> {
        match button {
            0 => Ok(BTN_LEFT),
            1 => Ok(BTN_MIDDLE),
            2 => Ok(BTN_RIGHT),
            other => anyhow::bail!("unknown mouse button index: {other}"),
        }
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        self.queue.flush().context("Wayland queue flush")
    }
}

impl InputBackend for WaylandInput {
    /// `code` is an X11 keycode (browser TS keymap output).
    /// evdev keycode = X11 − 8.
    fn inject_key(&mut self, code: u16, pressed: bool) -> anyhow::Result<()> {
        let evdev = (code as u32).saturating_sub(8);
        // wl_keyboard::key_state: released = 0, pressed = 1.
        let key_state = u32::from(pressed);
        let t = self.now_ms();
        self.keyboard.key(t, evdev, key_state);
        debug!(evdev, pressed, t, "inject_key");
        self.flush()
    }

    /// `x`, `y` in [0.0, 1.0] normalized screen coordinates.
    fn inject_mouse_move_abs(&mut self, x: f64, y: f64) -> anyhow::Result<()> {
        let w = self.width.load(Ordering::Relaxed);
        let h = self.height.load(Ordering::Relaxed);
        let t = self.now_ms();
        // motion_absolute: x/y are wl_fixed (f64 in wayland-client 0.31);
        // x_extent/y_extent are uint pixel dimensions.
        let px = x.clamp(0.0, 1.0) * w as f64;
        let py = y.clamp(0.0, 1.0) * h as f64;
        self.pointer.motion_absolute(t, px, py, w, h);
        self.pointer.frame();
        debug!(px, py, t, "inject_mouse_move_abs");
        self.flush()
    }

    fn inject_mouse_move_rel(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
        if dx.abs() < f64::EPSILON && dy.abs() < f64::EPSILON {
            return Ok(());
        }
        let t = self.now_ms();
        // motion: dx/dy are wl_fixed → f64 in wayland-client 0.31.
        self.pointer.motion(t, dx, dy);
        self.pointer.frame();
        debug!(dx, dy, t, "inject_mouse_move_rel");
        self.flush()
    }

    fn inject_button(&mut self, button: u8, pressed: bool) -> anyhow::Result<()> {
        let btn = Self::map_button(button)?;
        // wl_pointer::button_state: released = 0, pressed = 1.
        let btn_state = u32::from(pressed);
        let t = self.now_ms();
        self.pointer.button(t, btn, btn_state);
        self.pointer.frame();
        debug!(btn, pressed, t, "inject_button");
        self.flush()
    }

    /// `dx` = horizontal (positive = right), `dy` = vertical (positive = down).
    fn inject_scroll(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
        let t = self.now_ms();
        if dy.abs() > f64::EPSILON {
            // axis 0 = vertical; value is wl_fixed → f64.
            self.pointer.axis(t, 0, dy * SCROLL_PX_PER_UNIT);
        }
        if dx.abs() > f64::EPSILON {
            // axis 1 = horizontal.
            self.pointer.axis(t, 1, dx * SCROLL_PX_PER_UNIT);
        }
        self.pointer.frame();
        debug!(dx, dy, t, "inject_scroll");
        self.flush()
    }
}

// ─── XKB keymap helpers ───────────────────────────────────────────────────────

/// Compile an XKB keymap string for the current system layout.
///
/// Priority:
/// 1. `xkbcomp -xkb $DISPLAY -` — live layout from running X server (handles
///    `setxkbmap` layout switches correctly).
/// 2. `xkbcli compile-keymap --layout $XKB_DEFAULT_LAYOUT` — pure Wayland path
///    using libxkbcommon-tools (available on Ubuntu 26.04).
/// 3. Embedded `us` QWERTY keymap — last resort when neither tool is present.
fn build_xkb_keymap_string() -> anyhow::Result<String> {
    use std::process::Command;

    if let Ok(display) = std::env::var("DISPLAY") {
        let result = Command::new("xkbcomp").args(["-xkb", &display, "-"]).output();
        if let Ok(o) = result {
            if o.status.success() {
                if let Ok(s) = String::from_utf8(o.stdout) {
                    if !s.trim().is_empty() {
                        debug!(bytes = s.len(), "keymap: xkbcomp");
                        return Ok(s);
                    }
                }
            }
        }
    }

    let layout = std::env::var("XKB_DEFAULT_LAYOUT").unwrap_or_else(|_| "us".into());
    let variant = std::env::var("XKB_DEFAULT_VARIANT").unwrap_or_default();
    let options = std::env::var("XKB_DEFAULT_OPTIONS").unwrap_or_default();

    let mut args = vec!["compile-keymap".to_string(), "--layout".to_string(), layout.clone()];
    if !variant.is_empty() {
        args.extend(["--variant".into(), variant.clone()]);
    }
    if !options.is_empty() {
        args.extend(["--options".into(), options]);
    }

    match Command::new("xkbcli").args(&args).output() {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8(o.stdout).context("xkbcli output not UTF-8")?;
            debug!(layout, variant, bytes = s.len(), "keymap: xkbcli");
            return Ok(s);
        }
        Ok(o) => debug!("xkbcli failed: {}", String::from_utf8_lossy(&o.stderr)),
        Err(e) => debug!("xkbcli unavailable: {e}"),
    }

    debug!("keymap: embedded us fallback");
    Ok(minimal_us_keymap())
}

/// Minimal XKB keymap for `us` QWERTY.  Used as last-resort fallback.
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

/// Write a keymap string to an anonymous `memfd`.  Returns the raw fd.
///
/// Caller is responsible for closing the fd after sending the Wayland
/// `keymap` request (the library dups it internally).
fn write_keymap_to_memfd(keymap: &str) -> anyhow::Result<RawFd> {
    let name = CString::new("beam-keymap").expect("no interior NUL");
    // SAFETY: memfd_create is safe to call with a valid C string.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create");
    }
    // SAFETY: fd is valid and we own it exclusively.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(keymap.as_bytes()).context("write keymap")?;
    Ok(file.into_raw_fd())
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
        assert!(WaylandInput::map_button(255).is_err());
    }

    #[test]
    fn x11_to_evdev() {
        // Space: X11=65 → evdev=57
        assert_eq!((65u32).saturating_sub(8), 57);
        // Saturates at zero, never negative.
        assert_eq!((8u32).saturating_sub(8), 0);
        assert_eq!((0u32).saturating_sub(8), 0);
    }

    #[test]
    fn scroll_scale() {
        // 1.0 logical unit → 15.0 px
        assert!((1.0_f64 * SCROLL_PX_PER_UNIT - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn minimal_keymap_structure() {
        let km = minimal_us_keymap();
        assert!(km.contains("xkb_keymap"));
        assert!(km.contains("xkb_symbols"));
        assert!(km.contains("xkb_keycodes"));
    }
}
