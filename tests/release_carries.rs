//! `knives release members`, `knives carries` and the carriage census.
//!
//! Members counts the repository's parents and names who holds each; `--verify`
//! reports a dropped member's content. Carries compares a revision's net content
//! with each target, stops before superseded releases when a live one carries,
//! and answers against the trunk when no release exists. The census finds
//! orphan branches, respects an open pull, excludes anonymous heads, and says
//! unanswered when the forge withholds a fact or is unavailable.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/forge_shim.rs"]
mod forge_shim;
#[path = "common/lab.rs"]
mod lab;
#[path = "common/release_forge.rs"]
mod release_forge;

use forge_shim::{install_failing_gh, path_with_gh_shim, pull_record};
use knives::jj::Repo;
use lab::{
    Lab, ReleaseOutput, commit_at, extend_branch, home_after_first_cut, knives_release,
    release_command, release_test_home,
};
use release_forge::{ReleaseWithSnapshotForgeInput, release_with_snapshot_forge};
use std::process::Command;

#[test]
fn members_counts_parents_and_names_their_holders() {
    // Given: a flat two-member cut, then feat/alpha advances without moving
    // the release parent it originally held.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let release = commit_at(&lab, "release/2026-08-04");
    let released_alpha = commit_at(&lab, "feat/alpha");
    let released_beta = commit_at(&lab, "feat/beta");
    extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    let advanced_alpha = commit_at(&lab, "feat/alpha");

    // When: the release's members are inspected through the real CLI.
    let output = knives_release(&lab, &home, &["members"]);

    // Then: its own two direct parents are counted, and each is represented
    // once by its current holder or its branch's advanced successor.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "members failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "release/2026-08-04 @ {} — 2 parents",
            release.as_str().chars().take(12).collect::<String>()
        )),
        "the count must come from the release's parent list: {stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "- {} feat/beta",
            released_beta.as_str().chars().take(12).collect::<String>()
        )),
        "the held parent is missing its holder: {stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "- {} feat/alpha advanced to {}",
            released_alpha.as_str().chars().take(12).collect::<String>(),
            advanced_alpha.as_str().chars().take(12).collect::<String>()
        )),
        "the advanced member must name the current tip: {stdout}"
    );
    assert_eq!(
        stdout.lines().filter(|line| line.starts_with("- ")).count(),
        2,
        "each release parent must render exactly one row: {stdout}"
    );
}

#[test]
fn members_verify_reports_a_dropped_members_content() {
    // Given: the same hand-resolved conflicting cut as the recut scenario,
    // where the release tree contains neither member's original content.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("cut release/2026-08-04 as"),
        "first cut was refused: {first:?}"
    );
    let dropped_beta = commit_at(&lab, "feat/beta");
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::write(lab.work.join("shared.txt"), "merged\n").expect("resolve by hand");
    lab.jj_work(["new"]);

    // When: each member's content is replayed against the resolved release.
    let output = knives_release(&lab, &home, &["members", "--verify"]);

    // Then: the lost member is a finding, rather than a successful inspection.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "members verify must fail closed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "!! feat/beta@{}: the cut tree is missing or diverges from the member's content",
            dropped_beta.as_str().chars().take(12).collect::<String>()
        )),
        "the dropped member must be named under missing: {stdout}"
    );
}

#[test]
fn prose_parent_lines_do_not_inflate_the_count() {
    // Given: a flat two-member release whose description contains a line that
    // looks like an old text parser's parent record.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.jj_work([
        "describe",
        "-r",
        "release/2026-08-04",
        "-m",
        "release notes\nparent deadbeef from feat/x",
    ]);

    // When: the release is inspected by name.
    let output = knives_release(&lab, &home, &["members", "release/2026-08-04"]);

    // Then: prose never changes the structural parent count.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "members failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("release/2026-08-04 @") && stdout.contains("— 2 parents"),
        "the prose line inflated the count: {stdout}"
    );
    assert_eq!(
        stdout.lines().filter(|line| line.starts_with("- ")).count(),
        2,
        "the report must have exactly one row per actual parent: {stdout}"
    );
}

/// Publish an origin bookmark from the second clone, then fetch it into `work`.
fn publish_remote_bookmark(lab: &Lab, source: &str, destination: &str) {
    let run_in_second = |args: &[&str]| {
        let command = Command::new("jj")
            .args(args)
            .current_dir(&lab.second)
            .output()
            .expect("run jj in second clone");
        assert!(
            command.status.success(),
            "jj {args:?} failed: {}",
            String::from_utf8_lossy(&command.stderr)
        );
    };
    run_in_second(&["git", "fetch", "--remote", "origin"]);
    run_in_second(&["bookmark", "create", destination, "-r", source]);
    run_in_second(&[
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        destination,
    ]);
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
}

#[test]
fn release_carries_answers_carried_for_a_member() {
    // Given: a release cut carrying alpha.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(cut.status.success(), "{cut:?}");

    // When: carries checks every release target and the upstream trunk.
    let output = knives_release(&lab, &home, &["carries", "feat/alpha"]);

    // Then: the live release says it carries alpha exactly, so the answer is safe.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("carried-exact      release/2026-08-04"),
        "{stdout}"
    );
}

#[test]
fn release_carries_stops_before_superseded_targets_when_live_release_carries() {
    // Given: alpha is carried in both the previous cut and its live successor,
    // with the previous cut restored as a historical remote target afterward.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let historical = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "create",
        "history/alpha-release",
        "-r",
        historical.as_str(),
    ]);
    lab.push_branch("history/alpha-release");
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(second.status.success(), "{second:?}");
    publish_remote_bookmark(&lab, "history/alpha-release@origin", "release/2026-08-04");

    // When: carries finds alpha in the live release or trunk census.
    let output = knives_release(&lab, &home, &["carries", "feat/alpha"]);

    // Then: that safe answer does not probe or print stale release targets.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("carried-exact      release/2026-08-05"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("release/2026-08-04"),
        "safe results must not include superseded probes: {stdout}"
    );
}

#[test]
fn release_carries_answers_not_carried_for_outside_work() {
    // Given: a release cut carrying alpha and an independent beta branch.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(cut.status.success(), "{cut:?}");
    lab.branch("feat/beta", "beta.txt", "beta\n");

    // When: beta is checked against every release target and the trunk.
    let output = knives_release(&lab, &home, &["carries", "feat/beta"]);

    // Then: no safe target carries it, so the answer names the real remaining diff.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("NOT carried"), "{stdout}");
}

#[test]
fn release_carries_in_checks_only_the_requested_target() {
    // Given: alpha is in the release but absent from the upstream trunk.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(cut.status.success(), "{cut:?}");

    // When: the explicit target is the upstream trunk.
    let output = knives_release(
        &lab,
        &home,
        &["carries", "feat/alpha", "--in", "main@upstream"],
    );

    // Then: the live release cannot make a single-target trunk query safe.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("NOT carried        main@upstream"),
        "{stdout}"
    );
    assert!(!stdout.contains("release/2026-08-04"), "{stdout}");
}

#[test]
fn release_carries_in_exits_successfully_when_the_selected_historical_release_carries() {
    // Given: alpha was carried by a release that later became superseded.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let historical = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "create",
        "history/alpha-release",
        "-r",
        historical.as_str(),
    ]);
    lab.push_branch("history/alpha-release");
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(second.status.success(), "{second:?}");
    publish_remote_bookmark(&lab, "history/alpha-release@origin", "release/2026-08-04");

    // When: the explicit target is the known, historical release.
    let output = knives_release(
        &lab,
        &home,
        &["carries", "feat/alpha", "--in", "release/2026-08-04@origin"],
    );

    // Then: --in reports the direct target verdict, not its safe-delete role.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("carried-exact      release/2026-08-04@origin"),
        "{stdout}"
    );
}

#[test]
fn release_carries_answers_against_the_trunk_when_no_release_exists() {
    // Given: a branch but no release in hand.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: carries has no explicit target.
    let output = knives_release(&lab, &home, &["carries", "feat/alpha"]);

    // Then: the orphan question is answered against the upstream trunk.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("main@upstream"), "{stdout}");
    assert!(stdout.contains("NOT carried"), "{stdout}");
}

#[test]
fn release_carries_reports_carried_rewritten_for_a_squash_landed_branch() {
    // Given: alpha is squash-landed, so the trunk has its content but not its tip.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    let alpha = commit_at(&lab, "feat/alpha");
    let trunk = commit_at(&lab, "main@upstream");
    assert!(
        !Repo::open(&lab.work)
            .expect("open after squash merge")
            .is_ancestor(&alpha, &trunk)
            .expect("ancestry answerable"),
        "fixture must use a rewritten trunk commit"
    );
    let (home, _consumer) = release_test_home(&lab);

    // When: alpha is checked without any release.
    let output = knives_release(&lab, &home, &["carries", "feat/alpha"]);

    // Then: the trunk's tree-content evidence proves its rewritten carriage.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("carried-rewritten  main@upstream"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&trunk.as_str().chars().take(12).collect::<String>()),
        "{stdout}"
    );
}

#[test]
fn carries_superseded_only_carriage_is_findings() {
    // Given: a published cut carrying alpha survives at origin after the local
    // release drops it and the next cut becomes live.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let original = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "create",
        "history/alpha-release",
        "-r",
        original.as_str(),
    ]);
    lab.push_branch("history/alpha-release");
    let dropped = knives_release(
        &lab,
        &home,
        &[
            "drop",
            "feat/alpha",
            "--why",
            "superseded release preserves it",
        ],
    );
    assert!(dropped.status.success(), "{dropped:?}");
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05", "--allow-drop"]);
    assert!(second.status.success(), "{second:?}");

    // Publish the preserved historical commit under its release name only after
    // the successor cut has passed the duplicate-release gate.
    let run_in_second = |args: &[&str]| {
        let command = Command::new("jj")
            .args(args)
            .current_dir(&lab.second)
            .output()
            .expect("run jj in second clone");
        assert!(
            command.status.success(),
            "jj {args:?} failed: {}",
            String::from_utf8_lossy(&command.stderr)
        );
    };
    run_in_second(&["git", "fetch", "--remote", "origin"]);
    run_in_second(&[
        "bookmark",
        "create",
        "release/2026-08-04",
        "-r",
        "history/alpha-release@origin",
    ]);
    run_in_second(&[
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        "release/2026-08-04",
    ]);
    lab.jj_work(["git", "fetch", "--remote", "origin"]);

    // When: bare carries finds alpha only in the historical remote cut.
    let output = knives_release(&lab, &home, &["carries", "feat/alpha"]);

    // Then: the historical row is visible, but it cannot make the result safe.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("carried-exact      release/2026-08-04@origin"),
        "{stdout}"
    );
    assert!(
        stdout.contains("NOT carried        release/2026-08-05"),
        "{stdout}"
    );
}

/// A release history with one live and one superseded cut, plus one branch
/// deliberately absent from both. The historical cut remains only as an origin
/// ref, so it is a census target without becoming a maintained branch itself.
fn census_lab() -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let historical = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "create",
        "history/alpha-release",
        "-r",
        historical.as_str(),
    ]);
    lab.push_branch("history/alpha-release");
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(second.status.success(), "{second:?}");
    publish_remote_bookmark(&lab, "history/alpha-release@origin", "release/2026-08-04");
    lab.jj_work(["bookmark", "forget", "history/alpha-release"]);
    lab.branch("feat/beta", "beta.txt", "beta\n");
    (lab, home)
}

fn census_block<'a>(stdout: &'a str, branch: &str) -> &'a str {
    let wanted = format!("  {branch} @");
    stdout
        .split("\n\n")
        .find(|block| block.starts_with(&wanted))
        .unwrap_or_else(|| panic!("no census block for {branch}: {stdout}"))
}

#[test]
fn census_finds_the_orphan_branch() {
    // Given: alpha is in the live cut and beta is independent; the preceding
    // dated cut survives only as a superseded origin target.
    let (lab, home) = census_lab();

    // When: census asks only local carriage questions, so PR state is explicitly unknown.
    let output = knives_release(&lab, &home, &["carries", "--all", "--no-github"]);

    // Then: a carried member names its live carrier but does not spend a
    // superseded probe, while a non-carried member proves the negative against
    // every target, including the historical release.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        !stdout
            .split("\n\n")
            .any(|block| block.starts_with("  main @")),
        "the upstream trunk is a target, never a maintained-branch row: {stdout}"
    );
    let alpha = census_block(&stdout, "feat/alpha");
    assert!(
        alpha.contains("carried-exact      release/2026-08-05"),
        "{alpha}"
    );
    assert!(
        !alpha.contains("release/2026-08-04"),
        "a live-carried row must not probe superseded targets: {alpha}"
    );
    let beta = census_block(&stdout, "feat/beta");
    for target in [
        "release/2026-08-05",
        "main@upstream",
        "release/2026-08-04@origin",
    ] {
        assert!(
            beta.contains(&format!("NOT carried        {target}")),
            "beta must be checked against {target}: {beta}"
        );
    }
    assert!(
        stdout.contains("orphans: not carried anywhere (pull request state unknown)\n  feat/beta"),
        "{stdout}"
    );
}

#[test]
fn census_marks_unknown_pull_orphans_as_unanswered_in_json() {
    // Given: beta is locally uncarried and pull-request lookup is deliberately skipped.
    let (lab, home) = census_lab();

    // When: the census is emitted as its machine report.
    let output = knives_release_json(&lab, &home, &["carries", "--all", "--no-github"]);

    // Then: the qualified text listing remains actionable, but JSON cannot
    // represent beta as a pull-safe, proven orphan.
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse census JSON");
    let beta = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["branch"] == "feat/beta")
        .expect("beta row");
    assert!(beta["orphan"].is_null(), "{report}");
    assert!(
        report["orphans"]
            .as_array()
            .expect("qualified orphan listing")
            .iter()
            .any(|orphan| orphan == "feat/beta"),
        "{report}"
    );
    assert_eq!(output.status.code(), Some(3), "{report}");
}

#[test]
fn census_respects_an_open_pull() {
    // Given: beta's content remains outside every release but the forge says its
    // branch has an open pull request.
    let (lab, home) = census_lab();
    let pulls = format!("[{}]", pull_record(17, "OPEN", "feat/beta", None));

    // When: the real CLI completes one forge snapshot for the census.
    let output = release_with_snapshot_forge(ReleaseWithSnapshotForgeInput {
        lab: &lab,
        home: &home,
        pulls: &pulls,
        withheld_facts: &[],
        args: &["carries", "--all"],
        output: ReleaseOutput::Json,
    });

    // Then: the branch association is retained in the report and forbids an
    // orphan result despite every target being non-carried.
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse census JSON");
    let beta = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["branch"] == "feat/beta")
        .expect("beta row");
    assert_eq!(beta["in_open_pull"], true, "{report}");
    assert_eq!(beta["orphan"], false, "{report}");
    let alpha = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["branch"] == "feat/alpha")
        .expect("alpha row");
    assert_eq!(
        alpha["in_open_pull"], false,
        "a completed snapshot answers that an absent branch has no open pull: {report}"
    );
    assert_eq!(output.status.code(), Some(0), "{report}");
}

#[test]
fn census_withholds_a_selected_pull_fact_as_unanswered() {
    // Given: beta is locally uncarried and discovery names its open pull request.
    let (lab, home) = census_lab();
    let pulls = format!("[{}]", pull_record(17, "OPEN", "feat/beta", None));

    // When: the live batch withholds that selected pull request's fact.
    let output = release_with_snapshot_forge(ReleaseWithSnapshotForgeInput {
        lab: &lab,
        home: &home,
        pulls: &pulls,
        withheld_facts: &[17],
        args: &["carries", "--all"],
        output: ReleaseOutput::Json,
    });

    // Then: discovery cannot make the pull state a deletion-safe answer.
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse census JSON");
    let beta = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["branch"] == "feat/beta")
        .expect("beta row");
    assert!(beta["in_open_pull"].is_null(), "{report}");
    assert!(beta["orphan"].is_null(), "{report}");
    assert_eq!(output.status.code(), Some(3), "{report}");
}

#[test]
fn census_keeps_local_orphans_when_the_forge_is_unavailable() {
    // Given: beta is locally uncarried and the forge refuses every request.
    let (lab, home) = census_lab();

    // When: census attempts the normal forge snapshot.
    let output = knives_release_with_failing_forge(&lab, &home, &["carries", "--all"]);

    // Then: failure changes pull-request knowledge to unknown without hiding
    // the local orphan finding, and the unanswered deletion-safety check wins.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("pull request state unavailable:"),
        "{stdout}"
    );
    assert!(
        stdout.contains("orphans: not carried anywhere (pull request state unknown)\n  feat/beta"),
        "{stdout}"
    );
}

#[test]
fn census_excludes_anonymous_heads() {
    // Given: an unbookmarked commit with unique content, disconnected from the
    // working copy before the census runs.
    let (lab, home) = census_lab();
    lab.jj_work(["new", "main@upstream", "-m", "stranded"]);
    std::fs::write(lab.work.join("stranded.txt"), "stranded\n").expect("write stranded content");
    lab.jj_work(["new", "main@upstream"]);

    // When: the maintained-branch census runs without a pull-request lookup.
    let output = knives_release_json(&lab, &home, &["carries", "--all", "--no-github"]);

    // Then: the unnamed head is audit population, not a census row or schema field.
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse census JSON");
    let branches = report["rows"]
        .as_array()
        .expect("branch rows")
        .iter()
        .map(|row| row["branch"].as_str().expect("branch name"))
        .collect::<Vec<_>>();
    assert_eq!(branches, ["feat/alpha", "feat/beta"], "{report}");
    assert!(
        report.get("anonymous").is_none(),
        "anonymous heads belong exclusively to audit: {report}"
    );
}

/// Run census without a forge so its locally discovered anonymous id can be
/// supplied as a pull request's exact head oid on a subsequent run.
fn knives_release_json(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    release_command(lab, home, ReleaseOutput::Json, args)
        .output()
        .expect("run knives release census")
}

/// Run census with a forge that fails before returning any data.
fn knives_release_with_failing_forge(
    lab: &Lab,
    home: &tempfile::TempDir,
    args: &[&str],
) -> std::process::Output {
    let shim = tempfile::tempdir().expect("create forge shim directory");
    install_failing_gh(shim.path(), &shim.path().join("calls.log"));
    release_command(lab, home, ReleaseOutput::Text, args)
        .env("XDG_CACHE_HOME", shim.path().join("cache"))
        .env("PATH", path_with_gh_shim(shim.path()))
        .output()
        .expect("run knives release census with a failing forge")
}
