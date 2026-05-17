// Library façade for beam-agent — exposed only so integration tests
// in `tests/` can import internal modules.  Not part of the public
// API; semver stability is not guaranteed.

#[cfg(feature = "wayland")]
pub mod wayland;

// The wayland module needs these sibling modules to compile.
#[cfg(feature = "wayland")]
pub mod capture;
#[cfg(feature = "wayland")]
pub mod encoder;
