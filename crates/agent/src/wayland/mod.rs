//! Wayland backend implementation.
//!
//! Locks in the public shape of the `backend = "wayland"` path so the rest
//! of the agent (`main.rs`, `capture.rs`, `input.rs`, `audio.rs`) can branch
//! on a stable API surface. Submodules implement the four backend traits
//! (DisplayBackend, ScreenCaptureBackend, InputBackend, AudioBackend) for
//! the Wayland path.
//!
//! Status of each piece:
//! - **`compositor`** — spawns a headless `cage` (or `sway --backend=headless`)
//!   compositor with a private `WAYLAND_DISPLAY` + `XDG_RUNTIME_DIR`.
//!   Lifecycle-managed alongside the existing Xorg dummy child.
//! - **`capture`** — opens an xdg-desktop-portal-wlr screencast on the
//!   compositor session bus, grabs the PipeWire node id, builds a GStreamer
//!   pipeline `pipewiresrc path={id} ! videoconvert ! <existing H264 chain>`.
//! - **`input`** — binds `wlr_virtual_pointer_v1` + `wlr_virtual_keyboard_v1`
//!   on the compositor's display. Long-term we may switch to libei.
//! - **`audio`** — PipeWire native (`pipewiresrc ! audioconvert ! opusenc`)
//!   rather than the libpulse path. Sidesteps the `pipewire-pulse` shim
//!   and gives us per-session node IDs.
//!
//! The module is only compiled when the `wayland` Cargo feature is enabled,
//! so 24.04 builds (and any builder that lacks `libwayland-dev`/`libpipewire-0.3-dev`)
//! are unaffected.

#![cfg(feature = "wayland")]

mod audio;
mod capture;
mod compositor;
mod input;

pub use audio::WaylandAudio;
pub use capture::WaylandCapture;
pub use compositor::{WaylandDisplay, WaylandDisplayConfig};
pub use input::WaylandInput;
