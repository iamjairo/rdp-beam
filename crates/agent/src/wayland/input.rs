//! Virtual input injection for the Wayland backend.
//!
//! Binds `zwlr_virtual_pointer_manager_v1` (from `wayland-protocols-wlr`) and
//! `zwp_virtual_keyboard_manager_v1` (from `wayland-protocols-misc`) on the
//! compositor's display; implements the five `InputBackend` methods.
//!
//! **Status: stub.** Packet A4 produced two scaffolded passes in
//! [`wip/`]:
//! - `input-a4-wlroots.rs` (first pass, ~550 lines)
//! - `input-a4-wlroots-v2.rs` (refined pass, ~478 lines)
//!
//! Both hit two structural blockers:
//!
//! 1. `wayland-protocols-wlr 0.3.12` only exposes the **virtual pointer**
//!    protocol (`virtual_pointer::v1`). The virtual keyboard protocol
//!    (`zwp_virtual_keyboard_v1`) lives in a separate crate
//!    (`wayland-protocols-misc`) which is not currently a workspace dep.
//!    Adding it expands the build matrix.
//! 2. The macro-generated bindings take **strongly-typed enums** for
//!    `button.state` (`wl_pointer::ButtonState`), `axis.axis`
//!    (`zwlr_virtual_pointer_v1::Axis`), `motion_absolute` integer pixel
//!    fields (not f64), and `key.state` (`wl_keyboard::KeyState`). Both
//!    wip passes use raw `u32` literals.
//!
//! Resolving these is mechanical but multi-file (workspace `Cargo.toml`,
//! agent `Cargo.toml` feature list, and the impl). Deferred to a follow-up.

use crate::input::InputBackend;

pub struct WaylandInput {
    _placeholder: (),
}

impl InputBackend for WaylandInput {
    fn inject_key(&mut self, _code: u16, _pressed: bool) -> anyhow::Result<()> {
        anyhow::bail!(super::NOT_YET)
    }

    fn inject_mouse_move_abs(&mut self, _x: f64, _y: f64) -> anyhow::Result<()> {
        anyhow::bail!(super::NOT_YET)
    }

    fn inject_mouse_move_rel(&mut self, _dx: f64, _dy: f64) -> anyhow::Result<()> {
        anyhow::bail!(super::NOT_YET)
    }

    fn inject_button(&mut self, _button: u8, _pressed: bool) -> anyhow::Result<()> {
        anyhow::bail!(super::NOT_YET)
    }

    fn inject_scroll(&mut self, _dx: f64, _dy: f64) -> anyhow::Result<()> {
        anyhow::bail!(super::NOT_YET)
    }
}
