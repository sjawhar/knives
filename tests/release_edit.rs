//! `knives release include` and `drop`: one parent joins or leaves, and nothing else changes.
//!
//! Each edit is one operation with its ledger event and carries the repository
//! identity. A carried branch is a reported no-op, the trunk is never a member,
//! a drop needs its why, a drop resolves an advanced branch's parent by
//! ancestry, and content that survives through another member stays quiet.
//! Refused before any cut, when the upstream trunk cannot resolve, when the
//! release is held only as a remote ref, or when every pin of this release is
//! frozen — a pin frozen on an older release does not count.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::jj::Repo;
use lab::{
    Lab, commit_at, extend_branch, file_at_revision, home_after_first_cut, knives_release,
    newest_operation_description, operation_ids, release_parents, release_test_home,
    release_test_home_pinned,
};

#[test]
fn include_adds_one_parent_and_changes_nothing_else() {
    // Given: a cut made before feat/gamma existed. Including gamma is one new
    // parent; every other parent stays at the commit the release already has.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let before = release_parents(&lab, "release/2026-08-04");

    // When: the branch is included.
    let output = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(
        output.status.success(),
        "include failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: exactly one parent was added and none moved.
    let after = release_parents(&lab, "release/2026-08-04");
    assert_eq!(after.len(), before.len() + 1, "{before:?} -> {after:?}");
    for parent in &before {
        assert!(
            after.contains(parent),
            "an existing parent moved: {before:?} -> {after:?}"
        );
    }
    let gamma = commit_at(&lab, "feat/gamma");
    assert!(after.contains(&gamma), "gamma tip missing: {after:?}");
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "gamma.txt"),
        "gamma\n"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!(
            "now has {} parent(s): included feat/gamma",
            after.len()
        )),
        "the reported parent count and delta must match the release: {stdout}"
    );
}

#[test]
fn a_release_edit_is_one_operation_described_for_the_op_log() {
    // Given: a cut release and a new branch to include. An include used to be
    // three operations (duplicate, describe, bookmark set), each described as
    // raw `args: jj ...` — hard to audit, and three reconciliation points with
    // concurrent agents (#18).
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let operations_before = operation_ids(&lab.work);

    // When: the branch is included.
    let output = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(
        output.status.success(),
        "include failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the edit is ONE operation, described as knives' own act.
    let operations_after = operation_ids(&lab.work);
    assert_eq!(
        operations_after.len(),
        operations_before.len() + 1,
        "an edit must be one operation"
    );
    let description = newest_operation_description(&lab.work);
    assert_eq!(
        description, "knives: release/2026-08-04: included feat/gamma",
        "the operation must describe the verb, not the plumbing"
    );
}

#[test]
fn an_edited_release_carries_the_repository_identity() {
    // Given: identity configured only in the repository's own jj config, the
    // way every lab and managed checkout carries it. A release merge written
    // with an empty author cannot be pushed by jj later, so the library-side
    // writer must resolve identity the way the jj CLI does.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");

    // When: the release is edited.
    let output = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(output.status.success(), "{output:?}");

    // Then: the new release commit is authored, not anonymous.
    assert_eq!(
        lab.revision(&lab.work, "release/2026-08-04", "author.email()"),
        "knives-lab@example.test"
    );
    assert_eq!(
        lab.revision(&lab.work, "release/2026-08-04", "committer.name()"),
        "Knives Lab"
    );
}

#[test]
fn including_a_carried_branch_is_a_reported_noop() {
    // Including a parent the release already has changes nothing at all — not
    // the parent set, and not the release commit either. A no-op that still
    // duplicated the release would churn its identity under every consumer.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let before = release_parents(&lab, "release/2026-08-04");
    let before_commit = commit_at(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("already carries feat/alpha"), "{stdout}");
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);
    assert_eq!(
        commit_at(&lab, "release/2026-08-04"),
        before_commit,
        "a reported no-op rewrote the release"
    );
}

#[test]
fn include_refuses_to_advance_an_advanced_branch() {
    // Given: a released branch that has advanced. Moving a member to its tip is
    // a content change beyond "include this", so it only happens when asked
    // for by name: `advance`.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    let before = release_parents(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("the branch has moved on")
            && stdout.contains("knives release advance feat/alpha"),
        "the refusal must name the verb that does move a member: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, "release/2026-08-04"),
        before,
        "include must not move a member"
    );
}

#[test]
fn drop_removes_one_parent_and_records_why() {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha tip");
    let beta = repo.resolve_commit("feat/beta").expect("beta tip");
    let before = release_parents(&lab, "release/2026-08-04");

    let output = knives_release(
        &lab,
        &home,
        &["drop", "feat/beta", "--why", "beta is not ready"],
    );
    assert!(
        output.status.success(),
        "drop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("dropped feat/beta: beta is not ready"),
        "the reported delta must carry the stated reason: {stdout}"
    );

    // Then: only beta's parent left; the reason is on the release itself; the
    // branch bookmark still holds the dropped work.
    let after = release_parents(&lab, "release/2026-08-04");
    assert_eq!(after.len(), before.len() - 1, "{before:?} -> {after:?}");
    assert!(!after.contains(&beta), "beta parent survived: {after:?}");
    assert!(after.contains(&alpha), "alpha parent vanished: {after:?}");
    let description = Repo::open(&lab.work)
        .expect("reopen")
        .description_of("release/2026-08-04")
        .expect("release description");
    assert!(description.contains("beta is not ready"), "{description}");
    let tips = Repo::open(&lab.work)
        .expect("reopen for tips")
        .bookmark_tips()
        .expect("tips");
    assert!(
        tips.keys()
            .any(|reference| reference.branch().as_str() == "feat/beta"),
        "dropping a member must not touch its bookmark"
    );
}

#[test]
fn drop_resolves_an_advanced_branchs_parent_by_ancestry() {
    // Given: the release carries an old tip of feat/beta and the bookmark has
    // moved on. The name still resolves: the member parent is the one the
    // branch tip descends from.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let released_beta = commit_at(&lab, "feat/beta");
    extend_branch(&lab, "feat/beta", "beta.txt", "beta\nmore\n");
    let repo = Repo::open(&lab.work).expect("reopen after beta advanced");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha tip");
    let before = release_parents(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["drop", "feat/beta", "--why", "superseded"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let after = release_parents(&lab, "release/2026-08-04");
    assert!(
        !after.contains(&released_beta),
        "the ancestor parent survived: {after:?}"
    );
    assert_eq!(after.len(), before.len() - 1, "{before:?} -> {after:?}");
    assert!(
        after.contains(&alpha),
        "the drop removed more than the named member: {after:?}"
    );
}

#[test]
fn an_edit_refuses_when_the_upstream_trunk_cannot_resolve() {
    // Given: a registry whose base names a branch upstream does not have.
    // Edits classify parents against the trunk; guessing with no trunk would
    // let a drop or advance touch the base, which is rebase's domain.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nbase = \"missing\"\n",
            lab.work.display(),
            lab.upstream.display(),
        ),
    )
    .expect("write broken-trunk registry");
    let consumer = tempfile::tempdir().expect("create local consumer");
    std::fs::write(
        home.path().join("local-consumer"),
        consumer.path().display().to_string(),
    )
    .expect("write local consumer fixture path");
    let before = release_parents(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("cannot resolve")
            && stdout.contains("release edits classify parents against the upstream trunk"),
        "the refusal must name the missing trunk as its reason: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, "release/2026-08-04"),
        before,
        "an edit ran without a resolvable trunk"
    );
}

#[test]
fn a_drop_without_a_why_is_a_usage_error() {
    // Dropping shipped content without a reason is how a release becomes
    // unexplainable later; the parser refuses rather than defaulting one in.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    let output = knives_release(&lab, &home, &["drop", "feat/alpha"]);

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--why"),
        "{output:?}"
    );
}

#[test]
fn include_by_commit_id_adds_that_exact_parent() {
    // A commit that no bookmark names is still includable by id.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/loose", "loose.txt", "loose\n");
    let loose = commit_at(&lab, "feat/loose");
    lab.jj_work(["bookmark", "forget", "feat/loose"]);
    let before = release_parents(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", loose.as_str()]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let after = release_parents(&lab, "release/2026-08-04");
    assert!(
        after.contains(&loose),
        "the raw commit id was not added as a parent: {after:?}"
    );
    assert_eq!(after.len(), before.len() + 1, "{before:?} -> {after:?}");
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "loose.txt"),
        "loose\n"
    );
}

#[test]
fn include_of_content_reachable_through_another_parent_reports_the_carrier() {
    // Given: beta stacked on alpha, alpha's own parent dropped. Alpha's content
    // still ships through beta's history, but membership is the parent set, so
    // include says which situation holds instead of pretending it happened.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["new", "feat/alpha", "-m", "stacked work"]);
    std::fs::write(lab.work.join("beta.txt"), "beta\n").expect("write beta");
    lab.jj_work(["bookmark", "create", "feat/beta", "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = home_after_first_cut(&lab);
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/alpha", "--why", "beta carries it"],
    );
    assert!(dropped.status.success(), "{dropped:?}");
    let before = release_parents(&lab, "release/2026-08-04");
    let before_commit = commit_at(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("through another parent's history"),
        "{stdout}"
    );
    assert_eq!(
        release_parents(&lab, "release/2026-08-04"),
        before,
        "include mutated the release for content it does not carry"
    );
    assert_eq!(
        commit_at(&lab, "release/2026-08-04"),
        before_commit,
        "a reported non-include rewrote the release"
    );
}

#[test]
fn a_member_landed_upstream_by_merge_commit_can_still_be_dropped() {
    // Given: a released member whose pull merged upstream WITH A MERGE COMMIT,
    // so its tip is now reachable from the trunk. Every parent is a member —
    // the base is never one — so landing must not dead-end the post-merge
    // `drop`, and the drop must say the release itself no longer carries it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let alpha = commit_at(&lab, "feat/alpha");
    let beta = commit_at(&lab, "feat/beta");
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    assert!(
        Repo::open(&lab.work)
            .expect("open after merge")
            .is_ancestor(&alpha, &commit_at(&lab, "main@upstream"))
            .expect("ancestry answerable"),
        "fixture must land alpha in the trunk by merge commit"
    );

    // When: the landed member is dropped.
    let output = knives_release(
        &lab,
        &home,
        &["drop", "feat/alpha", "--why", "merged upstream"],
    );

    // Then: it leaves; the other member stays; and because no remaining member
    // reaches alpha's content, the loss is stated.
    assert!(
        output.status.success(),
        "dropping a landed member dead-ended: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no remaining member carries feat/alpha's content"),
        "{stdout}"
    );
    let parents = release_parents(&lab, "release/2026-08-04");
    assert!(
        !parents.contains(&alpha),
        "landed alpha survived: {parents:?}"
    );
    assert_eq!(parents, vec![beta], "only beta expected: {parents:?}");
}

#[test]
fn a_drop_whose_content_survives_through_another_member_stays_quiet() {
    // Given: beta stacked on alpha, both members. Dropping alpha loses nothing:
    // beta's ancestry still carries it, and saying otherwise would train people
    // to ignore the loss warning.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["new", "feat/alpha", "-m", "stacked work"]);
    std::fs::write(lab.work.join("beta.txt"), "beta\n").expect("write beta");
    lab.jj_work(["bookmark", "create", "feat/beta", "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");

    let output = knives_release(
        &lab,
        &home,
        &["drop", "feat/alpha", "--why", "beta carries it"],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        !stdout.contains("loses it"),
        "content survives through beta; no loss to report: {stdout}"
    );
    let parents = release_parents(&lab, "release/2026-08-04");
    assert_eq!(parents, vec![commit_at(&lab, "feat/beta")], "{parents:?}");
}

#[test]
fn an_edit_before_any_cut_says_to_cut_one_first() {
    // Given: branches and no release at all. Membership is a release's parent
    // set, so there is nothing to edit yet, and an include that invented a
    // release would ship a composition that never passed the cut's gates.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("no release to edit; cut one first"),
        "{stdout}"
    );
    let tips = Repo::open(&lab.work)
        .expect("open after the refusal")
        .bookmark_tips()
        .expect("read bookmark tips");
    assert!(
        !tips
            .keys()
            .any(|reference| reference.branch().as_str().starts_with("release/")),
        "an edit invented a release: {tips:?}"
    );
}

#[test]
fn an_edit_refuses_when_every_pin_of_the_release_is_frozen() {
    // Given: a dated release whose only consumer pins it by revision. Editing
    // it in place reaches nobody, exactly as a rebase would not, so the edit is
    // refused in favour of the one remedy that reaches consumers: a new dated
    // cut. Editing anyway would report a change nothing consumes.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let (home, _consumer) = release_test_home_pinned(
        &lab,
        "rev = \"release/2026-08-03\"",
        "rev = \"release/2026-08-04\"",
    );
    let before = release_parents(&lab, release);

    let output = knives_release(&lab, &home, &["include", "feat/gamma"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("frozen") && stdout.contains("cut a new dated release"),
        "frozen-pin guidance missing: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, release),
        before,
        "a release no pin follows was edited in place"
    );
}

#[test]
fn a_pin_frozen_on_an_older_release_does_not_refuse_editing_the_release_in_hand() {
    // Given: the consumer sits frozen on release/2026-08-03 - the pin is the
    // older cut's, not the release in hand's. Editing release/2026-08-04 reaches
    // that consumer neither way, so it must not block the edit: judged over every
    // pin, one frozen consumer made every include, drop and advance refuse - on
    // the pushed release and on a brand-new unpinned cut alike.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let (home, _consumer) = release_test_home_pinned(
        &lab,
        "rev = \"release/2026-08-02\"",
        "rev = \"release/2026-08-03\"",
    );
    let before = release_parents(&lab, release).len();

    let output = knives_release(&lab, &home, &["include", "feat/gamma"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "an edit was refused for a pin of another release: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("included feat/gamma"), "{stdout}");
    assert_eq!(release_parents(&lab, release).len(), before + 1);
}

#[test]
fn include_refuses_the_trunk_because_it_is_never_a_member() {
    // Given: a cut, and an upstream trunk that has moved past it. A release is
    // a flat merge of feature and fix branches; upstream enters through the
    // members' bases, never as a parent.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.advance_upstream("upstream advance\n");
    let before = release_parents(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "main@upstream"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("not a feature or fix branch"),
        "the refusal must state the model: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, "release/2026-08-04"),
        before,
        "including the trunk made it a parent"
    );
}

#[test]
fn an_edit_refuses_a_release_held_only_as_a_remote_ref() {
    // Given: a cut pushed to origin whose local bookmark is gone — the state a
    // fetch of somebody else's cut leaves, because jj creates no local bookmark
    // for an untracked remote one. An edit moves a local bookmark, and jj
    // rejects `name@remote` as a bookmark name, so without a gate the duplicate
    // is made and described before the move fails.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.push_branch("release/2026-08-04");
    lab.jj_work(["bookmark", "forget", "release/2026-08-04"]);
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let before = release_parents(&lab, "release/2026-08-04@origin");

    let output = knives_release(&lab, &home, &["include", "feat/beta"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("jj bookmark track release/2026-08-04@origin"),
        "the refusal must say how to get a local bookmark to edit: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, "release/2026-08-04@origin"),
        before,
        "the remote-only release was edited anyway"
    );
}
