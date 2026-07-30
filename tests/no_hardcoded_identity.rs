#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

//! Everything user-specific is configuration.
//!
//! The tool must hold no knowledge of any particular user, organisation, or
//! upstream repository. This scans the shipped source for the shapes that would
//! break that.
//!
//! `github.com/` is the load-bearing needle: it catches any hard-coded remote
//! URL whatever the owner, without this test itself having to name a user or an
//! organisation, which would be the same violation one level up. Project-family
//! needles are assembled from fragments for the same reason.
//!
//! Scope is deliberately the shipped source and guidance: `src/`, `plugin/`,
//! `docs/`, and `skill/`. It is NOT the build manifest or the package metadata.
//! `Cargo.toml` pins a library version and `package.json` records where this
//! tool is published; both are toolchain facts rather than subject matter,
//! saying what we link and where we ship, not which repositories this tool
//! manages. That distinction is the whole reason the scope is written down here
//! rather than left implicit.

use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "github.com/",
    "gitlab.com/",
    "bitbucket.org/",
    concat!("inspect", "_"),
    concat!("inspect", "-"),
    concat!("/", "inspect", "/"),
    concat!("`", "inspect", "`"),
    concat!("\"", "inspect", "\""),
    concat!("trajectory", "-labs"),
    concat!("UK", "GovernmentBEIS"),
    concat!("meridian", "labs"),
];

fn scanned_roots() -> Vec<PathBuf> {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    [
        base.join("src"),
        base.join("plugin"),
        base.join("docs"),
        base.join("skill"),
    ]
    .into_iter()
    .filter(|path| path.exists())
    .collect()
}

fn source_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "node_modules") {
                continue;
            }
            source_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn identity_offenders(files: &[PathBuf]) -> Vec<String> {
    let mut offenders = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for needle in FORBIDDEN {
            if text.contains(needle) {
                offenders.push(format!("{}: {needle}", path.display()));
            }
        }
    }
    offenders
}

#[test]
fn a_forge_url_in_a_non_rust_source_file_is_detected() {
    // Given: a UTF-8 source file without a Rust, TypeScript, or JavaScript extension
    let temp = tempfile::TempDir::new().expect("create fixture directory");
    let fixture = temp.path().join("embedded-template");
    std::fs::write(&fixture, "remote=https://github.com/example/repo")
        .expect("write fixture source");
    let mut files = Vec::new();

    // When: the identity guard scans the fixture root
    source_files(temp.path(), &mut files);
    let offenders = identity_offenders(&files);

    // Then: the extensionless source is scanned and its forge URL is rejected
    assert_eq!(
        offenders.len(),
        1,
        "non-Rust fixture escaped the identity guard"
    );

    // Given: a UTF-8 source file containing a project-family identifier
    let temp = tempfile::TempDir::new().expect("create fixture directory");
    let fixture = temp.path().join("embedded-template");
    let project = concat!("inspect", "_ai");
    std::fs::write(&fixture, format!("repository={project}")).expect("write fixture source");
    let mut files = Vec::new();

    // When: the identity guard scans the fixture root
    source_files(temp.path(), &mut files);
    let offenders = identity_offenders(&files);

    // Then: the extensionless source is rejected like a forge URL
    assert_eq!(
        offenders.len(),
        1,
        "non-Rust fixture escaped the identity guard"
    );
}

#[test]
fn the_shipped_source_names_no_user_organisation_or_upstream() {
    // Given: every source file this tool ships
    let mut files = Vec::new();
    for root in scanned_roots() {
        source_files(&root, &mut files);
    }
    assert!(
        !files.is_empty(),
        "found no source files to scan; the scope is wrong"
    );

    // When: each is searched for a hard-coded identity
    let offenders = identity_offenders(&files);

    // Then: there are none. Every such value belongs in the registry.
    assert!(
        offenders.is_empty(),
        "hard-coded identity in shipped source: {offenders:#?}"
    );
}
