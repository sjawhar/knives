//! The view knives writes must carry every field the installed `jj` records.
//!
//! Opening a repository at head merges divergent operation heads and commits
//! the merged view (`RepoLoader::load_at_head`) — a read-shaped write. A
//! jj-lib older than the installed binary silently drops view fields it does
//! not know. jj 0.45.0 added a Git HEAD per colocated workspace
//! (jj-vcs/jj f3115e386); a 0.43 build deleted every such entry on merge, and
//! each workspace's next `jj` command re-imported HEAD from disk and replaced
//! its working-copy commit with a fresh empty one. Observed on sami-agents
//! 2026-09-09, 17:22–19:06Z: 54 wipes, 510 entries destroyed, 335
//! `import git head` operations across ~100 workspaces.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Same process-wide jj config isolation as `tests/common/lab.rs`, for the
/// same reason: jj mints per-repository config directories on first contact.
#[ctor::ctor(unsafe)]
fn isolate_jj_config_home() {
    let xdg = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("xdg");
    // SAFETY: a constructor runs before `main`, hence before libtest spawns its
    // first thread; nothing can read the environment concurrently.
    unsafe { std::env::set_var("XDG_CONFIG_HOME", xdg) }
}

/// Runs the installed `jj` — the binary whose on-disk format knives must agree
/// with — and returns stdout.
fn jj(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("jj")
        .args([
            "--config",
            "fsmonitor.backend=none",
            "--config",
            "git.colocate=true",
            "--config",
            "user.name=view-compat",
            "--config",
            "user.email=view-compat@example.com",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn jj");
    assert!(
        output.status.success(),
        "jj {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("jj output is utf-8")
}

#[test]
fn merging_divergent_op_heads_preserves_per_workspace_git_heads() {
    let temp = tempfile::tempdir().expect("create temp dir");
    jj(temp.path(), &["git", "init", "--colocate", "main"]);
    let main = temp.path().join("main");
    std::fs::write(main.join("a"), "a\n").expect("write file");
    jj(&main, &["commit", "-m", "base"]);
    jj(&main, &["workspace", "add", "../x"]);
    jj(&main, &["workspace", "add", "../y"]);

    // Two operations forked from the same parent: divergent op heads, both
    // carrying the three per-workspace Git HEAD entries.
    let op = jj(
        &main,
        &["op", "log", "--limit", "1", "--no-graph", "-T", "id"],
    )
    .trim()
    .to_owned();
    jj(&main, &["--at-op", &op, "describe", "-m", "side one"]);
    jj(
        &temp.path().join("x"),
        &["--at-op", &op, "describe", "-m", "side two"],
    );

    // The call under test: opening the repo resolves the divergent heads and
    // WRITES the merged view.
    drop(knives::jj::Repo::open(&main).expect("knives opens the repo"));

    // The merged view must still name a Git HEAD for every colocated
    // workspace. `debug object view --op @` pretty-prints the view of the
    // merged operation; scope the check to the `git_heads` block, since the
    // workspace names also appear under `wc_commit_ids`.
    let view = jj(&main, &["debug", "object", "view", "--op", "@"]);
    let git_heads_at = view
        .find("git_heads:")
        .expect("view dump has a git_heads field");
    let git_heads = &view[git_heads_at..];
    let git_heads = &git_heads[..git_heads.find("wc_commit_ids").unwrap_or(git_heads.len())];
    for workspace in ["default", "x", "y"] {
        assert!(
            git_heads.contains(&format!("\"{workspace}\"")),
            "git_heads[{workspace}] missing after knives merged the op heads; \
             its next command would import HEAD from disk and replace the \
             working-copy commit.\ngit_heads block:\n{git_heads}\nfull view:\n{view}"
        );
    }
}
