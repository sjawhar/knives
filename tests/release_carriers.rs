//! Fast-forward carriers: an earlier write of a release that a push made a
//! fast-forward of its published position gave it as one more parent.
//!
//! `adopt` records such a release with the carrier labelled; the plan and
//! `members` label it and count it as no stacked member; every write that
//! composes from the release's parents (`include`, `drop`, `advance`, `cut`,
//! `rebase`) leaves it out while keeping the resolutions it holds; and a
//! write hides what it leaves a head nothing holds only when that is an
//! earlier write of the name or a commit a visible rewrite supersedes. A
//! drop names a loss only when the written tree lost something.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use std::collections::BTreeMap;

use knives::ids::{ChangeId, CommitId};
use knives::jj::Repo;
use lab::{Lab, commit_at, extend_branch, file_at_revision, knives_release, release_parents};

/// The fixed release branch every lab here releases on.
const RELEASE: &str = "integration";

/// How the plan, `members` and `adopt` name a carrier of [`RELEASE`].
const LABEL: &str = "integration earlier write (carrier, not a member)";

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

/// A registry home whose entry releases on the fixed branch [`RELEASE`],
/// with an empty local consumer.
fn fixed_home(lab: &Lab) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\n\
             release_branch = \"{RELEASE}\"\n",
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

/// `knives --json audit demo --no-github`, parsed, against the lab's own
/// origin with the same ledger; the registry is put back afterwards.
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

/// Assert the audit finds no anonymous head, saying after what.
fn assert_no_anonymous_head(lab: &Lab, home: &tempfile::TempDir, after: &str) {
    let report = audit_report(lab, home);
    assert!(
        findings(&report, "orphan-commit").is_empty(),
        "after {after}: {report}"
    );
}

/// The audit's anonymous-head findings, each as its rendered detail.
fn anonymous_heads(lab: &Lab, home: &tempfile::TempDir) -> Vec<String> {
    findings(&audit_report(lab, home), "orphan-commit")
        .into_iter()
        .map(ToString::to_string)
        .collect()
}

fn is_visible(lab: &Lab, commit: &CommitId) -> bool {
    knives::jj::commits_matching(&lab.work, "all()")
        .expect("list visible commits")
        .contains(commit)
}

fn change_of(lab: &Lab, commit: &CommitId) -> ChangeId {
    Repo::open(&lab.work)
        .expect("open")
        .change_id_of(commit.as_str())
        .expect("read a change id")
}

/// Step the working copy off every member and release onto an empty child
/// of the trunk, so that no working copy the lab's branch helper left sits on them.
fn step_off_onto_trunk(lab: &Lab) {
    lab.jj_work(["new", "main@origin"]);
}

/// Make the release a fast-forward of its pushed position by hand, as a
/// person does before a push: rebase it onto its parents and the publish
/// remote's commit, then put its tree back. Returns the commit before the
/// rebase, whose tree the release holds again.
fn fast_forward(lab: &Lab) -> CommitId {
    let before = commit_at(lab, RELEASE);
    lab.jj_work([
        "rebase",
        "-r",
        RELEASE,
        "-d",
        "parents(integration) | integration@origin",
    ]);
    lab.jj_work(["restore", "--from", before.as_str(), "--into", RELEASE]);
    before
}

/// `knives release adopt`, which must succeed; its stdout.
fn adopt_ok(lab: &Lab, home: &tempfile::TempDir) -> String {
    release_ok(lab, home, &["adopt"])
}

/// A fixed release whose pushed position is a carrier of the release in
/// hand, adopted and pushed.
struct CarrierState {
    lab: Lab,
    home: tempfile::TempDir,
    /// The first cut, pushed, which the fast-forward made a parent.
    carrier: CommitId,
    /// The adopted release, pushed.
    release: CommitId,
}

/// `members` on files of their own: cut, pushed, `feat/gamma` included, the
/// push made a fast-forward by hand, adopted and pushed again. The working
/// copy sits on the trunk.
fn carrier_state(members: &[(&str, &str, &str)]) -> CarrierState {
    let lab = Lab::new();
    for (branch, file, content) in members {
        lab.branch(branch, file, content);
    }
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ok(&lab, &home, &["cut"]);
    adopted_carrier(lab, home)
}

/// From a cut release in hand: push it, include `feat/gamma`, make the push
/// a fast-forward by hand, adopt and push.
fn adopted_carrier(lab: Lab, home: tempfile::TempDir) -> CarrierState {
    let carrier = commit_at(&lab, RELEASE);
    lab.push_branch(RELEASE);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    release_ok(&lab, &home, &["include", "feat/gamma"]);
    fast_forward(&lab);
    assert!(release_parents(&lab, RELEASE).contains(&carrier));
    let adopted = adopt_ok(&lab, &home);
    assert!(
        adopted.contains(&format!("  carrier {}: {LABEL}\n", carrier.short())),
        "{adopted}"
    );
    lab.push_branch(RELEASE);
    let release = commit_at(&lab, RELEASE);
    CarrierState {
        lab,
        home,
        carrier,
        release,
    }
}

/// Alpha and beta both writing `shared.txt`, cut with the conflict, resolved
/// by hand on the release to keep both lines and adopted, then carried the
/// way [`adopted_carrier`] carries it: the carrier holds the resolution.
fn resolved_carrier_state() -> CarrierState {
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    // The conflict leaves the cut's audit inconclusive, which exits with
    // findings; the cut is written all the same.
    let cut = knives_release(&lab, &home, &["cut"]);
    assert!(
        stdout(&cut).contains(&format!("cut {RELEASE} as")),
        "{}",
        text(&cut)
    );
    assert_eq!(
        knives::jj::conflicted_files(&lab.work, RELEASE).expect("list conflicts"),
        ["shared.txt"]
    );
    lab.jj_work(["edit", RELEASE]);
    std::fs::write(lab.work.join("shared.txt"), RESOLVED).expect("resolve by hand");
    step_off_onto_trunk(&lab);
    adopt_ok(&lab, &home);
    adopted_carrier(lab, home)
}

/// The hand resolution of `shared.txt` [`resolved_carrier_state`] records.
const RESOLVED: &str = "alpha\nbeta\n";

/// The release holds the hand resolution byte for byte, with no conflict.
fn assert_resolution_kept(lab: &Lab, after: &str) {
    assert_eq!(
        file_at_revision(lab, RELEASE, "shared.txt"),
        RESOLVED,
        "after {after}"
    );
    assert_eq!(
        knives::jj::conflicted_files(&lab.work, RELEASE).expect("list conflicts"),
        Vec::<String>::new(),
        "after {after}"
    );
}

/// The working copy sits on `release` and is not stale.
fn assert_working_copy_on(lab: &Lab, release: &CommitId, after: &str) {
    assert_eq!(
        lab.revision(&lab.work, "@-", "commit_id"),
        release.as_str(),
        "after {after}: the working copy moved off the release"
    );
    let opened = Repo::open(&lab.work).expect("open");
    assert_eq!(
        opened.stale_working_copy(&lab.work),
        None,
        "after {after}: the working copy is stale"
    );
}

/// The fork's ledger entries.
fn ledger_entries(home: &tempfile::TempDir) -> Vec<knives::ledger::Entry> {
    knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries
}

/// Every earlier write of [`RELEASE`] this repository holds: what the
/// ledger records under it and the publish remote's commit of it, with
/// every visible rewrite.
fn earlier_writes(lab: &Lab, home: &tempfile::TempDir) -> Vec<CommitId> {
    let opened = Repo::open(&lab.work).expect("open");
    let published = commit_at(lab, "integration@origin");
    knives::release_model::EarlierWrites::read(
        &opened,
        RELEASE,
        &ledger_entries(home),
        Some(&published),
    )
    .expect("read earlier writes")
    .commits()
    .to_vec()
}

#[test]
fn a_fast_forward_carrier_is_labelled_and_left_out_of_the_next_include_and_cut() {
    // Given: a fixed release whose push was made a fast-forward by hand: its
    // pushed first cut became one more parent, the tree put back.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ok(&lab, &home, &["cut"]);
    let carrier = commit_at(&lab, RELEASE);
    lab.push_branch(RELEASE);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    release_ok(&lab, &home, &["include", "feat/gamma"]);
    fast_forward(&lab);

    // When: it is adopted.
    let adopted = knives_release(&lab, &home, &["adopt"]);

    // Then: adopted, the carrier named.
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    assert!(
        stdout(&adopted).contains(&format!("  carrier {}: {LABEL}\n", carrier.short())),
        "{}",
        text(&adopted)
    );
    // And: the audit reads it as knives' own.
    let report = audit_report(&lab, &home);
    assert!(findings(&report, "release-drift").is_empty(), "{report}");
    // And: members and the plan label the carrier, and the plan counts no
    // stacked member.
    let members = release_ok(&lab, &home, &["members"]);
    assert!(
        members.contains(&format!("- {} {LABEL}", carrier.short())),
        "{members}"
    );
    // The adopted release is not pushed yet, the plan's one finding.
    let plan = plan_text(&lab, &home, false);
    assert!(
        plan.contains(&format!("    {}  {LABEL}", carrier.short())),
        "{plan}"
    );
    assert!(!plan.contains("stacked on a prior merge"), "{plan}");
    assert!(plan.contains("4 parent(s), flat"), "{plan}");

    // When: the next include, then the next cut.
    lab.branch("feat/delta", "delta.txt", "delta\n");
    step_off_onto_trunk(&lab);
    release_ok(&lab, &home, &["include", "feat/delta"]);
    assert!(!release_parents(&lab, RELEASE).contains(&carrier));
    // The unpushed include leaves the local release and its pushed position
    // with different trees, which the plan reports as findings; the cut is
    // written all the same.
    let cut = knives_release(&lab, &home, &["cut"]);
    assert!(
        stdout(&cut).contains(&format!("cut {RELEASE} as")),
        "{}",
        text(&cut)
    );

    // Then: the carrier is no parent of the cut, members and the audit name
    // no carrier and find no drift, and the tree holds every member.
    assert!(!release_parents(&lab, RELEASE).contains(&carrier));
    let members = release_ok(&lab, &home, &["members"]);
    assert!(!members.contains(LABEL), "{members}");
    assert!(!members.contains(carrier.short()), "{members}");
    let report = audit_report(&lab, &home);
    assert!(findings(&report, "release-drift").is_empty(), "{report}");
    // The release's own remote-drift names the pushed position, which is the
    // carrier until the cut is pushed: that is the remote ref, not a member.
    let naming: Vec<&serde_json::Value> = report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .filter(|finding| {
            !(finding["kind"] == "remote-drift" && finding["subject"]["branch"] == RELEASE)
                && finding.to_string().contains(carrier.short())
        })
        .collect();
    assert!(naming.is_empty(), "{naming:?}");
    for (file, content) in [
        ("alpha.txt", "alpha\n"),
        ("beta.txt", "beta\n"),
        ("gamma.txt", "gamma\n"),
        ("delta.txt", "delta\n"),
    ] {
        assert_eq!(file_at_revision(&lab, RELEASE, file), content, "{file}");
    }
}

#[test]
fn an_extra_parent_beside_a_carrier_is_refused_as_a_membership_change_outside_knives() {
    // Given: the fast-forward shape with one more parent, a branch no write
    // of the release ever had: its members match no recorded write.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ok(&lab, &home, &["cut"]);
    let carrier = commit_at(&lab, RELEASE);
    lab.push_branch(RELEASE);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    release_ok(&lab, &home, &["include", "feat/gamma"]);
    lab.branch("feat/stranger", "stranger.txt", "stranger\n");
    step_off_onto_trunk(&lab);
    lab.jj_work([
        "rebase",
        "-r",
        RELEASE,
        "-d",
        "parents(integration) | integration@origin | feat/stranger",
    ]);
    let before = ledger_entries(&home);

    // When: it is adopted.
    let output = knives_release(&lab, &home, &["adopt"]);

    // Then: refused as a membership change made outside knives, which belongs
    // in the verbs that record a composition; the carrier is named nowhere.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        out.contains(&format!(
            "demo: {RELEASE}'s members differ from every recorded write; make the change with \
             `knives release include`, `drop` or `advance` (a member added by hand: drop it, then \
             include it); that write records the new composition, marked unchecked\n"
        )),
        "{out}"
    );
    assert!(!out.contains(carrier.short()), "{out}");
    assert_eq!(ledger_entries(&home), before);
}

#[test]
fn a_drop_names_a_loss_only_when_the_written_tree_lost_something() {
    // Given: a fixed release of alpha, a twin of alpha writing the same file
    // the same way, and beta.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/alpha-twin", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ok(&lab, &home, &["cut"]);

    // When: the twin is dropped, whose content alpha also holds.
    let twin = release_ok(&lab, &home, &["drop", "feat/alpha-twin", "--why", "a twin"]);

    // Then: the drop says the content did not change, and names no loss.
    assert!(
        twin.contains(
            "demo: feat/alpha-twin left the parent set of integration, and the release's \
             content did not change\n"
        ),
        "{twin}"
    );
    assert!(!twin.contains("loses it"), "{twin}");
    assert_eq!(file_at_revision(&lab, RELEASE, "alpha.txt"), "alpha\n");

    // When: beta is dropped, whose content nothing else holds.
    let beta = release_ok(&lab, &home, &["drop", "feat/beta", "--why", "not ready"]);

    // Then: the drop still names the loss.
    assert!(
        beta.contains(
            "demo: no remaining member carries feat/beta's content; the release loses it\n"
        ),
        "{beta}"
    );
    assert!(!beta.contains("did not change"), "{beta}");
}

#[test]
fn a_carrier_dropped_by_its_id_leaves_the_parent_set_with_the_content_unchanged() {
    // Given: the adopted carrier state.
    let state = carrier_state(&[
        ("feat/alpha", "alpha.txt", "alpha\n"),
        ("feat/beta", "beta.txt", "beta\n"),
    ]);
    let (lab, home) = (&state.lab, &state.home);

    // When: the carrier is dropped by its commit id.
    let dropped = release_ok(
        lab,
        home,
        &["drop", state.carrier.as_str(), "--why", "an earlier write"],
    );

    // Then: it is no parent, and the drop says the content did not change.
    assert!(!release_parents(lab, RELEASE).contains(&state.carrier));
    assert!(
        dropped.contains("and the release's content did not change"),
        "{dropped}"
    );
    assert!(!dropped.contains("loses it"), "{dropped}");
    let opened = Repo::open(&lab.work).expect("open");
    assert!(
        opened
            .same_tree(&state.release, &commit_at(lab, RELEASE))
            .expect("compare trees")
    );
}

#[test]
fn an_include_over_a_carrier_keeps_the_hand_resolution_and_the_working_copy() {
    // Given: the adopted carrier state whose carrier holds a hand
    // resolution, a working copy left on the release.
    let state = resolved_carrier_state();
    let (lab, home) = (&state.lab, &state.home);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    lab.jj_work(["new", RELEASE]);

    // When: a branch is included.
    release_ok(lab, home, &["include", "feat/delta"]);

    // Then: the resolution is kept, no carrier is a parent, the working copy
    // still sits on the release it was on, and nothing is an anonymous head.
    assert_resolution_kept(lab, "include");
    assert!(!release_parents(lab, RELEASE).contains(&state.carrier));
    assert_working_copy_on(lab, &state.release, "include");
    assert_no_anonymous_head(lab, home, "include");
}

#[test]
fn a_drop_over_a_carrier_keeps_the_hand_resolution_and_the_working_copy() {
    // Given: the adopted carrier state with a hand resolution, a working
    // copy left on the release.
    let state = resolved_carrier_state();
    let (lab, home) = (&state.lab, &state.home);
    lab.jj_work(["new", RELEASE]);

    // When: gamma, a member outside the resolved conflict, is dropped.
    release_ok(lab, home, &["drop", "feat/gamma", "--why", "not ready"]);

    // Then: the resolution is kept and nothing else moved.
    assert_resolution_kept(lab, "drop");
    assert!(!release_parents(lab, RELEASE).contains(&state.carrier));
    assert_working_copy_on(lab, &state.release, "drop");
    assert_no_anonymous_head(lab, home, "drop");
}

#[test]
fn an_advance_over_a_carrier_keeps_the_hand_resolution_and_the_working_copy() {
    // Given: the adopted carrier state with a hand resolution, alpha grown,
    // a working copy left on the release.
    let state = resolved_carrier_state();
    let (lab, home) = (&state.lab, &state.home);
    extend_branch(lab, "feat/alpha", "more.txt", "more\n");
    lab.jj_work(["new", RELEASE]);

    // When: the release advances.
    let advanced = release_ok(lab, home, &["advance"]);

    // Then: alpha moved and the resolution is kept.
    assert!(advanced.contains("advanced feat/alpha"), "{advanced}");
    assert_resolution_kept(lab, "advance");
    assert!(!release_parents(lab, RELEASE).contains(&state.carrier));
    assert_working_copy_on(lab, &state.release, "advance");
    assert_no_anonymous_head(lab, home, "advance");
}

#[test]
fn a_cut_over_a_published_carrier_records_it_and_keeps_the_hand_resolution_and_the_working_copy() {
    // Given: the adopted carrier state with a hand resolution, pushed, a
    // working copy left on the release.
    let state = resolved_carrier_state();
    let (lab, home) = (&state.lab, &state.home);
    lab.jj_work(["new", RELEASE]);

    // When: the fixed release is cut again.
    let cut = release_ok(lab, home, &["cut"]);

    // Then: the cut records the published release, carrier and all, and
    // makes no commit: a cut of a fixed branch records what is published.
    // The resolution holds both members' lines, but each member adds
    // `shared.txt`, which the resolution changed: the audit cannot read it,
    // and carries it forward from the newest recorded write, which held the
    // same resolution.
    assert!(
        cut.contains(&format!(
            "demo: recorded published composition {RELEASE}@origin ({}) as the cut",
            state.release.short()
        )),
        "{cut}"
    );
    for member in ["feat/alpha", "feat/beta"] {
        assert!(
            cut.contains(&format!(
                "{member}: diverges where the newest recorded write already does; carried forward"
            )),
            "{cut}"
        );
    }
    assert_eq!(commit_at(lab, RELEASE), state.release, "{cut}");
    assert_resolution_kept(lab, "cut");
    assert!(release_parents(lab, RELEASE).contains(&state.carrier));
    assert_working_copy_on(lab, &state.release, "cut");
    assert_no_anonymous_head(lab, home, "cut");
}

#[test]
fn a_rebase_over_a_carrier_keeps_the_hand_resolution_and_moves_the_working_copy_with_it() {
    // Given: the adopted carrier state with a hand resolution, a working
    // copy left on the release, and the upstream trunk moved on.
    let state = resolved_carrier_state();
    let (lab, home) = (&state.lab, &state.home);
    lab.jj_work(["new", RELEASE]);
    lab.advance_upstream("upstream moved\n");

    // When: the release is rebased onto it.
    release_ok(lab, home, &["rebase", "main@upstream"]);

    // Then: the resolution is kept, and the working copy followed the
    // release's rewrite, the commit with its change id, and is not stale.
    assert_resolution_kept(lab, "rebase");
    assert!(!release_parents(lab, RELEASE).contains(&state.carrier));
    assert_eq!(
        lab.revision(&lab.work, "@-", "change_id"),
        change_of(lab, &state.release).as_str()
    );
    let opened = Repo::open(&lab.work).expect("open");
    assert_eq!(opened.stale_working_copy(&lab.work), None);
    assert_no_anonymous_head(lab, home, "rebase");
}

#[test]
fn a_rebase_over_a_carrier_with_no_working_copy_on_it_leaves_no_anonymous_head() {
    // Given: the adopted carrier state with a hand resolution, the working
    // copy sitting on the trunk, and the upstream trunk moved on.
    let state = resolved_carrier_state();
    let (lab, home) = (&state.lab, &state.home);
    lab.advance_upstream("upstream moved\n");

    // When: the release is rebased onto it.
    release_ok(lab, home, &["rebase", "main@upstream"]);

    // Then: the carrier is hidden with the release it carried, and the
    // resolution is kept.
    assert_resolution_kept(lab, "rebase");
    assert!(!is_visible(lab, &state.carrier));
    assert_no_anonymous_head(lab, home, "rebase");
}

#[test]
fn an_include_then_a_rebase_before_any_push_leaves_no_anonymous_head() {
    // Given: the adopted carrier state with a hand resolution.
    let state = resolved_carrier_state();
    let (lab, home) = (&state.lab, &state.home);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    step_off_onto_trunk(lab);
    lab.advance_upstream("upstream moved\n");

    // When: a branch is included, then the release rebased, nothing pushed.
    release_ok(lab, home, &["include", "feat/delta"]);
    release_ok(lab, home, &["rebase", "main@upstream"]);

    // Then: nothing is an anonymous head, and the resolution is kept.
    assert_resolution_kept(lab, "include and rebase");
    assert_no_anonymous_head(lab, home, "include and rebase");
}

#[test]
fn an_include_then_a_plain_push_leaves_no_anonymous_head() {
    // Given: the adopted carrier state with a hand resolution.
    let state = resolved_carrier_state();
    let (lab, home) = (&state.lab, &state.home);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    step_off_onto_trunk(lab);

    // When: a branch is included and the release pushed.
    release_ok(lab, home, &["include", "feat/delta"]);
    lab.push_branch(RELEASE);

    // Then: nothing is an anonymous head.
    assert_resolution_kept(lab, "include and push");
    assert_no_anonymous_head(lab, home, "include and push");
}

/// Every bookmark, local and remote, the tags and the working copy, other
/// than the release's own refs, each with what it names: what a write must
/// leave where it was.
fn refs(lab: &Lab) -> BTreeMap<String, String> {
    let opened = Repo::open(&lab.work).expect("open");
    let mut refs: BTreeMap<String, String> = opened
        .bookmark_tips()
        .expect("read bookmarks")
        .into_iter()
        .filter(|(reference, _)| reference.branch().as_str() != RELEASE)
        .map(|(reference, commit)| (reference.to_string(), commit.as_str().to_owned()))
        .collect();
    refs.insert("@".to_owned(), lab.revision(&lab.work, "@", "commit_id"));
    refs.insert("tags".to_owned(), lab.jj_work_output(["tag", "list"]));
    refs
}

/// The plan's text. Once the release in hand is `pushed`, the plan must exit
/// 0 with no finding; before, it must exit 1 with one finding: the release
/// names its local and pushed positions, whose trees differ.
fn plan_text(lab: &Lab, home: &tempfile::TempDir, pushed: bool) -> String {
    let output = knives_release(lab, home, &[]);
    let plan = stdout(&output);
    let findings: Vec<&str> = plan
        .lines()
        .filter(|line| line.trim_start().starts_with("!!"))
        .collect();
    let unpushed = format!("{RELEASE} names both");
    if pushed {
        assert_eq!(output.status.code(), Some(0), "{}", text(&output));
        assert!(findings.is_empty(), "{}", text(&output));
    } else {
        assert_eq!(output.status.code(), Some(1), "{}", text(&output));
        assert!(
            matches!(findings.as_slice(), [only] if only.contains(&unpushed)),
            "{}",
            text(&output)
        );
    }
    plan
}

/// Two rounds of a knives write, a fast-forward made by hand, a push and an
/// adopt: the release holds the previous round's release as its carrier,
/// which holds the first cut as its own. `reword` rewrites alpha's commit
/// between the rounds.
fn depth_two_chain(reword: bool) -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let home = fixed_home(&lab);
    release_ok(&lab, &home, &["cut"]);
    lab.push_branch(RELEASE);
    for (round, branch) in ["feat/gamma", "feat/delta"].into_iter().enumerate() {
        if reword && round == 1 {
            lab.jj_work(["describe", "feat/alpha", "-m", "feat/alpha reworded"]);
        }
        lab.branch(branch, &format!("{}.txt", &branch[5..]), "round\n");
        release_ok(&lab, &home, &["include", branch]);
        fast_forward(&lab);
        lab.push_branch(RELEASE);
        adopt_ok(&lab, &home);
    }
    (lab, home)
}

/// What a write over the depth-two chain must leave: no anonymous head; a
/// plan that reads `parents` parent(s), flat, with no stacked line, and that
/// exits 0 once the write is `pushed`, or else 1 with one finding, the
/// release's local and pushed positions holding different trees; and no
/// parent sharing a change id with an earlier write of the name.
fn assert_chain_written_clean(
    lab: &Lab,
    home: &tempfile::TempDir,
    (after, parents, pushed): (&str, usize, bool),
) {
    assert_no_anonymous_head(lab, home, after);
    let plan = plan_text(lab, home, pushed);
    assert!(
        plan.contains(&format!("{parents} parent(s), flat")),
        "after {after}: {plan}"
    );
    assert!(!plan.contains("stacked on a prior merge"), "{plan}");
    let opened = Repo::open(&lab.work).expect("open");
    let written: Vec<ChangeId> =
        knives::release_model::recorded_writes(&ledger_entries(home), RELEASE)
            .filter_map(|write| opened.change_id_of(write.commit.as_str()).ok())
            .collect();
    for parent in release_parents(lab, RELEASE) {
        assert!(
            !written.contains(&change_of(lab, &parent)),
            "after {after}: parent {} shares a change id with an earlier write",
            parent.short()
        );
    }
}

/// Case A of the depth-two chain: a knives write and a plain push.
fn chain_then_write_and_push(reword: bool) {
    // Given: the depth-two chain, then one more member.
    let (lab, home) = depth_two_chain(reword);
    lab.branch("feat/epsilon", "epsilon.txt", "epsilon\n");
    let before = refs(&lab);

    // When: a knives write, then a plain push.
    release_ok(&lab, &home, &["include", "feat/epsilon"]);
    lab.push_branch(RELEASE);

    // Then: nothing is an anonymous head, the plan exits 0 counting five
    // flat parents and no stacked member, no parent is an earlier write, and
    // every other ref is where it was.
    assert_chain_written_clean(&lab, &home, ("the write and push", 5, true));
    assert_eq!(refs(&lab), before);
}

#[test]
fn a_depth_two_carrier_chain_written_and_pushed_leaves_no_anonymous_head() {
    chain_then_write_and_push(false);
}

#[test]
fn a_depth_two_carrier_chain_with_a_reworded_member_leaves_no_anonymous_head() {
    chain_then_write_and_push(true);
}

#[test]
fn a_depth_two_carrier_chain_rebased_onto_a_moved_trunk_leaves_no_anonymous_head() {
    // Given: the depth-two chain, the working copy the branch helper left as
    // an empty child of delta, and the upstream trunk moved on.
    let (lab, home) = depth_two_chain(false);
    lab.advance_upstream("upstream moved\n");
    let before = refs(&lab);
    let change = |commit: &str| change_of(&lab, &CommitId::new(commit));
    let working_copy = change(&before["@"]);
    let delta = change(&before["feat/delta"]);
    let members: Vec<(String, ChangeId)> = before
        .iter()
        .filter(|(name, _)| name.starts_with("feat/") && !name.contains('@'))
        .map(|(name, commit)| (name.clone(), change(commit)))
        .collect();

    // When: the release is rebased onto it before any push.
    release_ok(&lab, &home, &["rebase", "main@upstream"]);

    // Then: nothing is an anonymous head, the plan counts four flat parents
    // and finds only the unpushed rebase, no parent is an earlier write, each
    // member's bookmark moved onto its rebased commit, the working copy
    // followed delta keeping its change id, and every other ref stayed.
    assert_chain_written_clean(&lab, &home, ("the rebase", 4, false));
    let after = refs(&lab);
    for (name, member) in &members {
        assert_eq!(&change(&after[name]), member, "{name}");
    }
    assert_eq!(change(&after["@"]), working_copy);
    assert_eq!(
        lab.revision(&lab.work, "@-", "change_id"),
        delta.as_str(),
        "the working copy follows delta onto its rebased commit"
    );
    // A member's bookmark moves with it, and so does the colocated git view
    // of it when there is one.
    let moved = |name: &str| {
        name == "@"
            || (name.starts_with("feat/") && (!name.contains('@') || name.ends_with("@git")))
    };
    let unmoved = |refs: &BTreeMap<String, String>| -> BTreeMap<String, String> {
        refs.iter()
            .filter(|(name, _)| !moved(name))
            .map(|(name, commit)| (name.clone(), commit.clone()))
            .collect()
    };
    assert_eq!(unmoved(&after), unmoved(&before));
}

#[test]
fn a_member_with_no_bookmark_dropped_from_an_unpushed_release_stays_visible() {
    // Given: a fixed release never pushed, one of whose members lost its
    // bookmark after the cut.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/m", "m.txt", "m\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ok(&lab, &home, &["cut"]);
    let member = commit_at(&lab, "feat/m");
    lab.jj_work(["bookmark", "delete", "feat/m"]);

    // When: it is dropped by its commit id.
    release_ok(&lab, &home, &["drop", member.as_str(), "--why", "unwanted"]);

    // Then: it stays visible, and the audit reports it as an orphan commit.
    assert!(is_visible(&lab, &member));
    let heads = anonymous_heads(&lab, &home);
    assert!(
        heads
            .iter()
            .any(|head| head.contains(member.short()) && head.contains("anonymous head: feat/m")),
        "{heads:?}"
    );
}

#[test]
fn a_conflicted_write_over_a_carrier_abandons_no_earlier_write() {
    // Given: the adopted carrier state, and a branch writing alpha's file
    // its own way. Every earlier write other than the release in hand is
    // noted while visible.
    let state = carrier_state(&[
        ("feat/alpha", "alpha.txt", "alpha\n"),
        ("feat/beta", "beta.txt", "beta\n"),
    ]);
    let (lab, home) = (&state.lab, &state.home);
    lab.branch("feat/clash", "alpha.txt", "clash\n");
    step_off_onto_trunk(lab);
    let visible: Vec<CommitId> = earlier_writes(lab, home)
        .into_iter()
        .filter(|commit| *commit != state.release && is_visible(lab, commit))
        .collect();
    assert!(visible.contains(&state.carrier), "{visible:?}");

    // When: it is included, which leaves the release with a conflict.
    release_ok(lab, home, &["include", "feat/clash"]);
    assert_eq!(
        knives::jj::conflicted_files(&lab.work, RELEASE).expect("list conflicts"),
        ["alpha.txt"]
    );

    // Then: each of them, the carrier among them, stays visible.
    for commit in &visible {
        assert!(is_visible(lab, commit), "{} was hidden", commit.short());
    }
}

#[test]
fn a_bookmarkless_member_dropped_over_a_carrier_stays_while_a_superseded_one_goes() {
    // Given: a fixed release of alpha, a member n that lost its bookmark and
    // a member s, pushed; then s reworded, which rewrites the release too,
    // gamma included, and the push made a fast-forward by hand, which brings
    // the pushed release back and with it s's older version; adopted and
    // pushed.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/n", "n.txt", "n\n");
    lab.branch("feat/s", "s.txt", "s\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ok(&lab, &home, &["cut"]);
    let carrier = commit_at(&lab, RELEASE);
    let member = commit_at(&lab, "feat/n");
    let older = commit_at(&lab, "feat/s");
    lab.jj_work(["bookmark", "delete", "feat/n"]);
    lab.push_branch(RELEASE);
    lab.jj_work(["describe", "feat/s", "-m", "feat/s reworded"]);
    assert!(
        !is_visible(&lab, &older),
        "the describe hid s's older version"
    );
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    release_ok(&lab, &home, &["include", "feat/gamma"]);
    fast_forward(&lab);
    assert!(is_visible(&lab, &older), "the fast-forward brings it back");
    adopt_ok(&lab, &home);
    lab.push_branch(RELEASE);

    // When: n is dropped by its commit id.
    release_ok(&lab, &home, &["drop", member.as_str(), "--why", "unwanted"]);

    // Then: the carrier and s's older version, which s's rewrite
    // supersedes, are hidden; n stays.
    assert!(!is_visible(&lab, &carrier));
    assert!(!is_visible(&lab, &older), "the superseded member was kept");
    assert!(is_visible(&lab, &member));

    // When: the next push and the next knives write.
    lab.push_branch(RELEASE);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    step_off_onto_trunk(&lab);
    release_ok(&lab, &home, &["include", "feat/delta"]);

    // Then: n is still visible, and the audit reports it as an orphan commit.
    assert!(is_visible(&lab, &member));
    let heads = anonymous_heads(&lab, &home);
    assert!(
        heads
            .iter()
            .any(|head| head.contains(member.short()) && head.contains("anonymous head: feat/n")),
        "{heads:?}"
    );
    assert_eq!(heads.len(), 1, "{heads:?}");
}

#[test]
fn a_member_reworded_under_a_tagged_release_stays_when_its_rewrite_is_dropped() {
    // Given: a fixed release of alpha and a member m with no bookmark,
    // pushed and tagged; then m reworded, which rewrites the tagged release
    // too, so both are hidden; then the fast-forward made by hand, which
    // brings the tagged release, and m under it, back.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/m", "m.txt", "m\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ok(&lab, &home, &["cut"]);
    let member = commit_at(&lab, "feat/m");
    lab.jj_work(["bookmark", "delete", "feat/m"]);
    lab.push_branch(RELEASE);
    lab.jj_work(["tag", "set", "v1", "-r", RELEASE]);
    lab.jj_work([
        "describe",
        "--ignore-immutable",
        "-r",
        member.as_str(),
        "-m",
        "feat/m reworded",
    ]);
    let reworded = Repo::open(&lab.work)
        .expect("open")
        .rewrites_of(member.as_str())
        .expect("read the rewrite")
        .into_iter()
        .next()
        .expect("the describe rewrote m");
    assert!(!is_visible(&lab, &member), "the describe hid m");
    fast_forward(&lab);
    assert!(is_visible(&lab, &member), "the fast-forward brings m back");
    // Control: a rule that reads a shared change id in either direction
    // (`Repo::rewrites_of`) names the visible m as a rewrite of the reworded
    // m, so it would take the reworded m for superseded. Only the walk from
    // the newer version back tells which one rewrote the other.
    assert_eq!(
        Repo::open(&lab.work)
            .expect("open")
            .rewrites_of(reworded.as_str())
            .expect("read the change"),
        vec![member.clone()],
        "the reworded m and m share a change id, both visible"
    );

    // When: the reworded m is dropped by its id.
    release_ok(
        &lab,
        &home,
        &["drop", reworded.as_str(), "--why", "unwanted"],
    );

    // Then: it stays visible, and the audit reports it: no visible commit
    // rewrote it, though its older version is visible.
    assert!(is_visible(&lab, &reworded));
    assert!(is_visible(&lab, &member));
    let heads = anonymous_heads(&lab, &home);
    assert!(
        heads.iter().any(|head| head.contains(reworded.short())
            && head.contains("anonymous head: feat/m reworded")),
        "{heads:?}"
    );
}
