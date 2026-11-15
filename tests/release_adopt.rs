//! `knives release adopt`: a release finished by hand recorded as its newest
//! knives write.
//!
//! A hand resolution of a conflict, a renumbered release log or a repair on the
//! release commit is adopted as one edit event, after which audit reads the
//! release as knives' own: no drift, no anonymous head. The event's parent set
//! is the replaced write's, plus each earlier write of the name a
//! fast-forward push added, labelled a carrier. Refused: a plan with
//! problems, another series' name, an upstream trunk that does not resolve, a
//! divergent or absent release, a name with no recorded write, a parent set
//! that is not the write's plus carriers (a child of the release included),
//! conflicts, a working copy on the commit, a published write the publish
//! remote has not replaced (a `record: published` note names one this
//! repository never held), and a change beyond the conflicts with no reason.
//! The write itself, or a re-signed copy, is
//! nothing to adopt. A resolution that drops every line a member added is
//! recorded and exits with findings.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::ledger::{Entry, Kind, RecordedParent};
use lab::{
    Lab, ReleaseOutput, commit_at, file_at_revision, home_after_first_cut, knives_release,
    release_command, release_parents, release_test_home,
};

const RELEASE: &str = "release/2026-08-04";

const BASE_LOG: &str = include_str!("fixtures/changelog_union/ygo/base/CHANGELOG.md");
const BASE_NOTES: &str = include_str!("fixtures/changelog_union/ygo/base/RELEASE_NOTES.md");
const SAMI11_LOG: &str = include_str!("fixtures/changelog_union/ygo/sami11/CHANGELOG.md");
const SAMI11_NOTES: &str = include_str!("fixtures/changelog_union/ygo/sami11/RELEASE_NOTES.md");
const SPLIT_LOG: &str = include_str!("fixtures/changelog_union/ygo/split-from-store/CHANGELOG.md");
const SPLIT_NOTES: &str =
    include_str!("fixtures/changelog_union/ygo/split-from-store/RELEASE_NOTES.md");
const CLIENT_LOG: &str = include_str!("fixtures/changelog_union/ygo/client-count/CHANGELOG.md");
const CLIENT_NOTES: &str =
    include_str!("fixtures/changelog_union/ygo/client-count/RELEASE_NOTES.md");
const SPLIT_ONE_PATH_LOG: &str =
    include_str!("fixtures/changelog_union/ygo/split-one-path/CHANGELOG.md");
const SPLIT_ONE_PATH_NOTES: &str =
    include_str!("fixtures/changelog_union/ygo/split-one-path/RELEASE_NOTES.md");

const SPLIT: &str = "rel/split-from-store";
const SAMI11: &str = "sami11";
const CLIENT: &str = "rel/client-count";

/// `knives release adopt <args>` from the work checkout, whatever its exit.
fn adopt(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let mut command = vec!["adopt"];
    command.extend_from_slice(args);
    knives_release(lab, home, &command)
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Both streams, for an assertion's message.
fn text(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Run a release verb that must succeed; its stdout.
fn release_ok(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> String {
    let output = knives_release(lab, home, args);
    assert!(
        output.status.success(),
        "{args:?} failed: {}",
        text(&output)
    );
    stdout(&output)
}

/// The fork's ledger entries.
fn ledger_entries(home: &tempfile::TempDir) -> Vec<Entry> {
    knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries
}

/// The newest event the ledger records about `name`.
fn newest_event(home: &tempfile::TempDir, name: &str) -> Entry {
    let entries = ledger_entries(home);
    entries
        .iter()
        .rev()
        .find(|entry| entry.kind == Kind::Event && entry.subject.as_deref() == Some(name))
        .cloned()
        .unwrap_or_else(|| panic!("no event about {name}: {entries:?}"))
}

/// `knives --json audit demo --no-github`, parsed. The release homes name an
/// unreachable origin so consumer pins match it; the audit reads live remote
/// refs, so it runs against the lab's own origin, with the same ledger. The
/// registry is put back afterwards, so the release verbs that follow plan as
/// they did before.
fn audit_report(lab: &Lab, home: &tempfile::TempDir) -> serde_json::Value {
    let registry = home.path().join("repos.toml");
    let configured = std::fs::read_to_string(&registry).expect("read registry");
    std::fs::write(
        &registry,
        configured.replace(
            "origin = \"https://forge.invalid/acme/work.git\"",
            &format!("origin = \"{}\"", lab.temp_origin().display()),
        ),
    )
    .expect("point the registry at the lab's origin");
    let output = lab::knives(lab, home, &["--json", "audit", "demo", "--no-github"]);
    std::fs::write(&registry, configured).expect("restore the registry");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("audit emits JSON ({error}): {}", text(&output)));
    assert_eq!(
        report["problems"],
        serde_json::json!(["open pull-head reconciliation was skipped (--no-github)"]),
        "the audit must have read everything but pull heads: {report}"
    );
    report
}

/// The audit findings of `kind`.
fn findings<'a>(report: &'a serde_json::Value, kind: &str) -> Vec<&'a serde_json::Value> {
    report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .filter(|finding| finding["kind"] == kind)
        .collect()
}

/// Edit the release in place, as a person resolving it does: `jj edit`, the
/// files written, `jj new`. The commit keeps the release's change id.
fn finish_in_place(lab: &Lab, files: &[(&str, &str)]) {
    lab.jj_work(["edit", RELEASE]);
    for (file, content) in files {
        std::fs::write(lab.work.join(file), content).expect("write the resolution");
    }
    lab.jj_work(["new"]);
}

/// Alpha and beta, each writing `shared.txt` its own way, cut as the release:
/// the cut leaves `shared.txt` conflicted.
fn conflicted_cut() -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert!(
        stdout(&cut).contains(&format!("cut {RELEASE} as")),
        "the first cut was refused: {}",
        text(&cut)
    );
    assert_eq!(
        knives::jj::conflicted_files(&lab.work, RELEASE).expect("list conflicts"),
        ["shared.txt"]
    );
    (lab, home)
}

/// Alpha and beta on files of their own, cut as the release.
fn two_branch_cut() -> (Lab, tempfile::TempDir, std::path::PathBuf) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, consumer) = home_after_first_cut(&lab);
    (lab, home, consumer)
}

#[test]
fn a_shared_file_conflict_resolved_by_hand_is_adopted_and_audits_clean() {
    // Given: a cut that left shared.txt conflicted, resolved by hand on the
    // release commit, which audit reads as drift from the cut.
    let (lab, home) = conflicted_cut();
    let cut = commit_at(&lab, RELEASE);
    finish_in_place(&lab, &[("shared.txt", "alpha\nbeta\n")]);
    let resolved = commit_at(&lab, RELEASE);
    let before = audit_report(&lab, &home);
    assert_eq!(findings(&before, "release-drift").len(), 1, "{before}");

    // When: the release is adopted.
    let output = adopt(&lab, &home, &[]);

    // Then: it is recorded against the cut, naming the file resolved by hand
    // and each member's added line as held.
    let out = stdout(&output);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        out.contains(&format!(
            "demo: adopted {} as the newest write of {RELEASE}, replacing the recorded cut {}\n  \
             baseline: the recorded cut {}\n  1 file(s) resolved by hand:\n    shared.txt\n",
            resolved.short(),
            cut.short(),
            cut.short()
        )),
        "{out}"
    );
    for member in ["feat/alpha", "feat/beta"] {
        assert!(
            out.contains(&format!(
                "  {member}: 0 of 1 added line(s) of shared.txt absent from the resolution\n"
            )),
            "{out}"
        );
    }
    let event = newest_event(&home, RELEASE);
    let change = knives::jj::Repo::open(&lab.work)
        .expect("open")
        .change_id_of(resolved.as_str())
        .expect("change id");
    assert!(
        event.text.starts_with(&format!(
            "edited {RELEASE} as {} (change {}) with 2 parent(s): adopted: replaces the recorded \
             cut {}; measured against the recorded cut {}; resolved by hand: shared.txt",
            resolved.short(),
            change.short(),
            cut.short(),
            cut.short()
        )),
        "{}",
        event.text
    );
    let mut evidence = vec![resolved.as_str().to_owned()];
    evidence.extend(
        release_parents(&lab, RELEASE)
            .iter()
            .map(|parent| parent.as_str().to_owned()),
    );
    assert_eq!(event.evidence, evidence);

    // And: the audit reads the release as knives' own.
    let report = audit_report(&lab, &home);
    assert!(findings(&report, "release-drift").is_empty(), "{report}");
    assert!(findings(&report, "orphan-commit").is_empty(), "{report}");
}

#[test]
fn a_resolution_that_drops_every_line_a_member_added_is_recorded_with_findings() {
    // Given: shared.txt resolved to alpha's side, so beta's file is back at
    // the base: none of the lines beta added is held.
    let (lab, home) = conflicted_cut();
    finish_in_place(&lab, &[("shared.txt", "alpha\n")]);
    let resolved = commit_at(&lab, RELEASE);

    // When: the release is adopted.
    let output = adopt(&lab, &home, &[]);

    // Then: it is recorded, and beta's count is printed, flagged, and makes
    // the exit findings; alpha's lines are all held.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        out.contains(
            "  !! feat/beta: 1 of 1 added line(s) of shared.txt absent from the resolution: \
             every line it added is gone\n"
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "  feat/alpha: 0 of 1 added line(s) of shared.txt absent from the resolution\n"
        ),
        "{out}"
    );
    let event = newest_event(&home, RELEASE);
    assert_eq!(
        event.evidence.first().map(String::as_str),
        Some(resolved.as_str())
    );
    assert!(
        event.text.contains(
            "added lines absent: feat/alpha 0 of 1 in shared.txt, feat/beta 1 of 1 in shared.txt"
        ),
        "{}",
        event.text
    );
}

#[test]
fn adopting_the_recorded_write_itself_writes_nothing() {
    // Given: a release at the cut knives recorded.
    let (lab, home, _consumer) = two_branch_cut();
    let cut = commit_at(&lab, RELEASE);
    let before = ledger_entries(&home);

    // When: it is adopted.
    let output = adopt(&lab, &home, &[]);

    // Then: there is nothing to adopt, and the ledger is unchanged.
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!(
            "demo: {RELEASE} is at {}, the recorded cut {}; nothing to adopt",
            cut.short(),
            cut.short()
        )),
        "{}",
        text(&output)
    );
    assert_eq!(ledger_entries(&home), before);

    // And when: the release is re-signed: a new commit id, the same change,
    // parents and tree.
    lab.jj_work([
        "metaedit",
        "--force-rewrite",
        "--ignore-immutable",
        "-r",
        RELEASE,
    ]);
    let rewritten = commit_at(&lab, RELEASE);
    assert_ne!(rewritten, cut, "the metaedit made no new commit");
    let output = adopt(&lab, &home, &[]);

    // Then: that is the same write, and nothing is recorded either.
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!(
            "demo: {RELEASE} is at {}, the recorded cut {} re-signed or metaedited (same \
             change, parents and tree); nothing to adopt",
            rewritten.short(),
            cut.short()
        )),
        "{}",
        text(&output)
    );
    assert_eq!(ledger_entries(&home), before);
}

#[test]
fn adopt_refuses_when_the_plan_has_problems() {
    // Given: a hand-repaired release, and a consumer path that is not there.
    let (lab, home, consumer) = two_branch_cut();
    finish_in_place(&lab, &[("alpha.txt", "alpha, repaired\n")]);
    std::fs::write(
        home.path().join("local-consumer"),
        consumer
            .with_file_name("consumer-that-was-never-cloned")
            .display()
            .to_string(),
    )
    .expect("point the release helpers at a missing consumer");
    let before = ledger_entries(&home);

    // When: it is adopted with a reason.
    let output = adopt(&lab, &home, &["--why", "a repair"]);

    // Then: refused before anything else, and nothing is recorded.
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        stdout(&output).contains("demo: adopt not applied; the plan could not answer:"),
        "{}",
        text(&output)
    );
    assert_eq!(ledger_entries(&home), before);
}

#[test]
fn adopting_another_series_release_is_a_usage_error_naming_that_series() {
    // Given: a fork with a series `old` on its own base beside the default
    // series.
    let lab = Lab::new();
    lab.upstream_branch("old", "old.txt", "old base\n");
    lab.advance_upstream("new base\n");
    let (home, _consumer) = release_test_home(&lab);
    let registry = home.path().join("repos.toml");
    let mut config = std::fs::read_to_string(&registry).expect("read registry");
    config.push_str("\n[repos.demo.series.old]\nbase = \"old\"\n");
    std::fs::write(&registry, config).expect("write registry");

    // When: the default series is asked to adopt series old's cut.
    let output = adopt(&lab, &home, &["release/old/2026-08-04"]);

    // Then: a usage error that names the series owning the name.
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "demo: release/old/2026-08-04 is a release of the series old (base old@upstream), \
             not of the default series (base main@upstream); adopt it with --series old"
        ),
        "{}",
        text(&output)
    );
}

#[test]
fn adopt_refuses_a_divergent_release() {
    // Given: the release rewritten in two clones, so its local bookmark has
    // two targets after a fetch.
    let (lab, home, _consumer) = two_branch_cut();
    lab.rewrite_in_both_clones(RELEASE);
    let before = ledger_entries(&home);

    // When: it is adopted.
    let output = adopt(&lab, &home, &[RELEASE]);

    // Then: refused: there is no one commit to adopt.
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!(
            "demo: {RELEASE} is divergent (2 targets), so there is no one commit to adopt; \
             resolve its divergence first"
        )),
        "{}",
        text(&output)
    );
    assert_eq!(ledger_entries(&home), before);
}

#[test]
fn adopt_refuses_a_name_with_no_bookmark_here_or_on_the_publish_remote() {
    // Given: a cut never pushed, whose local bookmark is gone.
    let (lab, home, _consumer) = two_branch_cut();
    lab.jj_work(["bookmark", "forget", RELEASE]);
    let before = ledger_entries(&home);

    // When: it is adopted by name.
    let output = adopt(&lab, &home, &[RELEASE]);

    // Then: refused: nothing holds the name, even after a fetch.
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!(
            "demo: {RELEASE} has no local bookmark, and origin holds no {RELEASE} after a \
             fetch; there is no commit to adopt"
        )),
        "{}",
        text(&output)
    );
    assert_eq!(ledger_entries(&home), before);
}

#[test]
fn adopt_refuses_a_release_with_no_recorded_write() {
    // Given: a release merged by hand, which no cut or edit recorded.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(RELEASE, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    let before = ledger_entries(&home);

    // When: it is adopted.
    let output = adopt(&lab, &home, &[]);

    // Then: refused: there is no write for it to replace.
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!(
            "demo: the ledger records no cut or edit of {RELEASE}, so there is no write for {} \
             to replace; cut the release first",
            commit_at(&lab, RELEASE).short()
        )),
        "{}",
        text(&output)
    );
    assert_eq!(ledger_entries(&home), before);
}

#[test]
fn adopt_refuses_an_extra_parent_that_is_no_earlier_write_and_names_it() {
    // Given: the cut given one more parent by hand, a branch no write of the
    // name ever was.
    let (lab, home, _consumer) = two_branch_cut();
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let gamma = commit_at(&lab, "feat/gamma");
    lab.jj_work([
        "rebase",
        "-r",
        RELEASE,
        "-d",
        "parents(release/2026-08-04) | feat/gamma",
    ]);
    assert!(release_parents(&lab, RELEASE).contains(&gamma));
    let before = ledger_entries(&home);

    // When: it is adopted.
    let output = adopt(&lab, &home, &[]);

    // Then: refused, naming the extra parent.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        out.contains(&format!(
            "  extra parent {} (feat/gamma): no parent of the recorded write, and no earlier \
             write of {RELEASE}\n",
            gamma.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), before);
}

#[test]
fn adopt_refuses_a_resolution_made_as_a_child_of_the_release() {
    // Given: the conflict resolved in a new commit on top of the cut, the
    // release name moved onto it.
    let (lab, home) = conflicted_cut();
    let cut = commit_at(&lab, RELEASE);
    lab.jj_work(["new", RELEASE, "-m", "resolve shared.txt"]);
    std::fs::write(lab.work.join("shared.txt"), "alpha\nbeta\n").expect("resolve");
    lab.jj_work(["bookmark", "set", RELEASE, "-r", "@"]);
    lab.jj_work(["new"]);
    let child = commit_at(&lab, RELEASE);
    let before = ledger_entries(&home);

    // When: it is adopted.
    let output = adopt(&lab, &home, &[]);

    // Then: refused, saying to squash the resolution into the release commit.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        out.contains(&format!(
            "  {} is a child of the release commit {}: squash the resolution into the release \
             commit (`jj squash --from {} --into {}`), then adopt again",
            child.short(),
            cut.short(),
            child.short(),
            cut.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), before);
}

#[test]
fn adopt_refuses_a_release_that_still_has_conflicts_and_names_them() {
    // Given: the conflicted cut changed by hand elsewhere, shared.txt still
    // conflicted.
    let (lab, home) = conflicted_cut();
    finish_in_place(&lab, &[("notes.txt", "unrelated\n")]);
    let before = ledger_entries(&home);

    // When: it is adopted.
    let output = adopt(&lab, &home, &["--why", "a note"]);

    // Then: refused, naming the conflicted file.
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        stdout(&output).contains(
            "still has 1 conflicted file(s), so it is not resolved yet; resolve them, then \
             adopt:\n    shared.txt\n"
        ),
        "{}",
        text(&output)
    );
    assert_eq!(ledger_entries(&home), before);
}

#[test]
fn adopt_refuses_a_release_a_working_copy_has_checked_out() {
    // Given: the conflict resolved in the working copy, which is still the
    // release commit.
    let (lab, home) = conflicted_cut();
    lab.jj_work(["edit", RELEASE]);
    std::fs::write(lab.work.join("shared.txt"), "alpha\nbeta\n").expect("resolve");
    lab.jj_work(["status"]);
    let before = ledger_entries(&home);

    // When: it is adopted.
    let output = adopt(&lab, &home, &[]);

    // Then: refused, saying to run `jj new` there first.
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!(
            "demo: {RELEASE} at {} is the working copy of workspace default; run `jj new` there \
             first",
            commit_at(&lab, RELEASE).short()
        )),
        "{}",
        text(&output)
    );
    assert_eq!(ledger_entries(&home), before);
}

#[test]
fn adopt_refuses_over_a_published_write_the_publish_remote_does_not_hold() {
    // Given: a pushed cut marked published by a `record: published` note,
    // then repaired by hand and not pushed.
    let (lab, home, _consumer) = two_branch_cut();
    let cut = commit_at(&lab, RELEASE);
    lab.push_branch(RELEASE);
    let noted = lab::knives(
        &lab,
        &home,
        &[
            "notch",
            RELEASE,
            "-m",
            "record: published as v1.0.0",
            "--evidence",
            cut.as_str(),
            "--repo",
            "demo",
        ],
    );
    assert!(noted.status.success(), "{}", text(&noted));
    finish_in_place(&lab, &[("alpha.txt", "alpha, repaired\n")]);
    let repaired = commit_at(&lab, RELEASE);
    let before = ledger_entries(&home);

    // When: it is adopted, with a reason for the repair.
    let output = adopt(&lab, &home, &["--why", "a repair"]);

    // Then: refused: consumers were given the cut, and origin still holds it.
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!(
            "demo: {RELEASE} is published (a record: published note names {}); adopting it \
             would move a published release: cut a new dated release first, then adopt that; \
             origin does not hold {}",
            cut.short(),
            repaired.short()
        )),
        "{}",
        text(&output)
    );
    assert_eq!(ledger_entries(&home), before);
}

#[test]
fn adopt_refuses_a_change_beyond_the_conflicts_without_a_why() {
    // Given: a cut with no conflict, repaired by hand.
    let (lab, home, _consumer) = two_branch_cut();
    let cut = commit_at(&lab, RELEASE);
    finish_in_place(&lab, &[("alpha.txt", "alpha, repaired\n")]);
    let before = ledger_entries(&home);

    // When: it is adopted with no reason.
    let output = adopt(&lab, &home, &[]);

    // Then: refused, naming the file.
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!(
            "changes 1 file(s) the recorded cut {} left without a conflict, which is a repair or \
             a renumber rather than a resolution; say why with --why, which the record keeps:\n    \
             alpha.txt\n",
            cut.short()
        )),
        "{}",
        text(&output)
    );
    assert_eq!(ledger_entries(&home), before);
}

#[test]
fn a_restore_beyond_the_conflicts_that_drops_a_members_line_is_recorded_with_findings() {
    // Given: a cut with no conflict, then alpha's file restored from the
    // trunk on the release by hand: a file the cut left without a conflict,
    // every line alpha added to it gone.
    let (lab, home, _consumer) = two_branch_cut();
    lab.jj_work([
        "restore",
        "--from",
        "main@origin",
        "--into",
        RELEASE,
        "alpha.txt",
    ]);
    let restored = commit_at(&lab, RELEASE);

    // When: it is adopted, with a reason for the change.
    let output = adopt(&lab, &home, &["--why", "a restore"]);

    // Then: it is recorded, and alpha's count in the file changed beyond the
    // conflicts is printed, flagged, and makes the exit findings.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        out.contains(
            "  1 file(s) changed beyond the baseline's conflicts, because: a restore\n    \
             alpha.txt\n"
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "  !! feat/alpha: 1 of 1 added line(s) of alpha.txt absent from the resolution: \
             every line it added is gone\n"
        ),
        "{out}"
    );
    let event = newest_event(&home, RELEASE);
    assert_eq!(
        event.evidence.first().map(String::as_str),
        Some(restored.as_str())
    );
    assert!(
        event
            .text
            .contains("added lines absent: feat/alpha 1 of 1 in alpha.txt"),
        "{}",
        event.text
    );
}

/// The base release log split where every ygo member inserts its own block:
/// above the first `## ` section.
fn ygo_preamble_and_sections() -> (&'static str, &'static str) {
    let at = BASE_LOG
        .find("\n## ")
        .map_or(BASE_LOG.len(), |newline| newline + 1);
    BASE_LOG.split_at(at)
}

/// The block a ygo member's `log` inserts above the base's sections.
fn block(log: &str) -> &str {
    let (preamble, sections) = ygo_preamble_and_sections();
    &log[preamble.len()..log.len() - sections.len()]
}

/// A lab whose trunk, upstream and on the fork, holds both ygo release logs.
fn ygo_lab() -> Lab {
    let lab = Lab::new();
    lab.upstream_trunk_file("CHANGELOG.md", BASE_LOG);
    lab.upstream_trunk_file("RELEASE_NOTES.md", BASE_NOTES);
    lab.mirror_upstream_trunk_to_origin();
    lab
}

/// A ygo member forked from the fork's trunk writing both release logs.
fn ygo_member(lab: &Lab, name: &str, (log, notes): (&str, &str)) {
    lab.jj_work(["new", "-r", "main@origin", "-m", name]);
    std::fs::write(lab.work.join("CHANGELOG.md"), log).expect("write the log");
    std::fs::write(lab.work.join("RELEASE_NOTES.md"), notes).expect("write the notes");
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// A commit on `branch` writing `files`, the bookmark moved onto it.
fn grow(lab: &Lab, branch: &str, files: &[(&str, &str)]) {
    lab.jj_work(["new", branch, "-m", "follow-up"]);
    for (file, content) in files {
        std::fs::write(lab.work.join(file), content).expect("write a member's file");
    }
    lab.jj_work(["bookmark", "set", branch, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// The first cut of split-from-store and sami11, both logs unioned.
fn unioned_first_cut() -> (Lab, tempfile::TempDir) {
    let lab = ygo_lab();
    ygo_member(&lab, SPLIT, (SPLIT_LOG, SPLIT_NOTES));
    ygo_member(&lab, SAMI11, (SAMI11_LOG, SAMI11_NOTES));
    let (home, _consumer) = release_test_home(&lab);
    release_ok(&lab, &home, &["cut", RELEASE]);
    assert_eq!(
        knives::jj::conflicted_files(&lab.work, RELEASE).expect("list conflicts"),
        Vec::<String>::new()
    );
    (lab, home)
}

#[test]
fn a_renumber_by_hand_is_adopted_only_with_a_why_and_keeps_the_recorded_parents() {
    // Given: the unioned cut, then sami11's bookmark moved on past the
    // release's parent, then the release renumbered by hand as a commit of
    // its own on the same parents, the name moved onto it.
    let (lab, home) = unioned_first_cut();
    let cut = commit_at(&lab, RELEASE);
    let cut_event = newest_event(&home, RELEASE);
    grow(&lab, SAMI11, &[("sami11.txt", "follow-up\n")]);
    let parents = release_parents(&lab, RELEASE);
    let renumbered = file_at_revision(&lab, RELEASE, "CHANGELOG.md").replacen(
        "## [1.55.1-sami.11] — 2026-11-13",
        "## [1.55.1-sami.12] — 2026-11-13",
        1,
    );
    lab.jj_work([
        "new",
        parents[0].as_str(),
        parents[1].as_str(),
        "-m",
        "release/2026-08-04, renumbered",
    ]);
    lab.jj_work(["restore", "--from", RELEASE]);
    std::fs::write(lab.work.join("CHANGELOG.md"), &renumbered).expect("renumber");
    lab.jj_work(["bookmark", "set", RELEASE, "-r", "@", "--allow-backwards"]);
    lab.jj_work(["new"]);
    let adopted = commit_at(&lab, RELEASE);
    let before = audit_report(&lab, &home);
    assert_eq!(findings(&before, "release-drift").len(), 1, "{before}");
    assert_eq!(findings(&before, "orphan-commit").len(), 1, "{before}");
    let entries = ledger_entries(&home);

    // When: it is adopted with no reason.
    let refused = adopt(&lab, &home, &[]);

    // Then: refused, naming the renumbered log, and nothing is recorded.
    assert_eq!(refused.status.code(), Some(3), "{}", text(&refused));
    assert!(
        stdout(&refused).contains(&format!(
            "changes 1 file(s) the recorded cut {} left without a conflict, which is a repair or \
             a renumber rather than a resolution; say why with --why, which the record keeps:\n    \
             CHANGELOG.md\n",
            cut.short()
        )),
        "{}",
        text(&refused)
    );
    assert_eq!(ledger_entries(&home), entries);

    // When: it is adopted with the reason.
    let output = adopt(&lab, &home, &["--why", "renumbered sami11's section"]);

    // Then: recorded with the reason, the cut it replaced abandoned.
    let out = stdout(&output);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        out.contains(
            "  no file resolved by hand\n  1 file(s) changed beyond the baseline's conflicts, \
             because: renumbered sami11's section\n    CHANGELOG.md\n"
        ),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "demo: abandoned {}, an earlier {RELEASE} commit nothing rests on any more",
            cut.short()
        )),
        "{out}"
    );
    let event = newest_event(&home, RELEASE);
    assert_eq!(
        event.evidence.first().map(String::as_str),
        Some(adopted.as_str())
    );
    assert!(
        event.text.contains(
            "changed beyond the baseline's conflicts: CHANGELOG.md (why: renumbered sami11's \
             section)"
        ),
        "{}",
        event.text
    );

    // And: its parent set is the cut's as the cut recorded it, sami11's
    // branch name included though the bookmark has moved on.
    assert_eq!(event.parents, cut_event.parents);
    assert!(
        event
            .parents
            .iter()
            .any(|parent| parent.branches.contains(&SAMI11.to_owned())),
        "{:?}",
        event.parents
    );

    // And: the audit finds neither drift nor an anonymous head.
    let report = audit_report(&lab, &home);
    assert!(findings(&report, "release-drift").is_empty(), "{report}");
    assert!(findings(&report, "orphan-commit").is_empty(), "{report}");
}

#[test]
fn the_ygo_rewritten_member_conflict_resolved_by_hand_is_adopted() {
    // Given: the ygo history's shape: the unioned cut, client-count included,
    // then split-from-store grown into split-one-path, which rewrote its
    // block, and advanced: CHANGELOG.md is left conflicted for a person.
    let (lab, home) = unioned_first_cut();
    ygo_member(&lab, CLIENT, (CLIENT_LOG, CLIENT_NOTES));
    release_ok(&lab, &home, &["include", CLIENT]);
    grow(
        &lab,
        SPLIT,
        &[
            ("CHANGELOG.md", SPLIT_ONE_PATH_LOG),
            ("RELEASE_NOTES.md", SPLIT_ONE_PATH_NOTES),
        ],
    );
    release_ok(&lab, &home, &["advance", SPLIT]);
    assert_eq!(
        knives::jj::conflicted_files(&lab.work, RELEASE).expect("list conflicts"),
        ["CHANGELOG.md"]
    );
    let advanced = commit_at(&lab, RELEASE);

    // When: a person resolves it with every member's block, the rewritten
    // one in place of the block it replaced, and the release is adopted.
    let (preamble, sections) = ygo_preamble_and_sections();
    let resolution = format!(
        "{preamble}{}{}{}{sections}",
        block(CLIENT_LOG),
        block(SPLIT_ONE_PATH_LOG),
        block(SAMI11_LOG)
    );
    finish_in_place(&lab, &[("CHANGELOG.md", &resolution)]);
    let output = adopt(&lab, &home, &[]);

    // Then: recorded against the advance, CHANGELOG.md resolved by hand,
    // every member's added lines held.
    let out = stdout(&output);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        out.contains(&format!(
            "  baseline: the recorded edit {}\n  1 file(s) resolved by hand:\n    CHANGELOG.md\n",
            advanced.short()
        )),
        "{out}"
    );
    for member in [SPLIT, SAMI11, CLIENT] {
        assert!(
            out.contains(&format!("  {member}: 0 of ")),
            "{member} is not counted as held: {out}"
        );
    }
    assert!(!out.contains("!!"), "{out}");

    // And: the audit reads the release as knives' own.
    let report = audit_report(&lab, &home);
    assert!(findings(&report, "release-drift").is_empty(), "{report}");
    assert!(findings(&report, "orphan-commit").is_empty(), "{report}");
}

#[test]
fn adopting_a_superseded_name_only_the_publish_remote_holds_clears_its_drift() {
    // Given: a conflicted cut resolved by hand, pushed and marked published,
    // then superseded by a cut of the same composition under a newer name,
    // whose reap forgot the old name here; a fetch brings back the publish
    // remote's ref of it, at the resolution no event records.
    let (lab, home) = conflicted_cut();
    finish_in_place(&lab, &[("shared.txt", "alpha\nbeta\n")]);
    let resolved = commit_at(&lab, RELEASE);
    lab.push_branch(RELEASE);
    let noted = lab::knives(
        &lab,
        &home,
        &[
            "notch",
            RELEASE,
            "-m",
            "record: published as v1.0.0",
            "--evidence",
            resolved.as_str(),
            "--repo",
            "demo",
        ],
    );
    assert!(noted.status.success(), "{}", text(&noted));
    // The working copy steps off the release, or the reap keeps it for that.
    lab.jj_work(["new", "main@origin"]);
    let newer = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(
        stdout(&newer).contains("cut release/2026-08-05 as"),
        "{}",
        text(&newer)
    );
    let opened = knives::jj::Repo::open(&lab.work).expect("open");
    assert_eq!(opened.local_bookmark_tip(RELEASE), None, "{}", text(&newer));
    lab.fetch_work();
    assert_eq!(commit_at(&lab, &format!("{RELEASE}@origin")), resolved);
    let before = audit_report(&lab, &home);
    assert!(
        findings(&before, "release-drift")
            .iter()
            .any(|finding| finding["detail"]
                .as_str()
                .is_some_and(|detail| detail.starts_with(&format!("publish remote {RELEASE}")))),
        "{before}"
    );

    // When: the old name is adopted.
    let output = adopt(&lab, &home, &[RELEASE]);

    // Then: the publish remote's commit is recorded as its newest write.
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(
        newest_event(&home, RELEASE)
            .evidence
            .first()
            .map(String::as_str),
        Some(resolved.as_str())
    );

    // And: the audit finds no drift for the publish remote's copy.
    let report = audit_report(&lab, &home);
    assert!(findings(&report, "release-drift").is_empty(), "{report}");
}

/// A conflicted cut made in the work checkout and never pushed, and the same
/// composition resolved by hand in the second clone, pushed from there when
/// `push` says so. The second clone never held the cut; it is given the
/// upstream remote, so it is the fork's checkout as the work checkout is.
/// Returns the lab, its home, the cut and the resolution.
fn resolved_in_second_clone(
    push: bool,
) -> (
    Lab,
    tempfile::TempDir,
    knives::ids::CommitId,
    knives::ids::CommitId,
) {
    let (lab, home) = conflicted_cut();
    let cut = commit_at(&lab, RELEASE);
    lab.push_branch("feat/alpha");
    lab.push_branch("feat/beta");
    let second = lab.second.clone();
    let upstream = lab.upstream.display().to_string();
    lab.jj_at(
        &second,
        ["git", "remote", "add", "upstream", upstream.as_str()],
    );
    lab.jj_at(&second, ["git", "fetch", "--all-remotes"]);
    lab.jj_at(
        &second,
        [
            "new",
            "feat/alpha@origin",
            "feat/beta@origin",
            "-m",
            "release/2026-08-04",
        ],
    );
    std::fs::write(second.join("shared.txt"), "alpha\nbeta\n").expect("resolve");
    lab.jj_at(&second, ["bookmark", "create", RELEASE, "-r", "@"]);
    lab.jj_at(&second, ["new"]);
    if push {
        lab.jj_at(
            &second,
            ["git", "push", "--remote", "origin", "--bookmark", RELEASE],
        );
    }
    let opened = knives::jj::Repo::open(&second).expect("open the second clone");
    let resolved = opened.resolve_commit(RELEASE).expect("the resolution");
    assert!(
        opened.resolve_commit(cut.as_str()).is_err(),
        "the cut is here"
    );
    (lab, home, cut, resolved)
}

/// `knives release adopt` run in the second clone, whatever its exit.
fn adopt_in_second_clone(lab: &Lab, home: &tempfile::TempDir) -> std::process::Output {
    release_command(lab, home, ReleaseOutput::Text, &["adopt"])
        .current_dir(&lab.second)
        .output()
        .expect("run knives release adopt")
}

#[test]
fn adopting_where_the_recorded_write_is_missing_measures_against_an_automerge() {
    // Given: a cut the work checkout made and never pushed, resolved by hand
    // in the second clone and pushed from there.
    let (lab, home, cut, resolved) = resolved_in_second_clone(true);

    // When: the release is adopted in the second clone.
    let output = adopt_in_second_clone(&lab, &home);

    // Then: it is measured against a fresh automerge of its parents, which
    // the output names, and recorded.
    let out = stdout(&output);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        out.contains(&format!(
            "  baseline: an automerge of its 2 member parent(s) with the release-log union, \
             because this repository does not hold the recorded cut {}\n  1 file(s) resolved by \
             hand:\n    shared.txt\n",
            cut.short()
        )),
        "{out}"
    );
    assert_eq!(
        newest_event(&home, RELEASE)
            .evidence
            .first()
            .map(String::as_str),
        Some(resolved.as_str())
    );
}

#[test]
fn adopt_refuses_over_a_published_write_this_repository_does_not_hold() {
    // Given: a cut the work checkout made, which a `record: published` note
    // names, resolved by hand in the second clone and not pushed.
    let (lab, home, cut, resolved) = resolved_in_second_clone(false);
    let noted = lab::knives(
        &lab,
        &home,
        &[
            "notch",
            RELEASE,
            "-m",
            "record: published as v1.0.0",
            "--evidence",
            cut.as_str(),
            "--repo",
            "demo",
        ],
    );
    assert!(noted.status.success(), "{}", text(&noted));
    let before = ledger_entries(&home);

    // When: the release is adopted in the second clone, which never held the
    // cut.
    let output = adopt_in_second_clone(&lab, &home);

    // Then: refused from the note alone: consumers were given the cut, and
    // origin does not hold the resolution.
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!(
            "demo: {RELEASE} is published (a record: published note names {}); adopting it \
             would move a published release: cut a new dated release first, then adopt that; \
             origin does not hold {}",
            cut.short(),
            resolved.short()
        )),
        "{}",
        text(&output)
    );
    assert_eq!(ledger_entries(&home), before);
}

#[test]
fn adopt_refuses_when_the_upstream_trunk_does_not_resolve() {
    // Given: a conflict resolved by hand on the release, and a registry whose
    // base names a branch upstream does not have: no member has a fork point
    // to count its lost lines from.
    let (lab, home) = conflicted_cut();
    finish_in_place(&lab, &[("shared.txt", "alpha\nbeta\n")]);
    let registry = home.path().join("repos.toml");
    let mut config = std::fs::read_to_string(&registry).expect("read registry");
    config.push_str("base = \"missing\"\n");
    std::fs::write(&registry, config).expect("write broken-trunk registry");
    let before = ledger_entries(&home);

    // When: it is adopted.
    let output = adopt(&lab, &home, &[]);

    // Then: refused as every release edit refuses it, and nothing recorded.
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        stdout(&output).contains(
            "demo: cannot resolve missing@upstream; release edits classify parents against the \
             upstream trunk, so fetch upstream first"
        ),
        "{}",
        text(&output)
    );
    assert_eq!(ledger_entries(&home), before);
}

/// A registry home whose entry releases on the fixed branch `integration`,
/// with an empty local consumer.
fn fixed_home(lab: &Lab) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\n\
             release_branch = \"integration\"\n",
            lab.upstream.display(),
        ),
    )
    .expect("write fixed-scheme registry");
    let consumer = home.path().join("consumer");
    std::fs::create_dir_all(&consumer).expect("create local consumer");
    std::fs::write(
        home.path().join("local-consumer"),
        consumer.display().to_string(),
    )
    .expect("write local consumer fixture path");
    home
}

#[test]
fn a_fast_forward_carrier_is_adopted_and_labelled_as_no_member() {
    // Given: a fixed release cut and pushed, then edited, then made a
    // fast-forward of the pushed position by hand: rebased onto its parents
    // and the publish remote's commit, its tree put back.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let home = fixed_home(&lab);
    release_ok(&lab, &home, &["cut"]);
    let published = commit_at(&lab, "integration");
    lab.push_branch("integration");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    release_ok(&lab, &home, &["include", "feat/gamma"]);
    let edited = commit_at(&lab, "integration");
    let edit_event = newest_event(&home, "integration");
    lab.jj_work([
        "rebase",
        "-r",
        "integration",
        "-d",
        "parents(integration) | integration@origin",
    ]);
    lab.jj_work([
        "restore",
        "--from",
        edited.as_str(),
        "--into",
        "integration",
    ]);
    assert!(release_parents(&lab, "integration").contains(&published));

    // When: the release is adopted.
    let output = adopt(&lab, &home, &[]);

    // Then: recorded, the pushed position named as a carrier.
    let out = stdout(&output);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        out.contains(&format!(
            "  carrier {}: integration earlier write (carrier, not a member)\n",
            published.short()
        )),
        "{out}"
    );

    // And: the event's parents are the edit's, plus the carrier, labelled.
    let event = newest_event(&home, "integration");
    let mut expected = edit_event.parents;
    expected.push(RecordedParent {
        commit: published.as_str().to_owned(),
        branches: vec!["integration earlier write (carrier, not a member)".to_owned()],
    });
    assert_eq!(event.parents, expected);

    // And: the commit the release holds is the one recorded, and the audit
    // finds no drift.
    assert_eq!(
        event.evidence.first().map(String::as_str),
        Some(commit_at(&lab, "integration").as_str())
    );
    let report = audit_report(&lab, &home);
    assert!(findings(&report, "release-drift").is_empty(), "{report}");
}
