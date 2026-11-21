//! The second and later cuts: what a recut carries from the previous composition.
//!
//! The recorded parent set is the membership. An omitted member is refused
//! until its drop is stated and a stated drop is restated at the next cut; a
//! member that landed upstream is carried, one merged past the candidate's base
//! is dropped; a stranded parent names where its branch went; a foreign pull
//! request can be fetched and carried; the previous release's parents are
//! carried verbatim by commit, whatever their bookmarks are doing. The orphan
//! gate refuses a recut that would strand release-lineage work, and a stated
//! drop does not trip it. A branch of `origin` or of the publish remote keeps
//! the work it holds, tracked or not; a branch with a release name, or of
//! `upstream`, keeps nothing. The shared base is the members' fork point, and
//! a recut after upstream drift carries the recorded resolution.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::ids::{CommitId, ReleaseScheme};
use knives::jj::Repo;
use lab::{
    Lab, commit_at, file_at_revision, home_after_first_cut, knives_release, operation_ids,
    release_parents, release_test_home,
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
        .read()
        .expect("read ledger")
        .entries;
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
        .read()
        .expect("read ledger")
        .entries;
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
        .read()
        .expect("read ledger")
        .entries;
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
        .read()
        .expect("read ledger")
        .entries;
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
        .read()
        .expect("read ledger")
        .entries;
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
fn a_recorded_member_rebased_and_adapted_is_carried_not_dropped() {
    // Given: a member rebased onto the advanced trunk and adapted there — its
    // file rewritten on the new base, so the recorded commit's own diff no
    // longer replays cleanly onto anything carrying the adapted version. jj
    // kept the change id; the composition follows the rebased branch through
    // `advance`. Observed on hawk 2026-09-10: a member recorded by commit at
    // one cut, rebased and folded into another parent, refused as "absent".
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let recorded = commit_at(&lab, "feat/alpha");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work(["rebase", "-b", "feat/alpha", "-d", "main@upstream"]);
    lab.jj_work(["new", "feat/alpha"]);
    std::fs::write(
        lab.work.join("alpha.txt"),
        "alpha, adapted to the new trunk\n",
    )
    .expect("adapt the member on its new base");
    lab.jj_work(["squash", "--into", "feat/alpha"]);
    let rewritten = commit_at(&lab, "feat/alpha");
    assert_ne!(recorded, rewritten, "the rebase must rewrite the member");
    assert_eq!(
        Repo::open(&lab.work)
            .expect("open")
            .change_id_of(rewritten.as_str())
            .expect("change id"),
        Repo::open(&lab.work)
            .expect("open")
            .change_id_of(recorded.as_str())
            .expect("change id"),
        "the rewrite must keep the change id, or this test proves nothing"
    );
    let advanced = knives_release(&lab, &home, &["advance", "feat/alpha"]);
    assert!(advanced.status.success(), "{advanced:?}");

    // When: the next cut is taken without --allow-drop.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the member is carried in its rebased form, not reported dropped.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as"),
        "a rebased member was treated as dropped: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        release_parents(&lab, "release/2026-08-05").contains(&rewritten),
        "the new cut must carry the rewritten member"
    );
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries;
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
        .read()
        .expect("read ledger")
        .entries;
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

/// Cut `name` from the hand-resolved `shared.txt` release and hold the cut to
/// what carrying that resolution means: written, nothing refused as a loss,
/// every row of `rows` printed, the resolution in its tree, and the exit
/// `code`. Returns its stdout.
fn cut_carrying_the_resolution(
    lab: &Lab,
    home: &tempfile::TempDir,
    name: &str,
    (rows, code): (&[String], i32),
) -> String {
    let output = knives_release(lab, home, &["cut", name]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout.contains(&format!("cut {name} as")),
        "re-cut was refused: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("missing or diverges"),
        "a carried resolution was refused as a loss: {stdout}"
    );
    for row in rows {
        assert!(
            stdout.contains(row.as_str()),
            "missing carried-resolution report: {stdout}"
        );
    }
    assert_eq!(output.status.code(), Some(code), "{stdout}");
    assert_eq!(
        file_at_revision(lab, name, "shared.txt"),
        "merged\n",
        "the resolution must survive the re-cut"
    );
    stdout
}

/// The cut's row for a member whose divergence the recorded release in hand
/// already has.
fn recorded_row(member: &str) -> String {
    format!(
        "  {member}: diverges where the release in hand (a recorded write) already does; \
         carried forward"
    )
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
    let (alpha, beta) = (commit_at(&lab, "feat/alpha"), commit_at(&lab, "feat/beta"));
    let missing_rows = |stdout: &str| {
        for (member, tip) in [("feat/alpha", &alpha), ("feat/beta", &beta)] {
            assert!(
                stdout.contains(&format!(
                    "  !! {member}@{}: the cut tree is missing or diverges from the member's \
                     content",
                    tip.short()
                )),
                "{stdout}"
            );
        }
        assert!(!stdout.contains("diverges where"), "{stdout}");
    };

    // When: the hand resolution, which nothing recorded, is verified.
    let verified = knives_release(&lab, &home, &["members", "release/2026-08-04", "--verify"]);

    // Then: verify holds the release to its members alone, as it always has:
    // the content neither member's tree holds reads as missing, a finding.
    let stdout = String::from_utf8_lossy(&verified.stdout);
    assert_eq!(verified.status.code(), Some(1), "{stdout}");
    missing_rows(&stdout);

    // When: the identical composition is re-cut under a new name, with no
    // adopt recording the resolution first. The working copy steps off the
    // release, or the reap after the cut keeps it for that, a finding of its
    // own.
    lab.jj_work(["new", "main@origin"]);
    let before_cut = operation_ids(&lab.work);
    let refused = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the cut is refused on the release's tree, whatever its audit
    // would say: nothing is written, and the refusal names the command that
    // records the resolution.
    let stdout = String::from_utf8_lossy(&refused.stdout);
    assert_eq!(refused.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("`knives release adopt release/2026-08-04`"),
        "{stdout}"
    );
    assert!(!stdout.contains("cut release/2026-08-05 as"), "{stdout}");
    assert_eq!(operation_ids(&lab.work), before_cut, "the refusal wrote");
    assert!(
        Repo::open(&lab.work)
            .expect("open")
            .local_bookmark_tip("release/2026-08-05")
            .is_none()
    );

    // Given: the hand resolution recorded as the first release's newest
    // write. It kept no line either member added, so `adopt` records it and
    // says so with findings.
    let adopted = knives_release(
        &lab,
        &home,
        &[
            "adopt",
            "release/2026-08-04",
            "--why",
            "kept neither side of shared.txt",
        ],
    );
    let adopted_stdout = String::from_utf8_lossy(&adopted.stdout);
    assert!(
        adopted_stdout.contains("as the newest write of release/2026-08-04"),
        "{adopted_stdout}\n{}",
        String::from_utf8_lossy(&adopted.stderr)
    );
    assert_eq!(adopted.status.code(), Some(1), "{adopted_stdout}");

    // When: the recorded release is verified.
    let verified = knives_release(&lab, &home, &["members", "release/2026-08-04", "--verify"]);

    // Then: verify is unchanged by the record: the content is still missing.
    let stdout = String::from_utf8_lossy(&verified.stdout);
    assert_eq!(verified.status.code(), Some(1), "{stdout}");
    missing_rows(&stdout);

    // When: the composition is re-cut.
    let rows = [recorded_row("feat/alpha"), recorded_row("feat/beta")];

    // Then: the recut's rows read the release in hand as a recorded write,
    // and it exits 0.
    let stdout = cut_carrying_the_resolution(&lab, &home, "release/2026-08-05", (&rows, 0));
    assert!(!stdout.contains("unrecorded"), "{stdout}");
    assert!(!stdout.contains("a recorded resolution"), "{stdout}");
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
            measured: knives::jj::Measured::WithoutCarriers(&[]),
            message: Some("release: with a foreign PR"),
            bookmark: None,
            publish_remote: None,
            earlier: &[],
            operation: "knives: cut with a foreign PR",
        },
    )
    .expect("merge")
    .commit;
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
    lab.jj_work(["new"]); // step @ off the hotfix so it snapshots as its own commit
    // The working copy now sits on the hotfix, so the hotfix is `@-`.
    let stacked = lab.revision(lab.work_path(), "@-", "commit_id");
    assert_eq!(stacked.len(), 40, "{stacked:?}");
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
    // And: the drop is reported after the reap. The hotfix rests on the old
    // cut, so the reap keeps that cut and nothing is dropped.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("dropping"), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "demo: --allow-drop: nothing dropped: the 1 commit(s) the cut does not carry are still visible: {}\n",
            &stacked[..12]
        )),
        "{stdout}"
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

/// The previous release of the tests of what keeps work on top of it.
const PREVIOUS: &str = "release/2026-08-04";
/// The cut those tests take after it.
const NEXT: &str = "release/2026-08-05";

/// A fork whose publish remote is a remote of its own, `release`, in the
/// shape of INFRA-4355's rehearsal: the previous release P, a flat merge of
/// alpha and beta, pushed to `release` and tagged; and C1, a commit on top of
/// P that the next cut does not carry, as `bet-1213/n1` was on 8.0.9.
///
/// Only `holder`, a `(remote, branch)`, holds C1: the branch is pushed there
/// and forgotten here, so it is that remote's alone, and untracked. `None`
/// leaves C1 held by nothing. Returns the lab, its home, P and C1.
fn work_on_a_published_release(
    holder: Option<(&str, &str)>,
) -> (Lab, tempfile::TempDir, CommitId, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let release_remote = lab.temp_path().join("release.git");
    lab::git_bare_repository(&release_remote);
    let release_url = release_remote.to_str().expect("utf-8 path");
    lab.jj_work(["git", "remote", "add", "release", release_url]);
    let (home, _consumer) = release_test_home(&lab);
    let registry = home.path().join("repos.toml");
    let entry = std::fs::read_to_string(&registry).expect("read the registry");
    std::fs::write(&registry, format!("{entry}release = \"{release_url}\"\n"))
        .expect("name the publish remote");
    let cut = knives_release(&lab, &home, &["cut", PREVIOUS]);
    assert!(cut.status.success(), "{cut:?}");
    assert_eq!(
        release_parents(&lab, PREVIOUS).len(),
        2,
        "P must be a flat merge of alpha and beta"
    );
    let previous = commit_at(&lab, PREVIOUS);
    lab.jj_work(["git", "push", "--remote", "release", "--bookmark", PREVIOUS]);
    lab.jj_work(["tag", "set", "v1.0.0", "-r", PREVIOUS]);
    lab.jj_work(["new", PREVIOUS, "-m", "work on the release"]);
    std::fs::write(lab.work.join("n1.txt"), "n1\n").expect("write C1");
    lab.jj_work(["new"]);
    let on_top = CommitId::new(lab.revision(&lab.work, "@-", "commit_id"));
    lab.jj_work(["new", "main@origin"]);
    if let Some((remote, branch)) = holder {
        lab.jj_work(["bookmark", "create", branch, "-r", on_top.as_str()]);
        lab.jj_work(["git", "push", "--remote", remote, "--bookmark", branch]);
        lab.jj_work(["bookmark", "forget", branch]);
    }
    let held_by: Vec<String> = Repo::open(&lab.work)
        .expect("open")
        .bookmark_tips()
        .expect("read bookmark tips")
        .into_iter()
        .filter(|(_, commit)| *commit == on_top)
        .map(|(reference, _)| reference.to_string())
        .collect();
    let named: Vec<String> = holder
        .map(|(remote, branch)| format!("{branch}@{remote}"))
        .into_iter()
        .collect();
    assert_eq!(
        held_by, named,
        "C1 must be held by the named branch alone, or this test proves nothing"
    );
    (lab, home, previous, on_top)
}

/// Assert that the cut in `output` was made and that its orphan gate did not
/// refuse it or list `commit`.
fn assert_not_refused_for(output: &std::process::Output, commit: &CommitId) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let listed = format!("    {}", commit.short());
    assert!(
        !stdout.contains("would be dropped") && !stdout.lines().any(|line| line == listed),
        "the orphan gate listed {}: {stdout}",
        commit.short()
    );
    assert!(
        matches!(output.status.code(), Some(0 | 1)),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains(&format!("cut {NEXT} as ")), "{stdout}");
}

/// Assert that the orphan gate refused the cut in `output` (exit 3), listing
/// `commit` alone, and that nothing was cut.
fn assert_refused_for(lab: &Lab, output: &std::process::Output, commit: &CommitId) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "demo: refusing to cut: 1 commit(s) are reachable only from {PREVIOUS} or its \
             descendants and would be dropped:\n    {}\n",
            commit.short()
        )),
        "{stdout}"
    );
    assert!(
        Repo::open(&lab.work)
            .expect("reopen after the refusal")
            .resolve_commit(NEXT)
            .is_err(),
        "the refused cut was made anyway"
    );
}

#[test]
fn an_untracked_origin_branch_keeps_the_work_it_holds_on_the_previous_release() {
    // Given (R10, K13 1): C1 on top of the published P, held only by an
    // untracked branch of `origin`, as BET's branches of sjawhar/rocq held
    // work on 8.0.9.
    let (lab, home, _previous, on_top) = work_on_a_published_release(Some(("origin", "bet/n1")));

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: the orphan gate keeps C1, and the cut is made.
    assert_not_refused_for(&output, &on_top);
}

#[test]
fn a_publish_remote_branch_keeps_the_work_it_holds_on_the_previous_release() {
    // Given (R10, K13 2): C1 held only by a branch of the publish remote,
    // under a name that is not a release name.
    let (lab, home, _previous, on_top) = work_on_a_published_release(Some(("release", "bet/n1")));

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: the orphan gate keeps C1, and the cut is made.
    assert_not_refused_for(&output, &on_top);
}

#[test]
fn work_on_the_previous_release_that_nothing_holds_refuses_the_cut() {
    // Given (R10, K13 3, a control): C1 held by nothing.
    let (lab, home, _previous, on_top) = work_on_a_published_release(None);

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: the orphan gate refuses it and lists C1.
    assert_refused_for(&lab, &output, &on_top);
}

#[test]
fn an_origin_branch_with_a_release_name_keeps_nothing() {
    // Given (R10, K13 4, a control): C1 held only by `release/scratch` on
    // `origin`, a release name that is not a dated cut.
    let (lab, home, _previous, on_top) =
        work_on_a_published_release(Some(("origin", "release/scratch")));

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: the orphan gate refuses it and lists C1.
    assert_refused_for(&lab, &output, &on_top);
}

#[test]
fn an_upstream_branch_keeps_nothing() {
    // Given (R10, K13 5, a control): C1 held only by a branch of `upstream`,
    // which its owner can delete.
    let (lab, home, _previous, on_top) = work_on_a_published_release(Some(("upstream", "bet/n1")));

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: the orphan gate refuses it and lists C1.
    assert_refused_for(&lab, &output, &on_top);
}

/// Edit the published P by hand: a new commit on P's parents with P's tree
/// and alpha repaired, the name moved onto it. The tag makes P immutable to
/// jj, so P itself cannot be edited. Returns the edit.
fn edit_the_published_release(lab: &Lab) -> CommitId {
    let parents = release_parents(lab, PREVIOUS);
    lab.jj_work([
        "new",
        parents[0].as_str(),
        parents[1].as_str(),
        "-m",
        PREVIOUS,
    ]);
    lab.jj_work(["restore", "--from", PREVIOUS]);
    std::fs::write(lab.work.join("alpha.txt"), "alpha, repaired\n").expect("repair alpha");
    lab.jj_work(["bookmark", "set", PREVIOUS, "-r", "@", "--allow-backwards"]);
    lab.jj_work(["new", "main@origin"]);
    commit_at(lab, PREVIOUS)
}

/// The line of the published remedy that `adopt` prints for `edited`, the
/// hand edit of P (`previous`). Its cut states the drop of `dropped`, where
/// the orphan gate would list that commit.
fn published_remedy(previous: &CommitId, edited: &CommitId, dropped: Option<&CommitId>) -> String {
    let stated = dropped.map_or_else(String::new, |commit| {
        format!(
            " with `--allow-drop`, which must print `--allow-drop: nothing dropped` naming \
             exactly {} (any other answer is a stop)",
            commit.short()
        )
    });
    format!(
        "demo: {PREVIOUS} is published (tag v1.0.0 names {p}), so a hand edit cannot be \
         recorded under it: move {PREVIOUS} back to its published commit ({PREVIOUS}@release, \
         {p}) (`jj bookmark set {PREVIOUS} -r {p} --allow-backwards`), then if the working copy \
         is on {e} or on an empty commit on it, `jj new 'trunk()'`, then cut a new dated \
         name{stated}, then `jj restore --from {e} --into <new name>`, then adopt the new name, \
         then `jj abandon {e}`, then `jj bookmark forget {PREVIOUS}` (never `jj bookmark \
         delete`, which a push would carry to the remote)\n",
        p = previous.short(),
        e = edited.short(),
    )
}

#[test]
fn the_published_remedy_does_not_name_work_an_origin_branch_holds() {
    // Given (R10, K13 6): the published P edited by hand, and C1 held only
    // by an untracked branch of `origin`.
    let (lab, home, previous, on_top) = work_on_a_published_release(Some(("origin", "bet/n1")));
    let edited = edit_the_published_release(&lab);

    // When: the edit is adopted.
    let adopted = knives_release(&lab, &home, &["adopt", "--why", "a repair"]);

    // Then: adopt refuses it with the published remedy, whose cut states no
    // drop: the orphan gate keeps C1.
    let stdout = String::from_utf8_lossy(&adopted.stdout);
    assert_eq!(adopted.status.code(), Some(3), "{stdout}");
    assert!(
        !stdout.contains(on_top.short()),
        "the remedy names {}: {stdout}",
        on_top.short()
    );
    assert!(
        stdout.contains(&published_remedy(&previous, &edited, None)),
        "{stdout}"
    );
}

#[test]
fn the_published_remedy_names_work_that_nothing_holds() {
    // Given (R10, K13 6, a control): the published P edited by hand, and C1
    // held by nothing.
    let (lab, home, previous, on_top) = work_on_a_published_release(None);
    let edited = edit_the_published_release(&lab);

    // When: the edit is adopted.
    let adopted = knives_release(&lab, &home, &["adopt", "--why", "a repair"]);

    // Then: adopt refuses it with the published remedy, whose cut states the
    // drop of C1.
    let stdout = String::from_utf8_lossy(&adopted.stdout);
    assert_eq!(adopted.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains(&published_remedy(&previous, &edited, Some(&on_top))),
        "{stdout}"
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

#[test]
fn a_recorded_member_recreated_with_an_identical_tree_is_carried() {
    for grown in [true, false] {
        recreated_member_is_carried(grown);
    }
}

/// A cut recording feat/alpha, then alpha recreated as feat/alpha2: a new
/// commit with alpha's exact tree and no ancestry or change id back to it (a
/// history rebuilt to strip trailers, say). An edit moves the member onto it by
/// `--from`; when `grown`, a later edit advances it past a commit that changes
/// alpha's file, so replaying alpha's diff onto the cut conflicts (the r12
/// shape), else the twin is the parent itself (the r11 shape).
fn recreated_member_is_carried(grown: bool) {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let alpha = commit_at(&lab, "feat/alpha");
    lab.branch("feat/alpha2", "alpha.txt", "alpha\n");
    let twin = commit_at(&lab, "feat/alpha2");
    assert_ne!(twin, alpha);
    let tree = |commit: &knives::ids::CommitId| {
        let output = std::process::Command::new("git")
            .args(["rev-parse", &format!("{}^{{tree}}", commit.as_str())])
            .current_dir(&lab.work)
            .output()
            .expect("git rev-parse");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    assert_ne!(tree(&alpha), "");
    assert_eq!(tree(&twin), tree(&alpha));
    let moved = knives_release(
        &lab,
        &home,
        &["advance", "feat/alpha2", "--from", alpha.as_str()],
    );
    assert!(moved.status.success(), "{moved:?}");
    if grown {
        lab::extend_branch(&lab, "feat/alpha2", "alpha.txt", "alpha\nmore\n");
        let advanced = knives_release(&lab, &home, &["advance", "feat/alpha2"]);
        assert!(advanced.status.success(), "{advanced:?}");
    }

    // When: the next cut is held against the recorded composition.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: alpha is carried by its identical-tree twin, said so, and the cut
    // lands without --allow-drop.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!(
            "demo: feat/alpha@{} is carried by {}, which has the same tree\n",
            alpha.short(),
            twin.short()
        )),
        "grown={grown}: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("cut release/2026-08-05 as "),
        "grown={grown}: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("refusing to cut"),
        "grown={grown}: {stdout}"
    );
}
