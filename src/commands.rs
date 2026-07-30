//! Command implementations.
//!
//! Each command splits into a pure `render` that builds a string and a thin
//! `run` that does the I/O and prints it. The split is not decoration: an
//! earlier implementation shipped a `render` with no caller, so the command
//! printed nothing and exited zero.

pub mod claim;
pub mod init;
pub mod preflight;
pub mod release;
pub mod repos;
pub mod start;
pub mod status;
pub mod sync;
pub mod wip;
