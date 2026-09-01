//! `knives`: multi-fork, multi-agent maintenance.
//!
//! Make the state of a fork cheap enough to query that no agent has a reason to
//! guess, and make collisions between agents visible before they cost work.
//!
//! The crate is split so the rules can be tested without a repository:
//!
//! - [`detect`] is pure. Functions from parsed values to findings, no I/O.
//! - [`ids`] gives every identifier its own type, because mixing a change id
//!   with a commit id, or a local bookmark with its remote counterpart, are the
//!   two mistakes this domain actually invites.
//! - [`jj`] is the only module that opens a repository.
//! - [`ledger`] is the only module that knows what a notch is: an append-only
//!   record per repository of what happened and what was decided, which is the
//!   half of state that [`store`] deletes when intent changes.
//! - [`forge`] is the only module that talks to a hosting service.
//! - [`config`] and [`store`] own the two things that cannot be recomputed:
//!   which repos are managed, and who is working on what and why.
//! - [`hook`] owns harness-facing session state and behavior.

pub mod carriage;
pub mod cli;
pub mod commands;
pub mod config;
pub mod detect;
pub mod forge;
pub mod forge_cache;
pub mod hook;
pub mod ids;
pub mod jj;
pub mod ledger;
pub mod pins;
pub mod release_model;
pub mod snapshot;
pub mod seen;
pub mod store;
pub mod timing;
