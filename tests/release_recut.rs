//! The second and later cuts: what a recut carries from the previous composition.
//!
//! The recorded parent set is the membership. An omitted member is refused
//! until its drop is stated and a stated drop is restated at the next cut; a
//! member that landed upstream is carried, one merged past the candidate's base
//! is dropped; a stranded parent names where its branch went; a foreign pull
//! request can be fetched and carried; the previous release's parents are
//! carried verbatim by commit, whatever their bookmarks are doing. The orphan
//! gate refuses a recut that would strand release-lineage work, and a stated
//! drop does not trip it. The shared base is the members' fork point, and a
//! recut after upstream drift carries the recorded resolution.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::ids::ReleaseScheme;
use knives::jj::Repo;
use lab::{
    Lab, commit_at, file_at_revision, home_after_first_cut, knives_release, release_parents,
    release_test_home,
};

#[test]
fn a_release_cut_records_its_whole_parent_set_under_the_release_name() {
    // Given: two branches with distinct tips that the cut must preserve as evidence.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let alpha = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("alpha tip");

    // When: a first cut is taken through the binary.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-15"]);
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "cut failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the release ref is the subject, every member is named, and their
    // commit ids remain evidence for a reader to verify later.
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let cut = entries
        .iter()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-15"))
        .unwrap_or_else(|| panic!("no cut entry: {entries:?}"));
    assert_eq!(cut.kind, knives::ledger::Kind::Event);
    assert!(cut.text.contains("feat/alpha"), "was: {}", cut.text);
    assert!(cut.text.contains("feat/beta"), "was: {}", cut.text);
    assert!(cut.text.contains("2 parent(s)"), "was: {}", cut.text);
    assert!(
        cut.evidence
            .iter()
            .any(|reference| reference == alpha.as_str()),
        "was: {:?}",
        cut.evidence
    );
    let created = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit("release/2026-08-15")
        .expect("release tip");
    assert_eq!(cut.anchor.as_deref(), Some(created.as_str()));

    let second = knives_release(&lab, &home, &["cut", "release/2026-08-16"]);
    assert!(
        second.status.success() || second.status.code() == Some(1),
        "second cut failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let second_cut = entries
        .iter()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-16"))
        .unwrap_or_else(|| panic!("no second cut entry: {entries:?}"));
    assert!(
        second_cut
            .text
            .contains("previous cut release/2026-08-15 recorded 2 member(s); all carried"),
        "was: {}",
        second_cut.text
    );
}

#[test]
fn a_cut_that_omits_a_recorded_member_is_refused_until_the_drop_is_stated() {
    // Given: a three-member cut through the binary, then the release rebuilt
    // by hand without feat/gamma — the bookmark moved, so the repository no
    // longer remembers the old composition. The ledger's cut event does.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.jj_work(["new", "feat/alpha", "feat/beta", "-m", "hand-rebuilt merge"]);
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        "@",
        "--allow-backwards",
    ]);
    lab.jj_work(["new"]);

    // When: the next cut is taken without stating the drop.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is refused, the missing member is named, and nothing was cut.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("refusing to cut") && stdout.contains("feat/gamma@"),
        "{stdout}"
    );
    assert!(
        Repo::open(&lab.work)
            .expect("reopen after the refusal")
            .resolve_commit("release/2026-08-05")
            .is_err(),
        "the refused cut was published anyway"
    );

    // And when: the drop is stated.
    let allowed = knives_release(&lab, &home, &["cut", "release/2026-08-05", "--allow-drop"]);

    // Then: the cut lands and its ledger event records exactly what was dropped.
    let stdout = String::from_utf8_lossy(&allowed.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as"),
        "the stated drop was still refused: {stdout}\n{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let event = entries
        .iter()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-05"))
        .unwrap_or_else(|| panic!("no cut entry: {entries:?}"));
    assert!(
        event
            .text
            .contains("previous cut release/2026-08-04 recorded 3 member(s); dropped: feat/gamma@"),
        "was: {}",
        event.text
    );
}

#[test]
fn a_drop_between_cuts_is_restated_at_the_next_cut() {
    // Given: a member dropped through the tool itself. The drop recorded a why
    // on the release, but the next cut still ships less than the last one did,
    // and the cut is where that is stated for the record.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/beta", "--why", "not this time"],
    );
    assert!(dropped.status.success(), "{dropped:?}");

    // When: the next cut is taken without stating the drop.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the cut is refused and names the member the last cut recorded.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("refusing to cut") && stdout.contains("feat/beta@"),
        "{stdout}"
    );

    // And when: the drop is stated, the cut lands and records it.
    let allowed = knives_release(&lab, &home, &["cut", "release/2026-08-05", "--allow-drop"]);
    let stdout = String::from_utf8_lossy(&allowed.stdout);
    assert!(stdout.contains("cut release/2026-08-05 as"), "{stdout}");
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let event = entries
        .iter()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-05"))
        .unwrap_or_else(|| panic!("no cut entry: {entries:?}"));
    assert!(
        event.text.contains("dropped: feat/beta@"),
        "was: {}",
        event.text
    );
}

#[test]
fn a_recorded_member_that_landed_upstream_is_carried_not_dropped() {
    // Given: a member squash-merged upstream, the composition rebased onto the
    // landing, and the landed member dropped — the standard shrink that loses
    // no content, because the base now carries the member's diff.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    lab.fetch_work();
    let rebased = knives_release(&lab, &home, &["rebase", "main@upstream"]);
    assert!(rebased.status.success(), "{rebased:?}");
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/alpha", "--why", "landed upstream as #7"],
    );
    assert!(dropped.status.success(), "{dropped:?}");

    // When: the next cut is taken without --allow-drop.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: nothing refuses — the recorded member's content is in the cut
    // through its base — and the event records a full carry.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as"),
        "a landed member was treated as dropped: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let event = entries
        .iter()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-05"))
        .unwrap_or_else(|| panic!("no cut entry: {entries:?}"));
    assert!(
        event
            .text
            .contains("previous cut release/2026-08-04 recorded 2 member(s); all carried"),
        "was: {}",
        event.text
    );
}

#[test]
fn a_recorded_member_this_repository_cannot_resolve_is_named_in_the_refusal() {
    // Given: a ledger whose newest cut event names a commit this checkout has
    // never seen — a stale ledger, or a re-clone. Unverifiable must not read
    // as carried, and the gate applies even to a first cut: with every release
    // bookmark gone, the ledger is the only witness the composition existed.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let ledger_dir = home.path().join("ledger").join("demo");
    std::fs::create_dir_all(&ledger_dir).expect("create ledger directory");
    std::fs::write(
        ledger_dir.join("20260801T000000.000000000Z-dead.md"),
        "+++\n\
         ts = \"2026-08-01T00:00:00Z\"\n\
         owner = \"an-agent\"\n\
         subject = \"release/2026-08-01\"\n\
         kind = \"event\"\n\
         evidence = [\"feedfeedfeedfeedfeedfeedfeedfeedfeedfeed\", \"deaddeaddeaddeaddeaddeaddeaddeaddeaddead\"]\n\
         +++\n\
         cut release/2026-08-01 as feedfeedfeed with 1 parent(s): feat/old@deaddeaddead\n",
    )
    .expect("write a cut event by hand");

    // When: a first cut is taken with no release bookmark anywhere.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the refusal names the unresolvable member rather than shrugging.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("deaddeaddead (not known to this repository)"),
        "{stdout}"
    );

    // And when: the drop is stated, the first cut proceeds.
    let allowed = knives_release(&lab, &home, &["cut", "release/2026-08-05", "--allow-drop"]);
    let stdout = String::from_utf8_lossy(&allowed.stdout);
    assert!(stdout.contains("cut release/2026-08-05 as"), "{stdout}");
}

#[test]
fn a_member_merged_upstream_past_the_candidates_base_is_dropped_not_carried() {
    // Given: alpha merged into upstream by a MERGE COMMIT, so the recorded tip
    // is an ancestor of the trunk. The composition was never rebased onto the
    // landing, and a hand rebuild then dropped alpha: the trunk carries the
    // content, this cut does not. A fork-point replay would degenerate to the
    // member itself and read empty without consulting the candidate at all.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    lab.fetch_work();
    lab.jj_work(["new", "feat/beta", "-m", "hand-rebuilt without alpha"]);
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        "@",
        "--allow-backwards",
    ]);
    lab.jj_work(["new"]);

    // When: the next cut is taken without stating the drop.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is refused and names alpha — content the upstream carries is
    // not content this cut ships.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("refusing to cut") && stdout.contains("feat/alpha@"),
        "{stdout}"
    );
}

#[test]
fn an_unverified_member_stays_in_the_recorded_composition() {
    // Given: three members entangled in one file, so every cut is conflicted,
    // then a hand rebuild without alpha. Alpha's replay onto the conflicted
    // candidate answers nothing either way — and an unanswered question must
    // not fall out of the baseline, or one conflicted cut launders a member
    // out of the composition without anyone stating the drop.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    lab.branch("feat/gamma", "shared.txt", "gamma\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("cut release/2026-08-04 as"),
        "first conflicted cut was refused: {first:?}"
    );
    let alpha = commit_at(&lab, "feat/alpha");
    lab.jj_work([
        "new",
        "feat/beta",
        "feat/gamma",
        "-m",
        "hand-rebuilt without alpha",
    ]);
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        "@",
        "--allow-backwards",
    ]);
    lab.jj_work(["new"]);

    // When: the next cut is taken.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the check is inconclusive rather than a refusal, and the new cut's
    // event keeps alpha in evidence so the next gate rechecks it.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cut release/2026-08-05 as"), "{stdout}");
    assert!(stdout.contains("carry check inconclusive"), "{stdout}");
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let event = entries
        .iter()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-05"))
        .unwrap_or_else(|| panic!("no cut entry: {entries:?}"));
    assert!(
        event.text.contains("unverified: feat/alpha@"),
        "was: {}",
        event.text
    );
    assert!(
        event.evidence.iter().any(|sha| sha == alpha.as_str()),
        "alpha fell out of the baseline: {:?}",
        event.evidence
    );
}

#[test]
fn shared_base_selects_the_newest_of_multiple_trunk_reachable_release_parents() {
    // Given: a release carrying an old origin trunk parent, a newer upstream trunk
    // parent, and a feature parent. This is the accumulated-bases shape #11 leaves
    // behind after upstream advances.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work([
        "new",
        "-r",
        "main@origin",
        "-r",
        "main@upstream",
        "-r",
        "feat/alpha",
        "-m",
        "release/2026-08-04",
    ]);
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    let repo = Repo::open(&lab.work).expect("open");
    let release = repo
        .resolve_commit("release/2026-08-04")
        .expect("resolve release");
    let newest_trunk_parent = repo
        .resolve_commit("main@upstream")
        .expect("resolve upstream trunk");

    // When: the release's shared base is selected.
    let shared_base = knives::commands::release::shared_base(&repo, &release, &newest_trunk_parent)
        .expect("select shared base");

    // Then: the newer trunk parent wins, not the older accumulation residue.
    assert_eq!(shared_base, Some(newest_trunk_parent));
}

#[test]
fn a_flat_releases_shared_base_is_the_members_fork_point() {
    // Given: a doctrine-flat release — members only, the base never a parent —
    // and an upstream that has advanced past the members' fork point.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "flat release",
    ]);
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let fork_point = repo
        .resolve_commit("main@origin")
        .expect("resolve fork point");
    lab.advance_upstream("upstream advance\n");
    let reopened = Repo::open(&lab.work).expect("reopen");
    let release = reopened
        .resolve_commit("release/2026-08-04")
        .expect("resolve release");
    let trunk_tip = reopened
        .resolve_commit("main@upstream")
        .expect("resolve trunk tip");

    // When: the release's shared base is selected.
    let shared_base = knives::commands::release::shared_base(&reopened, &release, &trunk_tip)
        .expect("select shared base");

    // Then: it is the commit every member forks from, not the advanced tip and
    // not nothing — a flat release still has exactly one fork point.
    assert_eq!(shared_base, Some(fork_point));
}

#[test]
fn a_flat_release_recuts_after_upstream_drift_collides_with_a_member() {
    // Given: a flat two-member cut, then upstream lands a squash that rewrites
    // the same file alpha creates. Auditing against the trunk tip instead of
    // the fork point made innocent beta read as diverging: its synthetic diff
    // deletes the drifted file while the cut modifies it.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "feature.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.publish_pull("feat/gamma", 9);
    lab.squash_merge_pull(9, Some("upstream drift\n"));

    // When: the identical composition is re-cut under the new name.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the cut lands — each member's content is measured from the fork
    // point, so upstream drift is not charged to the members. (The overall exit
    // still reports the unrelated lagging-trunk finding; only the audit is under
    // test here.)
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as"),
        "re-cut was refused: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("missing or diverges"),
        "audit false positive: {stdout}"
    );
    assert_eq!(
        release_parents(&lab, "release/2026-08-05"),
        vec![commit_at(&lab, "feat/alpha"), commit_at(&lab, "feat/beta")],
        "the re-cut must carry the composition verbatim"
    );
}

#[test]
fn a_recut_carries_a_recorded_resolution_that_dropped_content() {
    // Given: two members conflicting on one file, the conflict resolved by hand
    // ON the release with content that matches neither side — a published
    // judgment that deliberately diverges from both members.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("cut release/2026-08-04 as"),
        "first cut was refused: {first:?}"
    );
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::write(lab.work.join("shared.txt"), "merged\n").expect("resolve by hand");
    lab.jj_work(["new"]);

    // When: the identical composition is re-cut under a new name.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the recorded resolution is carried, reported, and not refused —
    // the audit charges a cut only with divergence it introduces.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as"),
        "re-cut was refused: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("missing or diverges"),
        "a recorded resolution was refused as a loss: {stdout}"
    );
    assert!(
        stdout.contains(
            "feat/alpha: diverges where the previous release already did \
             (a recorded resolution); carried forward"
        ) && stdout.contains(
            "feat/beta: diverges where the previous release already did \
             (a recorded resolution); carried forward"
        ),
        "missing carried-resolution report: {stdout}"
    );
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-05", "shared.txt"),
        "merged\n",
        "the resolution must survive the re-cut"
    );
}

#[test]
fn a_stranded_release_parent_reports_where_the_branch_went() {
    // The payload the design asks for. `parents_of` only ever reports bookmarks
    // pointing AT a parent, so the pure detector can only ever say "carries no
    // bookmark". Naming the branch and its new tip is the actionable half.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let stranded = lab.revision(&lab.work, "feat/alpha", "commit_id");

    // The branch moves on, leaving the old commit with nothing pointing at it.
    lab.jj_work(["new", "feat/alpha", "-m", "more work"]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    let moved_to = lab.revision(&lab.work, "feat/alpha", "commit_id");
    assert_ne!(stranded.trim(), moved_to.trim());

    let repo = Repo::open(&lab.work).expect("open");
    let trunk = repo.resolve_commit("main@upstream").expect("trunk");
    let branches = knives::release_model::carried_from_tips(
        &repo.bookmark_tips().expect("tips"),
        "main",
        &ReleaseScheme::Dated,
    );
    let past = knives::release_model::BranchSuccessions::of(
        &repo,
        std::slice::from_ref(&trunk),
        &branches,
    )
    .expect("branch successions")
    .successors_of(&knives::ids::CommitId::new(stranded.trim()))
    .expect("branches succeeding");
    assert!(
        past.iter()
            .any(|(branch, tip)| branch == "feat/alpha" && tip.as_str() == moved_to.trim()),
        "expected feat/alpha reported at its new tip, got {past:?}"
    );
}

#[test]
fn a_foreign_pull_request_can_be_fetched_and_carried_as_a_release_parent() {
    // The design allows a release parent to be any upstream pull request, not
    // only our own branches. Without the objects locally that commit cannot be a
    // merge parent at all, and none of the obvious fetch routes work: jj brings
    // branches only, and importing a raw pull ref leaves the commit invisible.
    let lab = lab::Lab::new();
    lab.branch("feat/theirs", "theirs.txt", "someone else's work\n");
    lab.push_branch("feat/theirs");
    lab.publish_pull("feat/theirs", 42);
    let sha = lab.revision(&lab.work, "feat/theirs", "commit_id");

    // A clone that has never seen the branch, only the pull ref.
    let fetched = knives::jj::fetch_pull_ref(&lab.second, &lab.upstream.display().to_string(), 42)
        .expect("fetch pull ref");
    assert_eq!(fetched.as_str(), sha.trim(), "fetched the wrong commit");

    // And it is usable as a parent, which is the whole point.
    let trunk = knives::ids::CommitId::new(lab.revision(&lab.second, "main", "commit_id").trim());
    let merge = knives::jj::write_release(
        &lab.second,
        &knives::jj::ReleaseWrite {
            source: None,
            parents: &[trunk, fetched],
            message: Some("release: with a foreign PR"),
            bookmark: None,
            operation: "knives: cut with a foreign PR",
        },
    )
    .expect("merge");
    let parents = knives::jj::Repo::open(&lab.second)
        .expect("open")
        .parents_of(merge.as_str())
        .expect("parents");
    assert_eq!(parents.len(), 2);
}

#[test]
fn a_cut_refuses_when_release_like_described_work_lives_only_in_the_release_lineage() {
    // Given: a real hotfix uses a release-like description while stacked on the
    // old release. The next flat cut would not include it, and no keeper reaches it.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work([
        "new",
        "release/2026-08-04",
        "-m",
        "chore(release): restore missing file",
    ]);
    std::fs::write(lab.work_path().join("hotfix.txt"), "fix\n").expect("write hotfix");
    lab.jj_work(["new"]); // park @ off the hotfix so it snapshots as its own commit
    let stacked = lab.revision(
        lab.work_path(),
        "description(glob:\"hotfix*\")",
        "commit_id",
    );
    let (home, _consumer) = release_test_home(&lab);

    // When: a newer cut is attempted without acknowledgement.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: refused, naming the exact commit, and no new bookmark exists.
    assert!(!output.status.success(), "cut should have refused");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains(&stacked.chars().take(12).collect::<String>()),
        "refusal must name the commit: {text}"
    );
    assert!(
        text.contains("--allow-drop"),
        "refusal must name the override: {text}"
    );
    let tips = Repo::open(&lab.work)
        .expect("open")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !tips
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-05")
    );

    // And when: the operator states that dropping the hotfix is intended.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05", "--allow-drop"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let tips = Repo::open(&lab.work)
        .expect("reopen after overridden cut")
        .bookmark_tips()
        .expect("read bookmark tips after overridden cut");
    assert!(
        tips.keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-05"),
        "the acknowledged cut must create the requested release"
    );
}

#[test]
fn a_dropped_branch_does_not_trip_the_orphan_gate() {
    // Given: a member dropped from the release. Its bookmark still holds its
    // content, so nothing is lost and the gate must stay quiet.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/beta", "--why", "not this time"],
    );
    assert!(dropped.status.success(), "{dropped:?}");

    // When: the next cut is made without --allow-drop.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it cuts, because feat/beta's bookmark still reaches its commits.
    assert!(
        output.status.success(),
        "gate tripped on a dropped branch: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // And: the drop it was given really removed something. A gate that stays
    // quiet over an unchanged release proves nothing about dropped content.
    let beta = commit_at(&lab, "feat/beta");
    assert!(
        !release_parents(&lab, "release/2026-08-05").contains(&beta),
        "the dropped branch is still a parent of the successor cut"
    );
}

#[test]
fn cut_with_a_previous_release_duplicates_it_verbatim() {
    // Given: a release, and a branch created after it. A cut is a new name for
    // the composition in hand, never a recomputation: the new branch joins
    // through `include`, and nothing advances.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let previous = release_parents(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let parents = release_parents(&lab, "release/2026-08-05");
    assert_eq!(parents, previous, "a cut must not recompute membership");
    let gamma = commit_at(&lab, "feat/gamma");
    assert!(
        !parents.contains(&gamma),
        "a branch must join through include, not by existing: {parents:?}"
    );
    assert!(stdout.contains("reaped release/2026-08-04"), "{stdout}");
    assert!(
        !Repo::open(&lab.work)
            .expect("reopen after the cut")
            .bookmark_tips()
            .expect("read bookmark tips")
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-04"),
        "the reap was reported but the superseded bookmark survived"
    );
}

#[test]
fn a_verbatim_cut_carries_a_member_whose_bookmark_is_divergent() {
    // Given: alpha pushed, rewritten locally and released at that rewrite, then
    // rewritten differently in another clone and pushed from there. The fetch
    // leaves alpha's bookmark divergent: one target the released copy, one the
    // pushed head - the state a member is left in when its pull request head
    // and its release copy both claim the name. The cut carries the previous
    // release's parents verbatim, so the released copy is kept by construction;
    // resolving members through their bookmarks read it as dropped.
    let lab = Lab::new();
    lab.branch("feat/alpha", "feature.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.push_branch("feat/alpha");
    lab.jj_at(&lab.second, ["git", "fetch", "--remote", "origin"]);
    lab.jj_at(&lab.second, ["bookmark", "track", "feat/alpha@origin"]);
    lab.rewrite_local_branch("feat/alpha", "released rewrite\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let released_alpha = commit_at(&lab, "feat/alpha");
    lab.jj_at(&lab.second, ["edit", "--ignore-immutable", "feat/alpha"]);
    std::fs::write(lab.second.join("feature.txt"), "pull request rewrite\n")
        .expect("rewrite in the second clone");
    lab.jj_at(&lab.second, ["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_at(&lab.second, ["new"]);
    lab.jj_at(
        &lab.second,
        [
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            "feat/alpha",
        ],
    );
    lab.fetch_work();
    let repo = Repo::open(&lab.work).expect("open");
    assert!(
        repo.conflicted_bookmarks()
            .expect("conflicted bookmarks")
            .iter()
            .any(
                |(reference, targets)| reference.branch().as_str() == "feat/alpha"
                    && targets.contains(&released_alpha)
            ),
        "feat/alpha must be divergent with the released copy as one target, or this test \
         proves nothing"
    );

    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("cut release/2026-08-05 as"),
        "a verbatim cut refused over a divergent member bookmark: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        release_parents(&lab, "release/2026-08-05").contains(&released_alpha),
        "the released copy was not carried"
    );
}
