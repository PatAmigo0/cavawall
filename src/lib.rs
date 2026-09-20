//! The parts of cavawall that are not the renderer
//!
//! Split out so `cavawall-curve` can share the config types and the path maths
//! instead of duplicating them - two copies of `resample` would drift, and the
//! whole point of the authoring tool is that it produces exactly what the
//! renderer consumes
pub mod app_config;
pub mod curve;
pub mod scheme;
