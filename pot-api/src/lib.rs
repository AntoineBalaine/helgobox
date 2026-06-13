//! The Pot ReaScript API (`HB_Pot_*`) and a standalone, instance-independent pot unit.
//!
//! This crate deliberately does NOT depend on the `helgobox` (`main`) crate: it carries
//! the pot engine surface needed by script-driven UIs (the Lua browser) and nothing
//! else, so it can be linked into a small standalone REAPER extension as well as into
//! the full Helgobox plugin.

mod api;
mod standalone_unit;

pub use api::register_pot_api;
pub use standalone_unit::standalone_pot_unit;
