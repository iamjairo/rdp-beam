// Library façade for beam-agent — exposed only so integration tests in
// `tests/` can import internal modules.  Not part of the public API;
// semver stability is not guaranteed.

pub mod audio;
pub mod capture;
pub mod clipboard;
pub mod display;
pub mod encoder;
pub mod gpu;
pub mod h264;
pub mod input;

#[cfg(feature = "wayland")]
pub mod wayland;
