//! The knives binary's release command against the snapshot forge shim, with an
//! isolated cache root: how a release verb reads pull requests in a test.
//!
//! Composes two sibling fixtures, so a binary that includes it declares
//! `mod forge_shim;` and `mod lab;` under those names first.

#![allow(
    clippy::expect_used,
    reason = "a fixture that cannot proceed IS the test failure"
)]
#![allow(
    unreachable_pub,
    dead_code,
    reason = "a test fixture included by path; not every test target uses every helper"
)]

use super::forge_shim::{install_snapshot_gh, path_with_gh_shim};
use super::lab::{Lab, ReleaseOutput, release_command};

#[derive(Clone, Copy)]
pub struct ReleaseWithSnapshotForgeInput<'a> {
    pub lab: &'a Lab,
    pub home: &'a tempfile::TempDir,
    /// The forge's pull requests, as the shim's JSON list.
    pub pulls: &'a str,
    /// Pull numbers whose by-number facts the forge withholds.
    pub withheld_facts: &'a [u64],
    pub args: &'a [&'a str],
    pub output: ReleaseOutput,
}

pub fn release_with_snapshot_forge(
    input: ReleaseWithSnapshotForgeInput<'_>,
) -> std::process::Output {
    let ReleaseWithSnapshotForgeInput {
        lab,
        home,
        pulls,
        withheld_facts,
        args,
        output,
    } = input;
    let shim = tempfile::tempdir().expect("create forge shim directory");
    install_snapshot_gh(shim.path(), pulls, withheld_facts);
    release_command(lab, home, output, args)
        .env("XDG_CACHE_HOME", shim.path().join("cache"))
        .env("PATH", path_with_gh_shim(shim.path()))
        .output()
        .expect("run knives release with a forge shim")
}

/// The text form with every fact answered: the common case.
pub fn knives_release_with_forge(
    lab: &Lab,
    home: &tempfile::TempDir,
    pulls: &str,
    args: &[&str],
) -> std::process::Output {
    release_with_snapshot_forge(ReleaseWithSnapshotForgeInput {
        lab,
        home,
        pulls,
        withheld_facts: &[],
        args,
        output: ReleaseOutput::Text,
    })
}
