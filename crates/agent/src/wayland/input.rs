//! Virtual input injection for the Wayland backend.
//!
//! Binds `zwlr_virtual_pointer_manager_v1` (wayland-protocols-wlr) and
//! `zwp_virtual_keyboard_manager_v1` (wayland-protocols-misc) on the
//! compositor's display; creates one virtual pointer and one virtual keyboard
//! per session; implements the five `InputBackend` methods.
//!
//! ## Protocol notes
//!
//! `zwlr_virtual_pointer_v1` (wlr-virtual-pointer-unstable-v1):
//! - `motion(time: u32, dx: f64, dy: f64)` — wl_fixed, relative pixels
//! - `motion_absolute(time: u32, x: u32, y: u32, x_extent: u32, y_extent: u32)`
//!   — all uint; x/y are pixel coords within the extent box
//! - `button(time: u32, button: u32, state: ButtonState)` — evdev code, enum
//! - `axis(time: u32, axis: Axis, value: f64)` — enum + wl_fixed pixels
//! - `frame()` — commits the event batch
//!
//! `zwp_virtual_keyboard_v1` (virtual-keyboard-unstable-v1):
//! - `keymap(format: u32, fd: BorrowedFd<'_>, size: u32)` — XKB_V1=1
//! - `key(time: u32, key: u32, state: u32)` — evdev code, released=0/pressed=1
//!
//! Keycode translation: browser TS sends X11 keycodes → evdev = X11 − 8.
//! Button mapping: browser 0/1/2 → BTN_LEFT(0x110)/BTN_MIDDLE(0x112)/BTN_RIGHT(0x111).
//! Scroll: 15 px per logical unit (matches GTK defaults).

use std::io::Write as _;
use std::os::fd::AsFd as _;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use tracing::{debug, info};
use wayland_client::{
    protocol::{wl_pointer::{Axis, ButtonState}, wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use crate::input::InputBackend;

const BTN_LEFT: u32 = 0x110;
const BTN_MIDDLE: u32 = 0x112;
const BTN_RIGHT: u32 = 0x111;

const SCROLL_PX_PER_UNIT: f64 = 15.0;

#[allow(dead_code)]
const WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1: u32 = 1;

// ── Wayland registry state ────────────────────────────────────────────────────

struct WlState {
    seat: Option<wl_seat::WlSeat>,
    vpm: Option<ZwlrVirtualPointerManagerV1>,
    vkm: Option<ZwpVirtualKeyboardManagerV1>,
}

impl WlState {
    #[allow(dead_code)]
    fn new() -> Self {
        Self { seat: None, vpm: None, vkm: None }
    }
}

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
                state.seat = Some(registry.bind(name, version.min(7), qh, ()));
            }
            "zwlr_virtual_pointer_manager_v1" => {
                state.vpm = Some(registry.bind(name, version.min(2), qh, ()));
            }
            "zwp_virtual_keyboard_manager_v1" => {
                state.vkm = Some(registry.bind(name, version.min(1), qh, ()));
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

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for WlState {
    fn event(
        _: &mut Self, _: &ZwlrVirtualPointerManagerV1,
        _: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for WlState {
    fn event(
        _: &mut Self, _: &ZwpVirtualKeyboardManagerV1,
        _: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for WlState {
    fn event(
        _: &mut Self, _: &ZwlrVirtualPointerV1,
        _: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for WlState {
    fn event(
        _: &mut Self, _: &ZwpVirtualKeyboardV1,
        _: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
    }
}

// ── Public struct ─────────────────────────────────────────────────────────────

/// Virtual input backend using `zwlr_virtual_pointer_v1` and
/// `zwp_virtual_keyboard_v1`.
pub struct WaylandInput {
    queue: EventQueue<WlState>,
    _state: WlState,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,
    start: Instant,
}

impl WaylandInput {
    /// Connect to `wayland_display` and bind both virtual-input managers.
    ///
    /// `wayland_display` is the socket name produced by the compositor packet,
    /// e.g. `"wayland-beam-99"`. `width`/`height` are shared atomics updated
    /// by the capture path with the compositor output size.
    #[allow(dead_code)]
    pub fn new(
        wayland_display: &str,
        width: Arc<AtomicU32>,
        height: Arc<AtomicU32>,
    ) -> anyhow::Result<Self> {
        // Override WAYLAND_DISPLAY so connect_to_env() picks up the right socket.
        // Safe: single-threaded at construction time; no concurrent env access.
        #[allow(deprecated)]
        unsafe { std::env::set_var("WAYLAND_DISPLAY", wayland_display) };

        let conn = Connection::connect_to_env()
            .with_context(|| format!("connect to Wayland display {wayland_display}"))?;

        let mut queue: EventQueue<WlState> = conn.new_event_queue();
        let qh = queue.handle();
        let mut state = WlState::new();

        conn.display().get_registry(&qh, ());

        // Two roundtrips: first delivers Global advertisements; second confirms
        // the bound proxy objects are ready.
        queue.roundtrip(&mut state).context("Wayland roundtrip 1")?;
        queue.roundtrip(&mut state).context("Wayland roundtrip 2")?;

        // Create both virtual objects while holding short-lived borrows, then
        // drop all borrows before calling roundtrip (which needs &mut state).
        let (pointer, keyboard) = {
            let seat = state
                .seat
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("compositor did not advertise wl_seat"))?;
            let vpm = state.vpm.as_ref().ok_or_else(|| {
                anyhow::anyhow!("compositor did not advertise zwlr_virtual_pointer_manager_v1")
            })?;
            let vkm = state.vkm.as_ref().ok_or_else(|| {
                anyhow::anyhow!("compositor did not advertise zwp_virtual_keyboard_manager_v1")
            })?;
            (
                vpm.create_virtual_pointer(Some(seat), &qh, ()),
                vkm.create_virtual_keyboard(seat, &qh, ()),
            )
        };
        queue.roundtrip(&mut state).context("Wayland roundtrip (objects)")?;

        // Send the XKB keymap once at construction — compositor must receive
        // this before any key events.
        let keymap_str = build_xkb_keymap_string().context("build XKB keymap")?;
        let keymap_bytes = keymap_str.len() as u32;

        let fd = write_keymap_to_memfd(&keymap_str).context("write keymap memfd")?;
        keyboard.keymap(WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, fd.as_fd(), keymap_bytes);
        // fd drops here, closing our copy; Wayland library dups before send.

        queue.roundtrip(&mut state).context("Wayland roundtrip (keymap)")?;

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

    /// Milliseconds elapsed since construction. Wraps at ~49 days — fine for
    /// event timestamps.
    fn now_ms(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// Browser button index → evdev button code.
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
    /// `code` is an X11 keycode (browser TS keymap output). evdev = X11 − 8.
    fn inject_key(&mut self, code: u16, pressed: bool) -> anyhow::Result<()> {
        let evdev = (code as u32).saturating_sub(8);
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
        // motion_absolute takes uint pixel coords and uint extent dimensions.
        let px = (x.clamp(0.0, 1.0) * w as f64) as u32;
        let py = (y.clamp(0.0, 1.0) * h as f64) as u32;
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
        self.pointer.motion(t, dx, dy);
        self.pointer.frame();
        debug!(dx, dy, t, "inject_mouse_move_rel");
        self.flush()
    }

    fn inject_button(&mut self, button: u8, pressed: bool) -> anyhow::Result<()> {
        let btn = Self::map_button(button)?;
        let state = if pressed { ButtonState::Pressed } else { ButtonState::Released };
        let t = self.now_ms();
        self.pointer.button(t, btn, state);
        self.pointer.frame();
        debug!(btn, pressed, t, "inject_button");
        self.flush()
    }

    /// `dx` = horizontal (positive = right), `dy` = vertical (positive = down).
    fn inject_scroll(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
        let t = self.now_ms();
        if dy.abs() > f64::EPSILON {
            self.pointer.axis(t, Axis::VerticalScroll, dy * SCROLL_PX_PER_UNIT);
        }
        if dx.abs() > f64::EPSILON {
            self.pointer.axis(t, Axis::HorizontalScroll, dx * SCROLL_PX_PER_UNIT);
        }
        self.pointer.frame();
        debug!(dx, dy, t, "inject_scroll");
        self.flush()
    }
}

// ── XKB keymap helpers ────────────────────────────────────────────────────────

/// Compile an XKB keymap string for the current system layout.
///
/// Priority:
/// 1. `xkbcomp -xkb $DISPLAY -` — live layout from running X server.
/// 2. `xkbcli compile-keymap --layout $XKB_DEFAULT_LAYOUT` — pure Wayland path.
/// 3. Embedded `us` QWERTY keymap — last resort when neither tool is present.
#[allow(dead_code)]
fn build_xkb_keymap_string() -> anyhow::Result<String> {
    use std::process::Command;

    if let Ok(display) = std::env::var("DISPLAY")
        && let Ok(o) = Command::new("xkbcomp").args(["-xkb", &display, "-"]).output()
        && o.status.success()
        && let Ok(s) = String::from_utf8(o.stdout)
        && !s.trim().is_empty()
    {
        debug!(bytes = s.len(), "keymap: xkbcomp");
        return Ok(s);
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

#[allow(dead_code)]
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

/// Write keymap to an anonymous memfd (Linux) or unlinkable temp file.
///
/// Returns an `OwnedFd`. Caller uses `as_fd()` for the Wayland `keymap()`
/// call; dropping the `OwnedFd` closes it (Wayland library dups before send).
#[allow(dead_code)]
fn write_keymap_to_memfd(keymap: &str) -> anyhow::Result<std::os::unix::io::OwnedFd> {
    // Use memfd_create on Linux for a true anonymous fd. On other targets
    // (macOS CI / cross-compile hosts) fall back to a regular temp file so
    // `cargo check --features wayland` works without Linux headers.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::FromRawFd as _;
        let name = std::ffi::CString::new("beam-keymap").expect("no interior NUL");
        // SAFETY: memfd_create is safe with a valid C string; fd is owned on success.
        let fd = unsafe { libc::syscall(libc::SYS_memfd_create, name.as_ptr(), 1i64 /* MFD_CLOEXEC */) } as i32;
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("memfd_create");
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(keymap.as_bytes()).context("write keymap")?;
        return Ok(std::os::unix::io::OwnedFd::from(file));
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Fallback: unlinked temp file — only reached on non-Linux hosts.
        let mut f = tempfile_unlinked().context("create temp keymap file")?;
        f.write_all(keymap.as_bytes()).context("write keymap")?;
        Ok(std::os::unix::io::OwnedFd::from(f))
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
fn tempfile_unlinked() -> anyhow::Result<std::fs::File> {
    let path = std::env::temp_dir().join(format!("beam-keymap-{}", std::process::id()));
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .context("open temp keymap file")?;
    let _ = std::fs::remove_file(&path);
    Ok(f)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

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
        assert_eq!((65u32).saturating_sub(8), 57); // space: X11=65 → evdev=57
        assert_eq!((8u32).saturating_sub(8), 0);   // saturates at zero
        assert_eq!((0u32).saturating_sub(8), 0);   // never negative
    }

    #[test]
    fn scroll_scale() {
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
