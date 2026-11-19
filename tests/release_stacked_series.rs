//! A stacked release series: `stacked = true` on a series table.
//!
//! The next release of such a series is one merge of its previous release
//! and its new members. The previous release is the first parent, and it is
//! never a member. A member may stack on it, and `members --verify` measures
//! each member from its fork point with it. A release made as a chain of
//! merges, one member each, reads as its previous release and the side
//! parents on its first-parent line. A cut starts the next release as one
//! commit on the previous release, `include` adds each member, the ledger
//! labels the previous release, and the reap keeps every release the live
//! one descends from. A previous release made by hand, with no recorded
//! write, is still the previous release once the reap has forgotten its
//! refs: the cut's event labels it. A cut whose start does not hold the
//! newest recorded write on its first-parent line, as on a machine whose
//! refs stop at an older release, is refused naming that write, and
//! `--allow-drop` states it. `adopt` measures a hand resolution against the
//! merge of the previous release and the members, and counts only the
//! members' lines. A series without the key keeps the unstacked rules: a
//! branch built on the previous release is refused.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::ledger::{Entry, Kind, RecordedParent};
use lab::{Lab, commit_at, extend_branch, knives_release, release_parents, release_test_home};

/// The series' first cut: the previous release of the release in hand.
const PREVIOUS: &str = "release/old/2026-08-04";

/// The series' next release, made by hand on [`PREVIOUS`], or cut.
const NEXT: &str = "release/old/2026-08-05";

/// The release cut on [`NEXT`] once that one is published.
const THIRD: &str = "release/old/2026-08-06";

/// Whether `revision` resolves in the lab's work checkout.
fn resolves(lab: &Lab, revision: &str) -> bool {
    knives::jj::Repo::open(lab.work_path())
        .expect("open work checkout")
        .resolve_commit(revision)
        .is_ok()
}

/// A lab whose upstream carries a second base, `old`, split from `main`: the
/// seed is their common ancestor, and each has moved on since.
fn two_base_lab() -> Lab {
    let lab = Lab::new();
    lab.upstream_branch("old", "old.txt", "old base\n");
    lab.advance_upstream("new base\n");
    lab
}

/// A branch forked from `base`, with its bookmark on its one commit.
fn branch_on(lab: &Lab, base: &str, name: &str, file: &str) {
    lab.jj_work(["new", "-r", base, "-m", name]);
    std::fs::write(lab.work_path().join(file), format!("{name}\n")).expect("write branch");
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// The release-test home with the registry entry given a series `old` on the
/// upstream branch `old`, plus whatever `extra` the test adds to that table.
fn series_home(lab: &Lab, extra: &str) -> tempfile::TempDir {
    let (home, _consumer) = release_test_home(lab);
    let registry = home.path().join("repos.toml");
    let mut text = std::fs::read_to_string(&registry).expect("read registry");
    text.push_str("\n[repos.demo.series.old]\nbase = \"old\"\n");
    text.push_str(extra);
    std::fs::write(&registry, text).expect("write registry");
    home
}

fn text(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Series `old`, with `stacked = true` when `stacked`: its first cut
/// [`PREVIOUS`] of `fix/a` and `fix/b`, pushed to origin and marked published
/// by a `record: published` note, and three branches on that cut
/// ([`branches_on_previous`]).
fn previous_release_lab(stacked: bool) -> (Lab, tempfile::TempDir) {
    series_lab(stacked, true)
}

/// [`previous_release_lab`], the first cut marked published only when
/// `published`: otherwise no tag, note or pin marks it.
fn series_lab(stacked: bool, published: bool) -> (Lab, tempfile::TempDir) {
    let lab = two_base_lab();
    branch_on(&lab, "old@upstream", "fix/a", "a.txt");
    branch_on(&lab, "old@upstream", "fix/b", "b.txt");
    let home = series_home(&lab, if stacked { "stacked = true\n" } else { "" });
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", PREVIOUS]);
    assert!(cut.status.success(), "{}", text(&cut));
    lab.push_branch(PREVIOUS);
    lab.fetch_work();
    if published {
        mark_published(&lab, &home, PREVIOUS);
    }
    branches_on_previous(&lab);
    (lab, home)
}

/// Three branches on [`PREVIOUS`]: `del` deletes `a.txt`, which that
/// release carries, and `m6` and `m5` each add a file.
fn branches_on_previous(lab: &Lab) {
    lab.jj_work(["new", "-r", PREVIOUS, "-m", "del"]);
    std::fs::remove_file(lab.work_path().join("a.txt")).expect("delete a file the cut carries");
    lab.jj_work(["bookmark", "create", "del", "-r", "@"]);
    lab.jj_work(["new"]);
    branch_on(lab, PREVIOUS, "m6", "m6.txt");
    branch_on(lab, PREVIOUS, "m5", "m5.txt");
}

/// A `record: published` note on release `name`, naming its commit, as the
/// release owner writes one.
fn mark_published(lab: &Lab, home: &tempfile::TempDir, name: &str) {
    let commit = commit_at(lab, name);
    let noted = lab::knives(
        lab,
        home,
        &[
            "notch",
            name,
            "-m",
            "record: published as v1.0.0",
            "--evidence",
            commit.as_str(),
            "--repo",
            "demo",
        ],
    );
    assert!(noted.status.success(), "{}", text(&noted));
}

/// `knives release --series old <args>`, which must exit 0.
fn release_ok(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let mut full = vec!["--series", "old"];
    full.extend_from_slice(args);
    let output = knives_release(lab, home, &full);
    assert_eq!(output.status.code(), Some(0), "{args:?}: {}", text(&output));
    output
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

/// Whether `from` and `to` hold the same tree in the work checkout.
fn same_tree(lab: &Lab, from: &str, to: &str) -> bool {
    knives::jj::changed_files_between(lab.work_path(), from, to)
        .expect("diff two revisions")
        .is_empty()
}

/// Whether `commit` is visible in the work checkout: not abandoned.
fn is_visible(lab: &Lab, commit: &knives::ids::CommitId) -> bool {
    knives::jj::commits_matching(lab.work_path(), "all()")
        .expect("list visible commits")
        .contains(commit)
}

/// Whether any ref of `name`, local or on a remote, is left in the work
/// checkout.
fn has_refs(lab: &Lab, name: &str) -> bool {
    knives::jj::Repo::open(lab.work_path())
        .expect("open work checkout")
        .bookmark_tips()
        .expect("read bookmark tips")
        .keys()
        .any(|reference| reference.branch().as_str() == name)
}

/// [`NEXT`] made by hand as one merge of [`PREVIOUS`] and the three
/// branches on it: the shape a stacked cut of them has.
fn flat_next(lab: &Lab) {
    lab.jj_work([
        "new", "-r", PREVIOUS, "-r", "del", "-r", "m6", "-r", "m5", "-m", NEXT,
    ]);
    lab.jj_work(["bookmark", "create", NEXT, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// [`NEXT`] made by hand as the compose script makes a release: the chain
/// `merge(merge(merge(PREVIOUS, del), m6), m5)`.
fn chain_next(lab: &Lab) {
    lab.jj_work(["new", "-r", PREVIOUS, "-r", "del", "-m", "compose del"]);
    lab.jj_work(["new", "-r", "@", "-r", "m6", "-m", "compose m6"]);
    lab.jj_work(["new", "-r", "@", "-r", "m5", "-m", NEXT]);
    lab.jj_work(["bookmark", "create", NEXT, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// The row `members` prints for `branch`'s commit, held by its bookmark.
fn member_row(lab: &Lab, branch: &str) -> String {
    format!("- {} {branch}", commit_at(lab, branch).short())
}

/// The row `members` prints for the previous release.
fn previous_row(lab: &Lab) -> String {
    format!(
        "- {} {PREVIOUS} previous release (not a member)",
        commit_at(lab, PREVIOUS).short()
    )
}

#[test]
fn the_plan_of_a_stacked_release_reports_no_stacked_member_and_exits_0() {
    // Given: a stacked series whose release in hand is one merge of its
    // previous release and three branches built on that release.
    let (lab, home) = previous_release_lab(true);
    flat_next(&lab);

    // When: the series plans.
    let plan = knives_release(&lab, &home, &["--series", "old"]);

    // Then: the previous release has a row of its own, no member reads as
    // stacked on a prior merge or is told to rebase off the trunk, and the
    // plan exits 0.
    let out = text(&plan);
    assert_eq!(plan.status.code(), Some(0), "{out}");
    assert!(out.contains(&format!("demo: {NEXT}")), "{out}");
    assert!(out.contains("4 parent(s), flat"), "{out}");
    assert!(
        out.contains(&format!(
            "    {}  {PREVIOUS} previous release (not a member)",
            commit_at(&lab, PREVIOUS).short()
        )),
        "{out}"
    );
    assert!(!out.contains("stacked on a prior merge"), "{out}");
    assert!(!out.contains("rebase it off the trunk"), "{out}");
    assert!(
        out.contains("every parent is still its branch tip"),
        "{out}"
    );
}

#[test]
fn the_plan_before_a_stacked_cut_reads_branches_on_the_previous_release_as_fit() {
    // Given: a stacked series whose release in hand is its published first
    // cut, the previous release of its next cut, with `del`, `m6` and `m5`
    // built on it.
    let (lab, home) = previous_release_lab(true);

    // When: the series plans.
    let plan = knives_release(&lab, &home, &["--series", "old"]);

    // Then: the plan exits 0, each branch reads as one the next cut
    // includes, and none is told to rebase off the trunk.
    let out = text(&plan);
    assert_eq!(plan.status.code(), Some(0), "{out}");
    assert!(!out.contains("rebase it off the trunk"), "{out}");
    for branch in ["del", "m6", "m5"] {
        assert!(
            out.contains(&format!(
                "{branch} is built on {PREVIOUS}, which the next cut of this stacked series"
            )),
            "{out}"
        );
    }

    // And: a branch that merges two of them carries a merge past the release
    // in hand, so it is still told so, the merge named.
    lab.jj_work(["new", "-r", "m6", "-r", "m5", "-m", "merged"]);
    lab.jj_work(["bookmark", "create", "merged", "-r", "@"]);
    lab.jj_work(["new"]);
    let merge = commit_at(&lab, "merged");
    let plan = knives_release(&lab, &home, &["--series", "old"]);
    let out = text(&plan);
    assert!(
        out.contains("branch merged's history past the trunk carries 1 merge commit(s)"),
        "{out}"
    );
    assert!(out.contains(merge.short()), "{out}");
    assert!(
        out.contains("rebase it off the trunk before including it"),
        "{out}"
    );
}

#[test]
fn members_verify_reads_a_stacked_release_with_no_missing_member() {
    // Given: the same stacked release, whose member `del` deletes a file the
    // previous release carries.
    let (lab, home) = previous_release_lab(true);
    flat_next(&lab);

    // When: its members' content is replayed onto it.
    let members = knives_release(&lab, &home, &["--series", "old", "members", "--verify"]);

    // Then: the previous release is no member, each member replays empty
    // from its fork point with that release, and nothing is missing or
    // unexplained.
    let out = text(&members);
    assert_eq!(members.status.code(), Some(0), "{out}");
    assert!(out.contains(&previous_row(&lab)), "{out}");
    for branch in ["del", "m6", "m5"] {
        assert!(out.contains(&member_row(&lab, branch)), "{out}");
        assert!(
            out.contains(&format!(
                "  {branch}@{}: carried (replay empty)",
                commit_at(&lab, branch).short()
            )),
            "{out}"
        );
    }
}

#[test]
fn members_verify_reads_a_chain_shaped_release_as_its_previous_release_and_side_parents() {
    // Given: the next release made as a chain of merges, one member each,
    // as the compose script makes it.
    let (lab, home) = previous_release_lab(true);
    chain_next(&lab);

    // When: its members' content is replayed onto it.
    let members = knives_release(&lab, &home, &["--series", "old", "members", "--verify"]);

    // Then: it has two parents, and reads as its previous release, on a row
    // of its own, and the side parents on its first-parent line, none of
    // them missing.
    let out = text(&members);
    assert_eq!(members.status.code(), Some(0), "{out}");
    assert!(out.contains(&format!("{NEXT} @ ")), "{out}");
    assert!(out.contains(" — 2 parents"), "{out}");
    assert!(out.contains(&previous_row(&lab)), "{out}");
    for branch in ["m5", "m6", "del"] {
        assert!(out.contains(&member_row(&lab, branch)), "{out}");
        assert!(
            out.contains(&format!(
                "  {branch}@{}: carried (replay empty)",
                commit_at(&lab, branch).short()
            )),
            "{out}"
        );
    }
}

#[test]
fn without_stacked_an_include_stacked_on_the_previous_release_is_refused() {
    // Given: the same series and branches without the stacked key, and the
    // next release cut: it starts identical to the published first cut,
    // which is now its previous release.
    let (lab, home) = previous_release_lab(false);
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", NEXT]);
    assert!(resolves(&lab, NEXT), "{}", text(&cut));

    // When: a branch built on the previous release is included.
    let include = knives_release(&lab, &home, &["--series", "old", "include", "del"]);

    // Then: the unstacked rule refuses it: its history past the trunk
    // carries the previous release's merge.
    let out = text(&include);
    assert_eq!(include.status.code(), Some(3), "{out}");
    assert!(out.contains("rebase it off the trunk"), "{out}");
}

#[test]
fn a_stacked_cut_starts_on_the_previous_release() {
    // Given: a stacked series whose published first cut is the previous
    // release.
    let (lab, home) = previous_release_lab(true);
    let previous = commit_at(&lab, PREVIOUS);

    // When: the next release is cut.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", NEXT]);

    // Then: it exits 0 and starts as one commit on the previous release
    // with that release's tree, the composition to edit.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(0), "{out}");
    assert_eq!(release_parents(&lab, NEXT), vec![previous.clone()], "{out}");
    assert!(same_tree(&lab, previous.as_str(), NEXT), "{out}");
    assert!(
        out.contains(&format!("{NEXT} starts identical to {PREVIOUS}")),
        "{out}"
    );
    assert!(out.contains("is the composition to edit"), "{out}");
}

#[test]
fn a_stacked_cut_on_an_unpublished_previous_release_is_refused_as_identical() {
    // Given: a stacked series whose first cut is on the publish remote with
    // no tag, no `record: published` note and no pin.
    let (lab, home) = series_lab(true, false);
    let recorded = ledger_entries(&home).len();

    // When: the next release is cut.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", NEXT]);

    // Then: it would start identical to the first cut, which can still be
    // edited in place, so the cut refuses, says to include into it, and
    // writes nothing.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(out.contains("nothing to cut"), "{out}");
    assert!(out.contains("`knives release include <branch>`"), "{out}");
    assert!(!resolves(&lab, NEXT), "{out}");
    assert!(resolves(&lab, PREVIOUS), "{out}");
    assert_eq!(ledger_entries(&home).len(), recorded, "{out}");
}

#[test]
fn includes_on_a_stacked_cut_make_one_merge_of_the_previous_release_and_its_members() {
    // Given: the next release cut on the published previous release.
    let (lab, home) = previous_release_lab(true);
    let previous = commit_at(&lab, PREVIOUS);
    release_ok(&lab, &home, &["cut", NEXT]);

    // When: each branch built on the previous release is included.
    for branch in ["del", "m6", "m5"] {
        release_ok(&lab, &home, &["include", branch]);
    }

    // Then: the release is one merge of the previous release and the three,
    // in that order, every member replays from its fork point with the
    // previous release, and its tree is the one jj's own merge of them has.
    let mut expected = vec![previous.clone()];
    expected.extend(["del", "m6", "m5"].map(|branch| commit_at(&lab, branch)));
    assert_eq!(release_parents(&lab, NEXT), expected);
    let members = text(&release_ok(&lab, &home, &["members", "--verify"]));
    for branch in ["del", "m6", "m5"] {
        assert!(
            members.contains(&format!(
                "  {branch}@{}: carried (replay empty)",
                commit_at(&lab, branch).short()
            )),
            "{members}"
        );
    }
    lab.jj_work([
        "new",
        "-r",
        previous.as_str(),
        "-r",
        "del",
        "-r",
        "m6",
        "-r",
        "m5",
        "-m",
        "merged by jj",
    ]);
    lab.jj_work(["bookmark", "create", "merged-by-jj", "-r", "@"]);
    assert!(same_tree(&lab, "merged-by-jj", NEXT));
}

#[test]
fn the_previous_release_is_never_dropped_or_advanced_and_rebase_refuses() {
    // Given: the next release with `del` included, and a branch `later`
    // built on the previous release, which ancestry alone would read as
    // that release's successor.
    let (lab, home) = previous_release_lab(true);
    let previous = commit_at(&lab, PREVIOUS);
    release_ok(&lab, &home, &["cut", NEXT]);
    release_ok(&lab, &home, &["include", "del"]);
    branch_on(&lab, previous.as_str(), "later", "later.txt");
    let held = commit_at(&lab, NEXT);
    let series = |args: &[&str]| {
        let mut full = vec!["--series", "old"];
        full.extend_from_slice(args);
        knives_release(&lab, &home, &full)
    };

    // When: the previous release is dropped, every member is advanced,
    // `later` is advanced by name and from the previous release, and the
    // release is rebased.
    let dropped = series(&["drop", previous.as_str(), "--why", "not a member"]);
    let advance = series(&["advance"]);
    let named = series(&["advance", "later"]);
    let from = series(&["advance", "later", "--from", previous.as_str()]);
    let rebase = series(&["rebase"]);

    // Then: the drop and the rebase refuse, no advance moves the previous
    // release, and the release is as it was.
    assert_eq!(dropped.status.code(), Some(3), "{}", text(&dropped));
    assert!(
        text(&dropped).contains("the previous release"),
        "{}",
        text(&dropped)
    );
    assert_eq!(advance.status.code(), Some(0), "{}", text(&advance));
    assert!(
        text(&advance).contains("is at its branch tip"),
        "{}",
        text(&advance)
    );
    assert_eq!(named.status.code(), Some(3), "{}", text(&named));
    assert_eq!(from.status.code(), Some(3), "{}", text(&from));
    assert!(
        text(&from).contains("no advance moves it"),
        "{}",
        text(&from)
    );
    assert_eq!(rebase.status.code(), Some(3), "{}", text(&rebase));
    assert!(text(&rebase).contains("is stacked"), "{}", text(&rebase));
    assert_eq!(commit_at(&lab, NEXT), held);
    assert_eq!(
        release_parents(&lab, NEXT),
        vec![previous, commit_at(&lab, "del")]
    );
}

#[test]
fn a_member_with_a_merge_past_the_previous_release_is_refused() {
    // Given: the next release cut, and a branch that merges two branches
    // built on the previous release.
    let (lab, home) = previous_release_lab(true);
    release_ok(&lab, &home, &["cut", NEXT]);
    lab.jj_work(["new", "-r", "m6", "-r", "m5", "-m", "merged"]);
    lab.jj_work(["bookmark", "create", "merged", "-r", "@"]);
    lab.jj_work(["new"]);
    let merge = commit_at(&lab, "merged");
    let held = commit_at(&lab, NEXT);

    // When: that branch is included.
    let include = knives_release(&lab, &home, &["--series", "old", "include", "merged"]);

    // Then: its merge past the previous release is refused and named, and
    // the release is as it was.
    let out = text(&include);
    assert_eq!(include.status.code(), Some(3), "{out}");
    assert!(out.contains("carries 1 merge commit(s)"), "{out}");
    assert!(out.contains(merge.short()), "{out}");
    assert_eq!(commit_at(&lab, NEXT), held, "{out}");
}

#[test]
fn the_reap_after_a_stacked_cut_keeps_every_release_the_live_one_descends_from() {
    // Given: the first stacked cut, with `del` included, pushed and
    // published.
    let (lab, home) = previous_release_lab(true);
    let previous = commit_at(&lab, PREVIOUS);
    release_ok(&lab, &home, &["cut", NEXT]);
    release_ok(&lab, &home, &["include", "del"]);
    lab.push_branch(NEXT);
    lab.fetch_work();
    mark_published(&lab, &home, NEXT);
    let first = commit_at(&lab, NEXT);

    // When: a second stacked cut is made on it.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", THIRD]);

    // Then: the cut and its reap exit 0. The first stacked cut's name is
    // forgotten here and on origin's tracking ref, its commit and the
    // previous release's stay visible under the new cut, and nothing is
    // abandoned.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "reaped {NEXT} (refs forgotten everywhere; commit kept, {THIRD} descends from it)"
        )),
        "{out}"
    );
    assert!(!out.contains("abandoned"), "{out}");
    assert!(!has_refs(&lab, NEXT), "{out}");
    assert!(!has_refs(&lab, PREVIOUS), "{out}");
    assert!(is_visible(&lab, &first), "{out}");
    assert!(is_visible(&lab, &previous), "{out}");
    assert_eq!(release_parents(&lab, THIRD), vec![first], "{out}");
}

#[test]
fn a_stacked_cut_on_an_unrecorded_previous_release_takes_its_members() {
    // Given: a stacked series whose previous release was made by hand, as
    // the fork's 9.2 releases up to 8.0.9 were: one merge of `fix/a` and
    // `fix/b` that no knives cut wrote, pushed to origin and published by a
    // tag on its commit. `del`, `m6` and `m5` are built on it.
    let lab = two_base_lab();
    branch_on(&lab, "old@upstream", "fix/a", "a.txt");
    branch_on(&lab, "old@upstream", "fix/b", "b.txt");
    let home = series_home(&lab, "stacked = true\n");
    lab.jj_work(["new", "-r", "fix/a", "-r", "fix/b", "-m", PREVIOUS]);
    lab.jj_work(["bookmark", "create", PREVIOUS, "-r", "@"]);
    lab.jj_work(["new"]);
    lab.push_branch(PREVIOUS);
    lab.fetch_work();
    lab.jj_work(["tag", "set", "v1.0.0", "-r", PREVIOUS]);
    let previous = commit_at(&lab, PREVIOUS);
    branches_on_previous(&lab);

    // When: the next release is cut.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", NEXT]);

    // Then: it exits 0 and starts as one commit on the previous release.
    // The ledger records no write of the previous release, and the reap
    // forgets every ref of it and keeps its commit.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(0), "{out}");
    assert_eq!(release_parents(&lab, NEXT), vec![previous.clone()], "{out}");
    assert!(
        knives::release_model::recorded_writes(&ledger_entries(&home), PREVIOUS)
            .next()
            .is_none(),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "reaped {PREVIOUS} (refs forgotten everywhere; commit kept, {NEXT} descends from it)"
        )),
        "{out}"
    );
    assert!(!has_refs(&lab, PREVIOUS), "{out}");

    // When: each branch built on the previous release is included.
    for branch in ["del", "m6", "m5"] {
        release_ok(&lab, &home, &["include", branch]);
    }

    // Then: the release is one merge of the previous release and the three,
    // in that order.
    let mut expected = vec![previous.clone()];
    expected.extend(["del", "m6", "m5"].map(|branch| commit_at(&lab, branch)));
    assert_eq!(release_parents(&lab, NEXT), expected);

    // And: `members --verify` exits 0. It shows the previous release on a
    // row of its own, under its name, though no ref holds it now, then each
    // member, and nothing is missing.
    let members = release_ok(&lab, &home, &["members", "--verify"]);
    let out = text(&members);
    let own_row = format!(
        "- {} {PREVIOUS} previous release (not a member)",
        previous.short()
    );
    assert!(out.lines().any(|line| line == own_row), "{out}");
    for branch in ["del", "m6", "m5"] {
        assert!(out.contains(&member_row(&lab, branch)), "{out}");
    }
}

#[test]
fn the_ledger_event_of_a_stacked_cut_labels_the_previous_release() {
    // Given: a stacked series whose published first cut is the previous
    // release.
    let (lab, home) = previous_release_lab(true);
    let previous = commit_at(&lab, PREVIOUS);
    let labelled = RecordedParent {
        commit: previous.as_str().to_owned(),
        branches: vec![format!("{PREVIOUS} previous release (not a member)")],
    };

    // When: the next release is cut, and then a member is included.
    release_ok(&lab, &home, &["cut", NEXT]);
    let cut = newest_event(&home, NEXT);
    let start = commit_at(&lab, NEXT);
    release_ok(&lab, &home, &["include", "del"]);
    let edit = newest_event(&home, NEXT);

    // Then: each event records the previous release after the commit in
    // its evidence and first in its parents, under the label, and records
    // as members only what was included.
    assert!(cut.text.starts_with(&format!("cut {NEXT} as ")), "{cut:?}");
    assert!(
        cut.text.contains("previous release (not a member)"),
        "{cut:?}"
    );
    assert_eq!(
        cut.evidence,
        vec![start.as_str().to_owned(), previous.as_str().to_owned()],
        "{cut:?}"
    );
    assert_eq!(cut.parents, vec![labelled.clone()], "{cut:?}");
    let del = commit_at(&lab, "del");
    assert!(
        edit.text.starts_with(&format!("edited {NEXT} as ")),
        "{edit:?}"
    );
    assert_eq!(
        edit.evidence,
        vec![
            commit_at(&lab, NEXT).as_str().to_owned(),
            previous.as_str().to_owned(),
            del.as_str().to_owned()
        ],
        "{edit:?}"
    );
    assert_eq!(
        edit.parents,
        vec![
            labelled,
            RecordedParent {
                commit: del.as_str().to_owned(),
                branches: vec!["del".to_owned()],
            }
        ],
        "{edit:?}"
    );
}

#[test]
fn a_hand_resolution_on_a_stacked_release_is_adopted_and_names_its_files() {
    // Given: the next release cut on the published previous release, with
    // `del`, `m6` and `m5` included. `del` deletes a.txt, which the previous
    // release carries; `m6` and `m5` each keep its one line and add a line of
    // their own after it, so the includes leave a.txt conflicted. A person
    // resolves the file on the release commit as `del` asks: the previous
    // release's line goes, and each member's own line stays.
    let (lab, home) = previous_release_lab(true);
    let previous = commit_at(&lab, PREVIOUS);
    extend_branch(&lab, "m6", "a.txt", "fix/a\nm6\n");
    extend_branch(&lab, "m5", "a.txt", "fix/a\nm5\n");
    release_ok(&lab, &home, &["cut", NEXT]);
    for branch in ["del", "m6", "m5"] {
        release_ok(&lab, &home, &["include", branch]);
    }
    assert_eq!(
        knives::jj::conflicted_files(lab.work_path(), NEXT).expect("list conflicts"),
        ["a.txt"]
    );
    lab.jj_work(["edit", NEXT]);
    std::fs::write(lab.work_path().join("a.txt"), "m6\nm5\n").expect("write the resolution");
    lab.jj_work(["new"]);
    let resolved = commit_at(&lab, NEXT);

    // When: the release is adopted, with a reason.
    let adopted = release_ok(
        &lab,
        &home,
        &[
            "adopt",
            "--why",
            "kept each member's line of the file del deletes",
        ],
    );

    // Then: it exits 0 and names the file resolved by hand. Each member's
    // lines are counted from its fork point with the previous release, so
    // the previous release's line that `del` deleted is no lost line of `m6`
    // or `m5`, and `del` added no line to lose.
    let out = text(&adopted);
    assert!(
        out.contains("1 file(s) resolved by hand:\n    a.txt\n"),
        "{out}"
    );
    for member in ["m6", "m5"] {
        assert!(
            out.contains(&format!(
                "\n  {member}: 0 of 1 added line(s) of a.txt absent from the resolution\n"
            )),
            "{out}"
        );
    }
    assert_eq!(
        out.matches("absent from the resolution").count(),
        2,
        "{out}"
    );

    // And: the event names the file, and records the previous release
    // first, under its label, then the members, as every write of the
    // release records them.
    let event = newest_event(&home, NEXT);
    assert!(
        event
            .text
            .starts_with(&format!("edited {NEXT} as {} ", resolved.short())),
        "{event:?}"
    );
    assert!(
        event.text.contains("; resolved by hand: a.txt; "),
        "{event:?}"
    );
    assert!(
        event
            .text
            .contains("; added lines absent: m6 0 of 1 in a.txt, m5 0 of 1 in a.txt; "),
        "{event:?}"
    );
    let members = ["del", "m6", "m5"].map(|branch| (branch, commit_at(&lab, branch)));
    let mut evidence = vec![resolved.as_str().to_owned(), previous.as_str().to_owned()];
    evidence.extend(members.iter().map(|(_, commit)| commit.as_str().to_owned()));
    assert_eq!(event.evidence, evidence, "{event:?}");
    let mut parents = vec![RecordedParent {
        commit: previous.as_str().to_owned(),
        branches: vec![format!("{PREVIOUS} previous release (not a member)")],
    }];
    parents.extend(members.iter().map(|(branch, commit)| RecordedParent {
        commit: commit.as_str().to_owned(),
        branches: vec![(*branch).to_owned()],
    }));
    assert_eq!(event.parents, parents, "{event:?}");
}

/// The release cut on [`THIRD`], with `z` included.
const FOURTH: &str = "release/old/2026-08-07";

/// The release cut on [`FOURTH`], a start with no member.
const FIFTH: &str = "release/old/2026-08-08";

/// The release cut after [`FIFTH`].
const SIXTH: &str = "release/old/2026-08-09";

/// A stacked series whose ledger records NEXT with `del` included, THIRD cut
/// on it as a start, FOURTH cut on THIRD with `z` included, and FIFTH cut on
/// FOURTH as a start, each published by a note; while this machine's release
/// refs stop at THIRD, as on a machine that never fetched FOURTH or FIFTH:
/// the reap after each cut forgot the refs of the release below it, FIFTH's
/// bookmark is forgotten here, and THIRD's is made again at its commit.
/// Returns the commit of THIRD and the commit of FIFTH's cut, the newest
/// recorded write.
fn third_behind_fifth_lab() -> (
    Lab,
    tempfile::TempDir,
    knives::ids::CommitId,
    knives::ids::CommitId,
) {
    let (lab, home) = previous_release_lab(true);
    release_ok(&lab, &home, &["cut", NEXT]);
    release_ok(&lab, &home, &["include", "del"]);
    mark_published(&lab, &home, NEXT);
    release_ok(&lab, &home, &["cut", THIRD]);
    mark_published(&lab, &home, THIRD);
    let third = commit_at(&lab, THIRD);
    branch_on(&lab, THIRD, "z", "z.txt");
    release_ok(&lab, &home, &["cut", FOURTH]);
    release_ok(&lab, &home, &["include", "z"]);
    mark_published(&lab, &home, FOURTH);
    release_ok(&lab, &home, &["cut", FIFTH]);
    mark_published(&lab, &home, FIFTH);
    let fifth = commit_at(&lab, FIFTH);
    lab.jj_work(["bookmark", "forget", FIFTH]);
    lab.jj_work(["bookmark", "set", THIRD, "-r", third.as_str()]);
    assert!(!has_refs(&lab, FOURTH) && !has_refs(&lab, FIFTH));
    (lab, home, third, fifth)
}

#[test]
fn a_stacked_cut_on_a_release_older_than_the_newest_recorded_write_is_refused_naming_it() {
    // Given: the ledger records FOURTH with `z` and FIFTH on it, and this
    // machine's newest release ref is THIRD.
    let (lab, home, third, fifth) = third_behind_fifth_lab();
    let recorded = ledger_entries(&home).len();

    // When: the next release is cut.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", SIXTH]);

    // Then: it is refused, naming FIFTH's newest recorded write, a cut, and
    // its commit, and saying to fetch and move FIFTH to it; nothing is cut,
    // so `z` and FIFTH are not dropped by a cut that starts on THIRD.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: this cut does not have {FIFTH}'s newest recorded write (a \
             cut) {} on its first-parent line, so it does not carry {FIFTH}\n",
            fifth.short()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "  the candidate was discarded; fetch, then move {FIFTH} to {}, its newest recorded \
             write: `jj bookmark set {FIFTH} -r {} --allow-backwards`, or re-run with \
             --allow-drop to state the drop is intended\n",
            fifth.short(),
            fifth.short()
        )),
        "{out}"
    );
    assert!(!resolves(&lab, SIXTH), "{out}");
    assert_eq!(ledger_entries(&home).len(), recorded, "{out}");
    assert_eq!(commit_at(&lab, THIRD), third, "{out}");

    // When: FIFTH is moved to that write, as the refusal says, and the next
    // release is cut again.
    lab.jj_work([
        "bookmark",
        "set",
        FIFTH,
        "-r",
        fifth.short(),
        "--allow-backwards",
    ]);
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", SIXTH]);

    // Then: it starts on FIFTH's write.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(0), "{out}");
    assert_eq!(release_parents(&lab, SIXTH), vec![fifth], "{out}");
}

#[test]
fn a_stacked_cut_off_the_newest_recorded_write_goes_ahead_under_allow_drop_and_says_so() {
    // Given: the ledger records FOURTH with `z` and FIFTH on it, and this
    // machine's newest release ref is THIRD.
    let (lab, home, third, fifth) = third_behind_fifth_lab();

    // When: the next release is cut, the drop stated.
    let cut = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", SIXTH, "--allow-drop"],
    );

    // Then: it starts on THIRD, and both its line and its event name FIFTH's
    // newest recorded write, its release and its commit.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(0), "{out}");
    assert!(
        out.contains(&format!(
            "demo: --allow-drop: cutting without {FIFTH}'s newest recorded write (a cut) {} on \
             its first-parent line\n",
            fifth.short()
        )),
        "{out}"
    );
    assert_eq!(release_parents(&lab, SIXTH), vec![third], "{out}");
    let event = newest_event(&home, SIXTH);
    assert!(
        event.text.contains(&format!(
            "; previous cut {FIFTH} recorded 0 member(s); all carried; not built on {FIFTH}'s \
             newest recorded write (a cut) {}",
            fifth.short()
        )),
        "{event:?}"
    );
}

#[test]
fn a_stacked_cut_on_an_older_write_of_the_newest_release_names_its_member_and_its_write() {
    // Given: NEXT cut on the published previous release and `m6` included,
    // an edit; then NEXT's bookmark moved back to its cut, a start with no
    // member, as a machine that never fetched the edit still has it, and
    // that start published by a note.
    let (lab, home) = previous_release_lab(true);
    release_ok(&lab, &home, &["cut", NEXT]);
    let start = commit_at(&lab, NEXT);
    release_ok(&lab, &home, &["include", "m6"]);
    let edit = commit_at(&lab, NEXT);
    lab.jj_work([
        "bookmark",
        "set",
        NEXT,
        "-r",
        start.as_str(),
        "--allow-backwards",
    ]);
    mark_published(&lab, &home, NEXT);
    let recorded = ledger_entries(&home).len();

    // When: the next release is cut.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", THIRD]);

    // Then: it is refused, naming `m6`, which only NEXT's newest write
    // holds, and that write, an edit off the start's first-parent line, and
    // saying to fetch and move NEXT to it; nothing is cut.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: this cut does not carry 1 member(s) which {NEXT}'s newest \
             recorded write (an edit) holds:\n    m6@{}\n",
            commit_at(&lab, "m6").short()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: this cut does not have {NEXT}'s newest recorded write (an \
             edit) {} on its first-parent line, so it does not carry {NEXT}\n",
            edit.short()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "  the candidate was discarded; fetch, then move {NEXT} to {}, its newest recorded \
             write: `jj bookmark set {NEXT} -r {} --allow-backwards`, or re-run with \
             --allow-drop to state the drop is intended\n",
            edit.short(),
            edit.short()
        )),
        "{out}"
    );
    assert!(!resolves(&lab, THIRD), "{out}");
    assert_eq!(ledger_entries(&home).len(), recorded, "{out}");
}

#[test]
fn a_stacked_first_cut_with_no_release_ref_here_is_refused_naming_the_newest_recorded_write() {
    // Given: NEXT cut on the published previous release, a start whose cut
    // is the newest recorded write; the branches built on the previous
    // release removed; and NEXT's bookmark forgotten, so this machine holds
    // no release ref of the series, as one that never fetched it.
    let (lab, home) = previous_release_lab(true);
    release_ok(&lab, &home, &["cut", NEXT]);
    let start = commit_at(&lab, NEXT);
    lab.jj_work(["bookmark", "delete", "del", "m6", "m5"]);
    lab.jj_work(["bookmark", "forget", NEXT]);
    assert!(!has_refs(&lab, PREVIOUS) && !has_refs(&lab, NEXT));
    let recorded = ledger_entries(&home).len();

    // When: the next release is cut, which with no release here reads as the
    // series' first cut of its branches.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", THIRD]);

    // Then: it is refused, naming NEXT's newest recorded write, a cut, and
    // its commit, and saying to fetch and move NEXT to it; nothing is cut.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: this cut does not have {NEXT}'s newest recorded write (a \
             cut) {} on its first-parent line, so it does not carry {NEXT}\n",
            start.short()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "  the candidate was discarded; fetch, then move {NEXT} to {}, its newest recorded \
             write: `jj bookmark set {NEXT} -r {} --allow-backwards`, or re-run with \
             --allow-drop to state the drop is intended\n",
            start.short(),
            start.short()
        )),
        "{out}"
    );
    assert!(!resolves(&lab, THIRD), "{out}");
    assert_eq!(ledger_entries(&home).len(), recorded, "{out}");
}

/// The stacked series' NEXT, cut on the published PREVIOUS with `m6`
/// included and marked published, then fixed by hand in place (`jj squash`
/// into it, which keeps its change id) and adopted. Returns NEXT's edit,
/// the commit the adopt replaced, and the adopt's commit, NEXT's newest
/// recorded write.
fn adopted_next_lab() -> (
    Lab,
    tempfile::TempDir,
    knives::ids::CommitId,
    knives::ids::CommitId,
) {
    let (lab, home) = previous_release_lab(true);
    release_ok(&lab, &home, &["cut", NEXT]);
    release_ok(&lab, &home, &["include", "m6", "--why", "in NEXT"]);
    mark_published(&lab, &home, NEXT);
    let edit = commit_at(&lab, NEXT);
    lab.jj_work(["new", "-r", NEXT]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["squash", "--into", NEXT]);
    lab.jj_work(["new", "-r", "old@upstream"]);
    release_ok(&lab, &home, &["adopt", NEXT, "--why", "a fix by hand"]);
    let adopted = commit_at(&lab, NEXT);
    assert_ne!(adopted, edit, "the squash rewrote NEXT");
    (lab, home, edit, adopted)
}

#[test]
fn a_record_built_on_the_write_an_adopt_replaced_is_refused() {
    // Given: NEXT's newest recorded write is an adopt of a fix by hand,
    // which kept the change id of the edit it replaced; and a commit merged
    // by hand of that older edit and `m5`, so it lacks the fix.
    let (lab, home, edit, adopted) = adopted_next_lab();
    lab.jj_work(["new", "-r", edit.as_str(), "-r", "m5", "-m", THIRD]);
    lab.jj_work(["new"]);
    let commit = knives::ids::CommitId::new(lab.jj_work_output([
        "log",
        "-r",
        "@-",
        "--no-graph",
        "-T",
        "commit_id",
    ]));
    let entries = ledger_entries(&home);

    // When: it is recorded as the series' third release.
    let recorded = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", THIRD, "--at", commit.as_str()],
    );

    // Then: it is refused, naming the adopt's commit, and nothing is
    // written: the older write shares the adopt's change id, but it is what
    // the adopt replaced, not a rewrite of it, and it lacks the fix.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(out.contains(adopted.short()), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");
}

#[test]
fn a_stacked_cut_on_the_write_an_adopt_replaced_is_refused() {
    // Given: NEXT's newest recorded write is an adopt of a fix by hand, and
    // NEXT's bookmark back at the published edit the adopt replaced, as a
    // machine has it that keeps that edit.
    let (lab, home, edit, adopted) = adopted_next_lab();
    lab.jj_work([
        "bookmark",
        "set",
        NEXT,
        "-r",
        edit.as_str(),
        "--allow-backwards",
    ]);
    let recorded = ledger_entries(&home).len();

    // When: the next release is cut.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", THIRD]);

    // Then: it is refused, naming the adopt's commit, and nothing is cut:
    // the cut would start on the edit and drop the fix.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(out.contains(adopted.short()), "{out}");
    assert!(!resolves(&lab, THIRD), "{out}");
    assert_eq!(ledger_entries(&home).len(), recorded, "{out}");
}

/// [`adopted_next_lab`], then the adopt, NEXT's newest recorded write,
/// copied by `jj duplicate`: the copy has the adopt's parents and tree and a
/// change id of its own. Returns the adopt's commit and the copy's.
fn copied_next_lab() -> (
    Lab,
    tempfile::TempDir,
    knives::ids::CommitId,
    knives::ids::CommitId,
) {
    let (lab, home, _edit, adopted) = adopted_next_lab();
    lab.jj_work(["duplicate", adopted.as_str()]);
    let copies = format!("children(m6) ~ {}", adopted.as_str());
    let copy = lab.jj_work_output([
        "log",
        "-r",
        copies.as_str(),
        "--no-graph",
        "-T",
        "commit_id",
    ]);
    assert!(
        !copy.is_empty() && !copy.contains('\n'),
        "one copy: {copy:?}"
    );
    let copy = knives::ids::CommitId::new(copy);
    assert!(
        same_tree(&lab, adopted.as_str(), copy.as_str()),
        "the copy has the adopt's tree"
    );
    let change = |commit: &knives::ids::CommitId| {
        lab.jj_work_output([
            "log",
            "-r",
            commit.as_str(),
            "--no-graph",
            "-T",
            "change_id",
        ])
    };
    assert_ne!(
        change(&copy),
        change(&adopted),
        "the copy has a change id of its own"
    );
    (lab, home, adopted, copy)
}

#[test]
fn a_record_built_on_a_copy_of_the_newest_write_is_recorded() {
    // Given: NEXT's newest recorded write, an adopt of a fix by hand, copied
    // by `jj duplicate`; and a commit merged by hand of the copy and `m5`.
    let (lab, home, _adopted, copy) = copied_next_lab();
    lab.jj_work(["new", "-r", copy.as_str(), "-r", "m5", "-m", THIRD]);
    lab.jj_work(["new"]);
    let commit = knives::ids::CommitId::new(lab.jj_work_output([
        "log",
        "-r",
        "@-",
        "--no-graph",
        "-T",
        "commit_id",
    ]));

    // When: it is recorded as the series' third release.
    let recorded = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", THIRD, "--at", commit.as_str()],
    );

    // Then: it is recorded: the copy, on the commit's first-parent line,
    // has the write's tree, so it stands for the write, whatever its change
    // id; and the commit's own tree holds the fix.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert_eq!(
        newest_event(&home, THIRD)
            .evidence
            .first()
            .map(String::as_str),
        Some(commit.as_str()),
        "{out}"
    );
}

#[test]
fn a_stacked_cut_on_a_copy_of_the_newest_write_goes_ahead() {
    // Given: NEXT's newest recorded write, an adopt of a fix by hand, copied
    // by `jj duplicate`; and NEXT's bookmark on the copy, as a machine has it
    // that took the copy for NEXT, published by a note.
    let (lab, home, _adopted, copy) = copied_next_lab();
    lab.jj_work([
        "bookmark",
        "set",
        NEXT,
        "-r",
        copy.as_str(),
        "--allow-backwards",
    ]);
    mark_published(&lab, &home, NEXT);

    // When: the next release is cut.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", THIRD]);

    // Then: it goes ahead on the copy: a commit with the newest write's tree
    // stands for that write on the start's first-parent line, and the cut's
    // own tree, the copy's, holds its fix.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(0), "{out}");
    assert_eq!(release_parents(&lab, THIRD), vec![copy], "{out}");
}

/// Change `files` by hand on [`NEXT`] in place (`jj squash` into it, which
/// keeps its change id): each written with its content, or removed where it
/// has none; then adopt it with `why`. Returns the adopt's commit.
fn fix_next_by_hand(
    lab: &Lab,
    home: &tempfile::TempDir,
    files: &[(&str, Option<&str>)],
    why: &str,
) -> knives::ids::CommitId {
    lab.jj_work(["new", "-r", NEXT]);
    change_files(lab, files);
    lab.jj_work(["squash", "--into", NEXT]);
    lab.jj_work(["new", "-r", "old@upstream"]);
    release_ok(lab, home, &["adopt", NEXT, "--why", why]);
    commit_at(lab, NEXT)
}

/// Write each of `files` in the work checkout with its content, or remove
/// it where it has none.
fn change_files(lab: &Lab, files: &[(&str, Option<&str>)]) {
    for (file, content) in files {
        let path = lab.work_path().join(file);
        match content {
            Some(content) => std::fs::write(&path, content).expect("write a file by hand"),
            None => std::fs::remove_file(&path).expect("remove a file by hand"),
        }
    }
}

/// A commit merged by hand of `first` and `m7`, with `files` changed in its
/// resolution ([`change_files`]), and no ref on it. Returns its commit.
fn merged_by_hand(
    lab: &Lab,
    first: &knives::ids::CommitId,
    files: &[(&str, Option<&str>)],
) -> knives::ids::CommitId {
    lab.jj_work([
        "new",
        "-r",
        first.as_str(),
        "-r",
        "m7",
        "-m",
        "merged by hand",
    ]);
    change_files(lab, files);
    lab.jj_work(["new"]);
    knives::ids::CommitId::new(lab.jj_work_output([
        "log",
        "-r",
        "@-",
        "--no-graph",
        "-T",
        "commit_id",
    ]))
}

#[test]
fn a_record_on_the_adopt_whose_own_merge_undoes_the_fix_is_refused_naming_the_file() {
    for fixed in [None, Some("fixed another way\n")] {
        // Given: NEXT's newest recorded write is an adopt of a fix by hand
        // at `fix.txt`; `m7` built on that adopt; and a commit merged by hand
        // of the adopt and `m7`, whose resolution deletes `fix.txt` or
        // changes it.
        let (lab, home, _edit, adopted) = adopted_next_lab();
        branch_on(&lab, adopted.as_str(), "m7", "m7.txt");
        let commit = merged_by_hand(&lab, &adopted, &[("fix.txt", fixed)]);
        let entries = ledger_entries(&home);

        // When: it is recorded as the series' third release.
        let recorded = knives_release(
            &lab,
            &home,
            &["--series", "old", "cut", THIRD, "--at", commit.as_str()],
        );

        // Then: it is refused, naming the adopt and the file, and nothing is
        // written: the adopt on the commit's first-parent line carries
        // nothing for the fix, as the commit's own merge undid it; and the
        // drift check leaves `fix.txt` to the gate.
        let out = text(&recorded);
        assert_eq!(recorded.status.code(), Some(3), "{fixed:?}: {out}");
        assert!(
            out.contains(&format!(
                "demo: refusing to record {THIRD} at {}: it does not carry the fix by hand of \
                 {NEXT}'s adopt {}, in 1 file(s): fix.txt\n",
                commit.short(),
                adopted.short()
            )),
            "{fixed:?}: {out}"
        );
        assert!(!out.contains("!! fix.txt"), "{fixed:?}: {out}");
        assert_eq!(ledger_entries(&home), entries, "{fixed:?}: {out}");
    }
}

#[test]
fn a_stacked_cut_on_a_merge_that_undid_the_fix_is_refused_and_one_on_the_adopt_goes_ahead() {
    // Given: NEXT's newest recorded write is an adopt of a fix by hand at
    // `fix.txt`; `m7` built on that adopt; a commit merged by hand of the
    // adopt and `m7`, whose resolution deletes `fix.txt`; and NEXT's
    // bookmark moved to that merge, published by a note.
    let (lab, home, _edit, adopted) = adopted_next_lab();
    branch_on(&lab, adopted.as_str(), "m7", "m7.txt");
    let merge = merged_by_hand(&lab, &adopted, &[("fix.txt", None)]);
    lab.jj_work(["bookmark", "set", NEXT, "-r", merge.as_str()]);
    mark_published(&lab, &home, NEXT);
    let recorded = ledger_entries(&home).len();

    // When: the next release is cut.
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", THIRD]);

    // Then: it is refused, naming the adopt and the file, and nothing is
    // cut: the cut's own tree lacks the fix, whatever its first-parent line
    // holds.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: this cut does not carry the fix by hand of {NEXT}'s adopt {}, \
             in 1 file(s): fix.txt\n",
            adopted.short()
        )),
        "{out}"
    );
    assert!(!resolves(&lab, THIRD), "{out}");
    assert_eq!(ledger_entries(&home).len(), recorded, "{out}");

    // When: NEXT is moved back to the adopt, published by a note, and the
    // next release is cut again.
    lab.jj_work([
        "bookmark",
        "set",
        NEXT,
        "-r",
        adopted.as_str(),
        "--allow-backwards",
    ]);
    mark_published(&lab, &home, NEXT);
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", THIRD]);

    // Then: it goes ahead on the adopt, whose tree carries the fix.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(0), "{out}");
    assert_eq!(release_parents(&lab, THIRD), vec![adopted], "{out}");
}

#[test]
fn a_stacked_record_without_one_file_of_a_fix_is_refused_for_that_file_alone() {
    // Given: NEXT cut on the published previous release with `m6` included
    // and published; fixed by hand in place at `x.txt` and `y.txt` and
    // adopted; then `x.txt` changed by hand again and adopted again, which
    // ends the first adopt's hold on `x.txt` alone; `m7` built on the second
    // adopt; and a commit merged by hand of that adopt and `m7`, whose
    // resolution deletes `y.txt`.
    let (lab, home) = previous_release_lab(true);
    release_ok(&lab, &home, &["cut", NEXT]);
    release_ok(&lab, &home, &["include", "m6", "--why", "in NEXT"]);
    mark_published(&lab, &home, NEXT);
    let first = fix_next_by_hand(
        &lab,
        &home,
        &[("x.txt", Some("x fixed\n")), ("y.txt", Some("y fixed\n"))],
        "a fix by hand",
    );
    let second = fix_next_by_hand(&lab, &home, &[("x.txt", Some("x refined\n"))], "refined");
    branch_on(&lab, second.as_str(), "m7", "m7.txt");
    let without = merged_by_hand(&lab, &second, &[("y.txt", None)]);
    let entries = ledger_entries(&home);

    // When: it is recorded as the series' third release.
    let refused = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", THIRD, "--at", without.as_str()],
    );

    // Then: it is refused, naming the first adopt and `y.txt` alone, and
    // nothing is written: the second adopt's fix it carries.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to record {THIRD} at {}: it does not carry the fix by hand of \
             {NEXT}'s adopt {}, in 1 file(s): y.txt\n",
            without.short(),
            first.short()
        )),
        "{out}"
    );
    assert!(!out.contains(&format!("adopt {}", second.short())), "{out}");
    assert_eq!(ledger_entries(&home), entries, "{out}");

    // When: the merge that keeps both files is recorded.
    let with = merged_by_hand(&lab, &second, &[]);
    let recorded = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", THIRD, "--at", with.as_str()],
    );

    // Then: it is recorded.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
}

#[test]
fn a_stacked_record_whose_member_changes_other_lines_of_a_fixed_file_is_recorded() {
    // Given: NEXT cut on the published previous release with `m4` included,
    // which adds `shared.txt` of five lines, and published; a line added by
    // hand in place at the top of that file and adopted; `m7` built on the
    // adopt, which changes the last line; and a commit merged by hand of the
    // adopt and `m7`.
    let (lab, home) = previous_release_lab(true);
    lab.jj_work(["new", "-r", PREVIOUS, "-m", "m4"]);
    std::fs::write(lab.work_path().join("shared.txt"), "1\n2\n3\n4\n5\n").expect("write m4");
    lab.jj_work(["bookmark", "create", "m4", "-r", "@"]);
    lab.jj_work(["new"]);
    release_ok(&lab, &home, &["cut", NEXT]);
    release_ok(&lab, &home, &["include", "m4", "--why", "in NEXT"]);
    mark_published(&lab, &home, NEXT);
    let adopted = fix_next_by_hand(
        &lab,
        &home,
        &[("shared.txt", Some("0 fixed\n1\n2\n3\n4\n5\n"))],
        "a fix by hand",
    );
    lab.jj_work(["new", "-r", adopted.as_str(), "-m", "m7"]);
    std::fs::write(
        lab.work_path().join("shared.txt"),
        "0 fixed\n1\n2\n3\n4\n5 changed\n",
    )
    .expect("write m7");
    lab.jj_work(["bookmark", "create", "m7", "-r", "@"]);
    lab.jj_work(["new"]);
    let commit = merged_by_hand(&lab, &adopted, &[]);

    // When: it is recorded as the series' third release.
    let recorded = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", THIRD, "--at", commit.as_str()],
    );

    // Then: it is recorded: the adopt's change at `shared.txt` replays
    // empty onto it, though it holds there what no recorded write holds.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(!out.contains("the fix by hand of"), "{out}");
}

#[test]
fn a_member_that_changes_a_file_an_adopt_added_is_refused_until_an_adopt_keeps_its_content() {
    // Given: NEXT's newest recorded write is an adopt that added `fix.txt`
    // by hand, a file the composition did not have; `m7` built on that
    // adopt, which changes `fix.txt`; and a commit merged by hand of the
    // adopt and `m7`, which holds `m7`'s `fix.txt`.
    let (lab, home, _edit, adopted) = adopted_next_lab();
    let changed = "fixed by hand\nand changed by m7\n";
    lab.jj_work(["new", "-r", adopted.as_str(), "-m", "m7"]);
    std::fs::write(lab.work_path().join("fix.txt"), changed).expect("write m7");
    lab.jj_work(["bookmark", "create", "m7", "-r", "@"]);
    lab.jj_work(["new"]);
    let commit = merged_by_hand(&lab, &adopted, &[]);
    let change = lab.jj_work_output([
        "log",
        "-r",
        commit.as_str(),
        "--no-graph",
        "-T",
        "change_id",
    ]);
    let entries = ledger_entries(&home);

    // When: it is recorded as the series' third release.
    let refused = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", THIRD, "--at", commit.as_str()],
    );

    // Then: it is refused, naming `fix.txt` and the adopt, and nothing is
    // written: the adopt's change at a file it added replays onto other
    // content there as a conflict, and the commit does not hold what the
    // newest write holds. The refusal says to keep the new content by an
    // adopt of it.
    let out = text(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to record {THIRD} at {}: it does not carry the fix by hand of \
             {NEXT}'s adopt {}, in 1 file(s): fix.txt\n",
            commit.short(),
            adopted.short()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "to keep the new content of those files, edit {NEXT} by hand to hold it and run \
             `knives release adopt {NEXT}`"
        )),
        "{out}"
    );
    assert_eq!(ledger_entries(&home), entries, "{out}");

    // When: NEXT is edited by hand to hold `m7`'s `fix.txt` and adopted, as
    // the refusal says, and the same commit, which jj rebased with NEXT, is
    // recorded again.
    let kept = fix_next_by_hand(
        &lab,
        &home,
        &[("fix.txt", Some(changed))],
        "keep m7's change",
    );
    let rebased = knives::ids::CommitId::new(lab.jj_work_output([
        "log",
        "-r",
        change.as_str(),
        "--no-graph",
        "-T",
        "commit_id",
    ]));
    let recorded = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", THIRD, "--at", rebased.as_str()],
    );

    // Then: it is recorded: the new adopt holds `fix.txt` now, and the
    // first adopt's hold on it has ended.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(0), "{out}");
    assert!(
        !out.contains(&format!("adopt {}", adopted.short())),
        "{out}"
    );
    assert!(!out.contains(&format!("adopt {}", kept.short())), "{out}");
}

#[test]
fn a_stacked_record_that_resolves_an_advance_conflict_against_an_adopted_fix_is_refused() {
    // Given: the next release cut on the published previous release, with
    // `m6` included; NEXT fixed by hand in place, a.txt changed, and adopted;
    // then `m6` changes a.txt as well, and `advance` writes NEXT with a.txt
    // conflicted; and the third release merged by hand of NEXT and `m5`,
    // which resolves a.txt to `m6`'s content, without the fix.
    let (lab, home) = previous_release_lab(true);
    release_ok(&lab, &home, &["cut", NEXT]);
    release_ok(&lab, &home, &["include", "m6"]);
    lab.jj_work(["new", "-r", NEXT]);
    std::fs::write(lab.work_path().join("a.txt"), "fixed by hand\n").expect("write the fix");
    lab.jj_work(["squash", "--into", NEXT]);
    lab.jj_work(["new", "-r", "main@origin"]);
    release_ok(&lab, &home, &["adopt", NEXT, "--why", "a fix by hand"]);
    let adopt = commit_at(&lab, NEXT);
    extend_branch(&lab, "m6", "a.txt", "m6\n");
    let advanced = knives_release(&lab, &home, &["--series", "old", "advance", "m6"]);
    assert!(
        matches!(advanced.status.code(), Some(0 | 1)),
        "{}",
        text(&advanced)
    );
    let conflicted = commit_at(&lab, NEXT);
    lab.jj_work([
        "new",
        "-r",
        conflicted.as_str(),
        "-r",
        "m5",
        "-m",
        "the third release, merged by hand",
    ]);
    std::fs::write(lab.work_path().join("a.txt"), "m6\n").expect("resolve a.txt to m6's");
    lab.jj_work(["new"]);
    let commit = knives::ids::CommitId::new(lab.jj_work_output([
        "log",
        "-r",
        "@-",
        "--no-graph",
        "-T",
        "commit_id",
    ]));
    let entries = ledger_entries(&home).len();

    // When: it is recorded.
    let recorded = knives_release(
        &lab,
        &home,
        &["--series", "old", "cut", THIRD, "--at", commit.as_str()],
    );

    // Then: it is refused, naming the adopt whose fix it drops, and nothing
    // is written: a write that left the fixed file conflicted did not remove
    // the fix, so the hold on it stands until an adopt states otherwise.
    let out = text(&recorded);
    assert_eq!(recorded.status.code(), Some(3), "{out}");
    assert!(out.contains(adopt.short()), "{out}");
    assert_eq!(ledger_entries(&home).len(), entries, "{out}");
}
