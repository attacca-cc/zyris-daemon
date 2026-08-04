//! Node assembly for zyrisd.
//!
//! `zyris::runtime::Runner` already does dialing, backoff, and reconnect, so what lives here
//! is the four things it doesn't: credentials that never start enrollment, the gate confining
//! paths and resources, SIGTERM and exit codes, and the desktop child's lifetime.

pub mod config;
