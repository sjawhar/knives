//! `knives release rebase`: the whole composition moves onto a newer trunk.
//!
//! Refused when every pin is frozen, when a stale parent cannot be mapped, when
//! every member landed, or when the release is held only as a remote ref; a
//! followed dated release that moved sideways is repaired with a merge. A bare
//! rebase finds its target through the merged pull requests read from the
//! snapshot and drops members whose work landed — by merge or by squash — unless
//! `--no-drop` or work past the pull keeps them; a second rebase does not grow
//! the release, and one already at its target still drops landed members.

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

use forge_shim::pull_record;
use knives::jj::Repo;
use lab::{
    Lab, ReleaseOutput, commit_at, extend_branch, file_at_revision, knives_release,
    release_parents, release_test_home, release_test_home_pinned,
};
use release_forge::{
    ReleaseWithSnapshotForgeInput, knives_release_with_forge, release_with_snapshot_forge,
};

#[test]
fn release_rebase_refuses_when_every_pin_is_frozen() {
    // Given: a dated release whose only consumer pins it by revision. Moving the
    // bookmark in place would reach nobody, so this requires a new dated cut.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home_pinned(
        &lab,
        "rev = \"release/2026-08-03\"",
        "rev = \"release/2026-08-04\"",
    );
    lab.advance_upstream("upstream advance\n");
    let before = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit(release)
        .expect("resolve release before refusal");

    // When: the real binary is asked to rebase the release onto upstream.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: it directs the caller to a dated cut, exits incomplete, and does not move it.
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("frozen") && stdout.contains("cut a new dated release"),
        "frozen-pin guidance missing: {stdout}"
    );
    let after = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit(release)
        .expect("resolve release after refusal");
    assert_eq!(before, after, "a frozen release was moved in place");
}

#[test]
fn release_rebase_refusal_for_fixed_release_explains_that_revision_pins_cannot_follow_it() {
    // Given: a fixed release branch whose only consumer pin is a frozen revision.
    let lab = Lab::new();
    let release = "integration";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let consumer = lab.consumer_with_pin_history(
        "pyproject.toml",
        "# checkout pin\nwork = { git = \"https://forge.invalid/acme/work.git\", rev = \"integration\" }\n",
        "# origin pin\nwork = { git = \"https://forge.invalid/acme/work.git\", rev = \"integration\" }\n",
    );
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nrelease_branch = \"integration\"\n",
            lab.work.display(),
            lab.upstream.display(),
        ),
    )
    .expect("write fixed-release registry");
    lab.advance_upstream("upstream advance\n");

    // When: the fixed release is asked to move in place.
    let output = knives_release(
        &lab,
        &home,
        &[
            "--consumer",
            consumer.to_str().expect("utf-8 consumer path"),
            "rebase",
            "main@upstream",
        ],
    );

    // Then: it is incomplete and names the only viable remediation.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("update the frozen consumer pins")
            && stdout.contains("fixed branches cannot reach revision pins"),
        "fixed-scheme guidance missing: {stdout}"
    );
}

#[test]
fn release_rebase_repairs_a_followed_dated_release_with_a_sideways_merge() {
    // Given: an existing dated release, a consumer that follows it, and a new upstream commit.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let previous_parents = Repo::open(&lab.work)
        .expect("open release repository")
        .parents_of(release)
        .expect("read existing release parents");
    let (home, _consumer) = release_test_home(&lab);
    lab.advance_upstream("upstream advance\n");
    let upstream = Repo::open(&lab.work)
        .expect("reopen release repository")
        .resolve_commit("main@upstream")
        .expect("resolve advanced upstream");
    // When: the repair command moves the existing release onto a new flat merge.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: the command succeeds; the legacy trunk parent is shed — the base is
    // never a parent — and the members were rebased onto the new upstream,
    // their bookmarks following.
    assert!(
        output.status.success(),
        "release rebase failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let repo = Repo::open(&lab.work).expect("reopen repaired release repository");
    let parents = release_parents(&lab, release);
    let old_base = &previous_parents[0]; // lab.octopus puts main@origin first
    assert!(
        !parents.contains(&old_base.commit),
        "superseded base still a parent: {parents:?}"
    );
    assert!(
        !parents.contains(&upstream),
        "the new upstream must not become a parent either: {parents:?}"
    );
    assert_eq!(
        parents,
        vec![commit_at(&lab, "feat/alpha"), commit_at(&lab, "feat/beta")],
        "the rewritten members are the whole parent set"
    );
    assert!(
        repo.is_ancestor(&upstream, &commit_at(&lab, release))
            .expect("ancestry"),
        "the release must contain the upstream through its members"
    );
}

#[test]
fn a_second_rebase_does_not_grow_the_release() {
    // Given: a release rebased once already, and upstream advancing again.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is rebased after each upstream advance.
    for advance in ["first advance\n", "second advance\n"] {
        lab.advance_upstream(advance);
        let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);
        assert!(
            output.status.success(),
            "rebase failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Then: two parents — the members — whatever the rebase count. The first
    // rebase sheds the legacy trunk parent and no rebase adds one back.
    let parents = Repo::open(&lab.work)
        .expect("open")
        .parents_of(release)
        .expect("parents");
    assert_eq!(parents.len(), 2, "parents accumulated: {parents:?}");
}

#[test]
fn a_rebase_refuses_a_stale_parent_it_cannot_map() {
    // Given: a standard release whose captured alpha tip is no longer held by alpha.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let stale_parent = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("resolve captured alpha");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work(["new", "-r", "feat/alpha"]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);
    let before = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit(release)
        .expect("resolve release before refusal");

    // When: the real binary attempts to replace the release base.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: it refuses rather than carrying the stale parent or moving the release.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(3), "{text}");
    assert!(
        text.contains(&stale_parent.as_str()[..12]),
        "refusal must name the stale parent: {text}"
    );
    assert!(
        text.contains("feat/alpha (now "),
        "refusal must say where alpha moved: {text}"
    );
    let after = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit(release)
        .expect("resolve release after refusal");
    assert_eq!(before, after, "refusal moved the release");
}

#[test]
fn a_rebase_keeps_a_landed_bookmark_held_member() {
    // Given: alpha is both held by its bookmark and reachable from the explicit replacement.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let alpha = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("resolve alpha");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work([
        "new",
        "-r",
        "main@upstream",
        "-r",
        "feat/alpha",
        "-m",
        "upstream merge carrying alpha",
    ]);
    let replacement = lab.revision(&lab.work, "@", "commit_id");
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);

    // When: rebase uses that merge as its explicit replacement base.
    let output = knives_release(&lab, &home, &["rebase", replacement.trim()]);

    // Then: alpha remains a direct member parent despite being reachable from replacement.
    assert!(
        output.status.success(),
        "rebase failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parents = Repo::open(&lab.work)
        .expect("reopen")
        .parents_of(release)
        .expect("read repaired parents");
    assert_eq!(parents.len(), 2, "parents: {parents:?}");
    assert!(
        parents.iter().any(|parent| parent.commit == alpha),
        "landed alpha was dropped: {parents:?}"
    );
}

#[test]
fn a_rebase_moves_the_whole_composition_onto_the_target() {
    // Given: two members forked from the old trunk, their merge conflict
    // resolved on the release, and an upstream that has advanced. `rebase` is
    // `jj rebase -b <release> -d <target>`: the members move onto the target
    // and the release moves with them — the trunk never becomes a parent.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        Repo::open(&lab.work)
            .expect("open first cut")
            .resolve_commit("release/2026-08-04")
            .is_ok(),
        "first cut was not named: {first:?}"
    );
    lab.jj_work(["new", "release/2026-08-04"]);
    std::fs::write(lab.work.join("shared.txt"), "resolved\n").expect("resolve conflict");
    lab.jj_work(["squash"]);
    let old_alpha = commit_at(&lab, "feat/alpha");
    lab.advance_upstream("advance\n");

    // When: the composition is rebased onto the advanced trunk.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);
    assert!(
        output.status.success(),
        "rebase failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the members were rewritten onto the new trunk and their bookmarks
    // followed; the release's parents are exactly the moved members.
    let repo = Repo::open(&lab.work).expect("reopen after rebase");
    let trunk = commit_at(&lab, "main@upstream");
    let new_alpha = commit_at(&lab, "feat/alpha");
    assert_ne!(new_alpha, old_alpha, "alpha was not rewritten");
    assert!(
        repo.is_ancestor(&trunk, &new_alpha).expect("ancestry"),
        "alpha does not sit on the new trunk"
    );
    let parents = release_parents(&lab, "release/2026-08-04");
    assert!(parents.contains(&new_alpha), "{parents:?}");
    assert!(
        !parents.contains(&trunk),
        "the trunk must not become a parent: {parents:?}"
    );
    assert_eq!(parents.len(), 2, "{parents:?}");
    assert!(
        repo.is_ancestor(&trunk, &commit_at(&lab, "release/2026-08-04"))
            .expect("ancestry"),
        "the release does not contain the new trunk through its members"
    );
    // And: the recorded resolution replayed through the rebase.
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "shared.txt"),
        "resolved\n"
    );
}

#[test]
fn a_bare_rebase_with_no_merged_pull_request_requires_a_commit() {
    // Given: a release in hand, and a forge whose only pull request is still open.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.advance_upstream("upstream advance\n");
    let before = commit_at(&lab, release);
    let pulls = format!("[{}]", pull_record(7, "OPEN", "feat/alpha", None));

    // When: the bare rebase asks the forge for a default target.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: with nothing merged there is no default, and the release stays put.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(
            "demo: no pull request has merged, so there is no default target; \
             provide a commit to rebase onto"
        ),
        "missing refusal guidance: {stdout}"
    );
    assert_eq!(before, commit_at(&lab, release), "the release moved anyway");
}

#[test]
fn a_bare_rebase_reads_merged_pulls_through_the_snapshot() {
    // Given: alpha merged upstream by a merge commit, beta still open, and the
    // trunk advanced past that merge afterwards.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let merged_at = commit_at(&lab, "main@upstream");
    lab.advance_upstream("beyond the merge\n");
    let tip = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str())),
        pull_record(8, "OPEN", "feat/beta", None)
    );

    // When: the bare rebase asks the forge instead of taking a target.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: the release lands on the merge commit, not the later tip.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "demo: every merged pull request (#7) is in main@upstream by {}; rebasing onto it",
            &merged_at.as_str()[..12]
        )),
        "missing target explanation: {stdout}"
    );
    let repo = Repo::open(&lab.work).expect("reopen after the default rebase");
    let at_release = commit_at(&lab, release);
    assert!(
        repo.is_ancestor(&merged_at, &at_release).expect("ancestry"),
        "the release does not contain the merge commit"
    );
    assert!(
        !repo.is_ancestor(&tip, &at_release).expect("ancestry"),
        "the release overshot the merged point onto the trunk tip"
    );
}

#[test]
fn a_bare_rebase_covers_the_latest_of_several_merged_pull_requests() {
    // Given: alpha and gamma merged upstream in that order, so gamma's merge
    // commit is the first trunk commit containing both.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let first_merge = commit_at(&lab, "main@upstream");
    lab.publish_pull("feat/gamma", 8);
    lab.merge_pull_with_merge_commit(8);
    let second_merge = commit_at(&lab, "main@upstream");
    lab.advance_upstream("beyond both merges\n");
    let tip = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(first_merge.as_str())),
        pull_record(8, "MERGED", "feat/gamma", Some(second_merge.as_str()))
    );

    // When: the bare rebase chooses among several merged pull requests.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: the later merge commit wins because it contains the earlier one.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "demo: every merged pull request (#7, #8) is in main@upstream by {}; \
             rebasing onto it",
            &second_merge.as_str()[..12]
        )),
        "missing target explanation: {stdout}"
    );
    let repo = Repo::open(&lab.work).expect("reopen after the default rebase");
    let at_release = commit_at(&lab, release);
    assert!(
        repo.is_ancestor(&second_merge, &at_release)
            .expect("ancestry"),
        "the release does not contain the covering merge commit"
    );
    assert!(
        !repo.is_ancestor(&tip, &at_release).expect("ancestry"),
        "the release overshot the merged point onto the trunk tip"
    );
}

#[test]
fn a_bare_rebase_refuses_when_a_merged_candidate_fact_is_omitted() {
    // Given: discovery names two merged pull requests that are on local trunk,
    // but the facts batch answers only the latter one.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let first_merge = commit_at(&lab, "main@upstream");
    lab.publish_pull("feat/gamma", 8);
    lab.merge_pull_with_merge_commit(8);
    let second_merge = commit_at(&lab, "main@upstream");
    let before = commit_at(&lab, release);
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(first_merge.as_str())),
        pull_record(8, "MERGED", "feat/gamma", Some(second_merge.as_str()))
    );

    let output = release_with_snapshot_forge(ReleaseWithSnapshotForgeInput {
        lab: &lab,
        home: &home,
        pulls: &pulls,
        withheld_facts: &[7],
        args: &["rebase"],
        output: ReleaseOutput::Text,
    });

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let text = format!("{stdout}\n{stderr}");
    assert!(
        text.contains("#7") && !text.contains("rebasing onto it"),
        "the refusal did not name the unanswered merged pull request: {text}"
    );
    assert_eq!(before, commit_at(&lab, release), "the release moved anyway");
}

#[test]
fn a_bare_rebase_refuses_merged_work_missing_from_the_local_trunk() {
    // Given: the forge says a pull request merged, but its merge commit is not
    // in the local repository at all.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.advance_upstream("upstream advance\n");
    let before = commit_at(&lab, release);
    let pulls = format!(
        "[{}]",
        pull_record(
            7,
            "MERGED",
            "feat/alpha",
            Some("feedfacefeedfacefeedfacefeedfacefeedface")
        )
    );

    // When: the bare rebase tries to place that merge on the local trunk.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: it refuses with fetch guidance rather than guessing, and moves nothing.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(
            "demo: the merge commit(s) of #7 are not in the local main@upstream; \
             run knives sync, or provide a commit to rebase onto"
        ),
        "missing fetch guidance: {stdout}"
    );
    assert_eq!(before, commit_at(&lab, release), "the release moved anyway");
}

#[test]
fn a_bare_rebase_drops_a_member_whose_pull_request_landed() {
    // Given: alpha merged upstream by a merge commit and beta still open.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let merged_at = commit_at(&lab, "main@upstream");
    lab.advance_upstream("beyond the merge\n");
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str())),
        pull_record(8, "OPEN", "feat/beta", None)
    );

    // When: the bare rebase lands on the merge commit.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: alpha's parent is dropped, its bookmark untouched, and the drop is
    // recorded on the release itself.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("dropped feat/alpha: landed upstream as #7"),
        "missing drop report: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, release),
        vec![commit_at(&lab, "feat/beta")],
        "the landed member must be the only parent removed"
    );
    assert!(
        Repo::open(&lab.work)
            .expect("reopen")
            .resolve_commit("feat/alpha")
            .is_ok(),
        "dropping the parent must not touch the branch"
    );
    assert!(
        lab.revision(&lab.work, release, "description")
            .contains("dropped feat/alpha: landed upstream as #7"),
        "the release description must record the drop"
    );
}

#[test]
fn no_drop_keeps_a_landed_member_as_a_parent() {
    // Given: the same landed alpha, and the caller asking to keep it.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let merged_at = commit_at(&lab, "main@upstream");
    let alpha = commit_at(&lab, "feat/alpha");
    let pulls = format!(
        "[{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str()))
    );

    // When: the bare rebase runs with --no-drop.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase", "--no-drop"]);

    // Then: the landed member stays a parent and nothing reports a drop.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!stdout.contains("dropped feat/alpha"), "{stdout}");
    assert!(
        release_parents(&lab, release).contains(&alpha),
        "--no-drop must keep the landed member"
    );
}

#[test]
fn a_bare_rebase_drops_a_member_landed_by_squash() {
    // Given: alpha squash-merged, so its commits replay empty onto the target
    // without the tip ever entering the trunk's history.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    let merged_at = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str()))
    );

    // When: the bare rebase lands on the squash commit.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: alpha is dropped and beta's rewritten tip is the only parent.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("dropped feat/alpha: landed upstream as #7"),
        "missing drop report: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, release),
        vec![commit_at(&lab, "feat/beta")],
        "the squash-landed member must be dropped"
    );
}

#[test]
fn a_bare_rebase_keeps_a_landed_branch_that_carries_work_past_its_pull() {
    // Given: alpha's pull request holds only its first commit, the branch has a
    // second commit past it, and the release carries the extended tip. The pull
    // then squash-merges, so the branch still holds work the trunk lacks.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.publish_pull("feat/alpha", 7);
    extend_branch(&lab, "feat/alpha", "alpha-more.txt", "work past the pull\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.squash_merge_pull(7, None);
    let merged_at = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str()))
    );

    // When: the bare rebase lands on the squash commit.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: alpha is kept, with the reason stated, and both members remain.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("kept feat/alpha: it carries work past #7"),
        "missing keep report: {stdout}"
    );
    assert!(!stdout.contains("dropped feat/alpha"), "{stdout}");
    assert_eq!(
        release_parents(&lab, release).len(),
        2,
        "both members must remain parents"
    );
}

#[test]
fn a_rebase_refuses_a_composition_whose_every_member_landed() {
    // Given: a single-member release whose only branch merged upstream. jj
    // would re-parent the release straight onto the destination, leaving the
    // trunk as its only parent — and the base is never a parent.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", release]);
    assert!(cut.status.success(), "{cut:?}");
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let before = commit_at(&lab, release);

    // When: the rebase is pointed past the landing, explicitly.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: it refuses rather than making the trunk a parent, and moves nothing.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(
            "every member of release/2026-08-04 has landed in main@upstream; rebasing would \
             make the trunk the only parent, so nothing moved \u{2014} reap the release or \
             include new work"
        ),
        "missing fully-landed refusal: {stdout}"
    );
    assert_eq!(before, commit_at(&lab, release), "the release moved anyway");
}

#[test]
fn a_release_already_at_its_target_still_drops_landed_members() {
    // Given: both members squash-merged and the release already rebased onto
    // their covering commit with --no-drop, so it carries two landed members
    // as empty replayed chains.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    let first_merge = commit_at(&lab, "main@upstream");
    lab.publish_pull("feat/beta", 8);
    lab.squash_merge_pull(8, None);
    let second_merge = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(first_merge.as_str())),
        pull_record(8, "MERGED", "feat/beta", Some(second_merge.as_str()))
    );
    let kept = knives_release_with_forge(&lab, &home, &pulls, &["rebase", "--no-drop"]);
    assert!(kept.status.success(), "{kept:?}");

    // When: the bare rebase finds the release already contains the target.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: nothing moves, and dropping every landed member is refused because
    // it would leave the release without a parent.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("already contains"),
        "missing already-contains skip: {stdout}"
    );
    assert!(
        stdout.contains(
            "every member of release/2026-08-04 landed; dropping them all would leave it \
             without a parent, so nothing was dropped"
        ),
        "missing parentless-drop refusal: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, release).len(),
        2,
        "a refused drop must leave the parents alone"
    );
}

#[test]
fn a_release_already_containing_the_reference_by_ancestry_is_left_alone() {
    // Given: a release whose alpha parent merged the upstream advance, so the
    // seed trunk is reachable through alpha rather than a direct release parent.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let seed = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("main@upstream")
        .expect("seed tip");
    lab.advance_upstream("advance\n");
    lab.jj_work([
        "new",
        "feat/alpha",
        "main@upstream",
        "-m",
        "merge upstream into alpha",
    ]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    lab.jj_work(["new", "-r", "feat/alpha", "-r", "feat/beta", "-m", release]);
    lab.jj_work(["bookmark", "create", release, "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);
    let before = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit(release)
        .expect("release");

    // When: asked to include the seed, an ancestor of alpha's merged base.
    let output = knives_release(&lab, &home, &["rebase", seed.as_str()]);

    // Then: containment is recognized and the release does not move.
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("already contains"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let after = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit(release)
        .expect("release");
    assert_eq!(
        before, after,
        "release moved for an already-contained commit"
    );
}

#[test]
fn a_release_contains_the_trunk_through_a_parents_history_not_as_a_direct_parent() {
    // Given: a member branch that merged the advanced upstream, and a release
    // whose direct parents are the seed trunk, that branch, and another branch.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.advance_upstream("advance\n");
    lab.jj_work([
        "new",
        "feat/alpha",
        "main@upstream",
        "-m",
        "merge upstream into alpha",
    ]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    lab.octopus(release, "feat/alpha", "feat/beta");

    // When/Then: the probe sees containment through ancestry.
    let repo = Repo::open(&lab.work).expect("open");
    assert_eq!(
        knives::commands::release::trunk_lag(&repo, Some(release), "main@upstream"),
        None,
        "trunk is contained through feat/alpha's merge; the probe must not report lag"
    );
}

#[test]
fn a_rebase_refuses_a_release_held_only_as_a_remote_ref() {
    // A rebase moves the release bookmark the same way an edit does, and it does
    // so after the duplicate has been made and described. With no local bookmark
    // to move, that leaves a described commit behind and fails on jj's bookmark
    // name parser instead of saying what is missing.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.push_branch("release/2026-08-04");
    lab.jj_work(["bookmark", "forget", "release/2026-08-04"]);
    lab.advance_upstream("upstream advance\n");
    let before = release_parents(&lab, "release/2026-08-04@origin");

    let output = knives_release(&lab, &home, &["rebase"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("jj bookmark track release/2026-08-04@origin"),
        "the refusal must say how to get a local bookmark to move: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, "release/2026-08-04@origin"),
        before,
        "the remote-only release was rebased anyway"
    );
}
