//! Collectors and types for thermon.
//!
//! Every reader takes the filesystem root as a parameter (`/` in production), so
//! the same code runs against captured fixture trees in tests.

pub mod alerts;
pub mod client;
pub mod config;
pub mod control;
pub mod gpu;
pub mod health;
pub mod history;
pub mod hwmon;
pub mod peak;
pub mod processes;
pub mod procfs;
pub mod protocol;
pub mod sampler;
pub mod theme;
mod util;

#[cfg(test)]
mod test_support;
