//! The content audit: does the cut carry what its members carry?
//!
//! Each member replays against the release from the captured tip, not the moved
//! bookmark: an empty replay is content present, a non-empty one is content
//! missing, and a conflict is inconclusive only when the cut itself is
//! conflicted. Multi-commit members, adds-then-deletes, renames, regenerated
//! lockfiles and a loss the previous release did not have are each judged; the
//! previous release's recorded conflict resolutions survive a recut, a rebase
//! and a drop.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::ids::CommitId;
use knives::jj::Repo;
use lab::{Lab, file_at_revision, knives_release, release_test_home};
use std::process::Command;

#[test]
fn the_audit_catches_a_cut_missing_a_members_content() {
    // Given: a cut that names both feature tips as parents but whose tree loses beta.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![trunk.clone(), alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = committed_cut_fixture(&lab, &request, None).expect("build cut");
    lab.jj_work(["bookmark", "create", "doctored-cut", "-r", cut.as_str()]);
    lab.jj_work(["edit", "doctored-cut"]);
    std::fs::remove_file(lab.work.join("beta.txt")).expect("remove beta from cut tree");
    lab.jj_work(["bookmark", "set", "doctored-cut", "-r", "@"]);
    lab.jj_work(["new"]);
    let cut = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit("doctored-cut")
        .expect("resolve doctored cut");

    // When: the cut is audited against both captured feature tips.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        knives::commands::release::CutSubject::Committed(&cut),
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: the omitted branch is the only missing member.
    assert_eq!(audit.missing, vec!["feat/beta".to_owned()], "{audit:?}");
    assert!(audit.unexplained.is_empty(), "{audit:?}");
    assert!(!audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_passes_a_faithful_cut() {
    // Given: two feature tips included in a flat cut.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };

    // When: the cut is audited against its captured members.
    let cut = committed_cut_fixture(&lab, &request, None).expect("build cut");
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        knives::commands::release::CutSubject::Committed(&cut),
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: every member's content is present.
    assert!(audit.passed(), "{audit:?}");
}

#[test]
fn divergence_the_previous_release_already_carried_is_not_a_loss() {
    // Given: a previous release whose recorded resolution dropped beta's file,
    // and a fresh cut duplicated from it — the same published divergence.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-04".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let previous = committed_cut_fixture(&lab, &request, None).expect("build previous");
    lab.jj_work([
        "bookmark",
        "create",
        "resolved-previous",
        "-r",
        previous.as_str(),
    ]);
    lab.jj_work(["edit", "resolved-previous"]);
    std::fs::remove_file(lab.work.join("beta.txt")).expect("drop beta by resolution");
    lab.jj_work(["bookmark", "set", "resolved-previous", "-r", "@"]);
    lab.jj_work(["new"]);
    let previous = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit("resolved-previous")
        .expect("resolve doctored previous");
    let recut = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        ..request
    };
    let cut = committed_cut_fixture(&lab, &recut, Some(&previous)).expect("build recut");

    // When: the recut is audited with the previous release in view.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        knives::commands::release::CutSubject::Committed(&cut),
        knives::commands::release::AuditContext {
            previous: Some(&previous),
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: the carried divergence is reported without failing the audit.
    assert_eq!(audit.carried, vec!["feat/beta".to_owned()], "{audit:?}");
    assert!(audit.missing.is_empty(), "{audit:?}");
    assert!(audit.passed(), "{audit:?}");
}

#[test]
fn a_loss_the_previous_release_did_not_have_still_fails_the_audit() {
    // Given: a previous release that faithfully carries beta, and a recut whose
    // tree lost beta's file — new divergence, the incident the audit exists for.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-04".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let previous = committed_cut_fixture(&lab, &request, None).expect("build previous");
    let recut = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        ..request
    };
    let cut = committed_cut_fixture(&lab, &recut, Some(&previous)).expect("build recut");
    lab.jj_work(["bookmark", "create", "lossy-recut", "-r", cut.as_str()]);
    lab.jj_work(["edit", "lossy-recut"]);
    std::fs::remove_file(lab.work.join("beta.txt")).expect("lose beta from the recut");
    lab.jj_work(["bookmark", "set", "lossy-recut", "-r", "@"]);
    lab.jj_work(["new"]);
    let cut = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit("lossy-recut")
        .expect("resolve lossy recut");

    // When: the recut is audited with the previous release in view.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        knives::commands::release::CutSubject::Committed(&cut),
        knives::commands::release::AuditContext {
            previous: Some(&previous),
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: the new loss fails the audit; nothing about it is "carried".
    assert_eq!(audit.missing, vec!["feat/beta".to_owned()], "{audit:?}");
    assert!(audit.carried.is_empty(), "{audit:?}");
    assert!(!audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_passes_a_faithful_multi_commit_member_without_inconclusive() {
    // Given: alpha has two commits that both touch its original file, and both
    // feature tips are included in a flat cut.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "first\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work(["new", "feat/alpha", "-m", "alpha follow-up"]);
    std::fs::write(lab.work.join("alpha.txt"), "first\nsecond\n").expect("extend alpha");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![trunk.clone(), alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = committed_cut_fixture(&lab, &request, None).expect("build cut");

    // When: the fresh cut is audited using the captured tips.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        knives::commands::release::CutSubject::Committed(&cut),
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: replaying the net member effect sees it as present without a
    // manufactured intermediate-commit conflict.
    assert!(audit.inconclusive.is_empty(), "{audit:?}");
    assert!(audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_passes_when_a_member_adds_then_deletes_a_file() {
    // Given: alpha's final tree has no trace of the file it added in its first commit.
    let lab = Lab::new();
    lab.branch("feat/alpha", "z.txt", "temporary\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work(["new", "feat/alpha", "-m", "delete temporary file"]);
    std::fs::remove_file(lab.work.join("z.txt")).expect("delete temporary file");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = committed_cut_fixture(&lab, &request, None).expect("build cut");
    let deleted_path = Command::new("jj")
        .args([
            "--repository",
            lab.work.to_str().expect("utf-8 repository path"),
            "--ignore-working-copy",
            "file",
            "show",
            "-r",
            cut.as_str(),
            "root:z.txt",
        ])
        .output()
        .expect("inspect cut tree for deleted path");
    assert!(
        !deleted_path.status.success(),
        "the faithful cut unexpectedly contains z.txt: {}",
        String::from_utf8_lossy(&deleted_path.stderr)
    );

    // When: the faithful cut is audited using alpha's two-commit range.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        knives::commands::release::CutSubject::Committed(&cut),
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: the net-zero member is present and does not abandon the cut.
    assert!(audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_catches_a_member_whose_early_range_content_is_missing() {
    // Given: alpha's first commit adds early content and its second adds late content.
    let lab = Lab::new();
    lab.branch("feat/alpha", "early.txt", "early\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work(["new", "feat/alpha", "-m", "add late content"]);
    std::fs::write(lab.work.join("late.txt"), "late\n").expect("write late content");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = committed_cut_fixture(&lab, &request, None).expect("build cut");
    lab.jj_work(["bookmark", "create", "doctored-cut", "-r", cut.as_str()]);
    lab.jj_work(["edit", "doctored-cut"]);
    std::fs::remove_file(lab.work.join("early.txt")).expect("remove early content from cut");
    lab.jj_work(["bookmark", "set", "doctored-cut", "-r", "@"]);
    lab.jj_work(["new"]);
    let cut = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit("doctored-cut")
        .expect("resolve doctored cut");

    // When: the doctored cut is audited against alpha's complete captured range.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        knives::commands::release::CutSubject::Committed(&cut),
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: content absent from the first commit fails the member's whole-range audit.
    assert_eq!(audit.missing, vec!["feat/alpha".to_owned()], "{audit:?}");
    assert!(!audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_passes_when_a_member_renames_a_file() {
    // Given: alpha's second commit moves its first commit's file to a new path.
    let lab = Lab::new();
    lab.branch("feat/alpha", "old.txt", "content\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work(["new", "feat/alpha", "-m", "rename feature file"]);
    std::fs::rename(lab.work.join("old.txt"), lab.work.join("new.txt"))
        .expect("rename feature file");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = committed_cut_fixture(&lab, &request, None).expect("build cut");

    // When: the cut includes the renamed tree and audits the full member range.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        knives::commands::release::CutSubject::Committed(&cut),
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: the faithful rename is present and leaves the cut auditable.
    assert!(audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_fails_when_a_regenerated_lockfile_loses_member_content() {
    // Given: alpha adds a two-entry lockfile, but the cut's conflict-free tree
    // carries only the regenerated first entry.
    let lab = Lab::new();
    lab.branch("feat/alpha", "uv.lock", "pkg-a\npkg-b\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![trunk.clone(), alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = committed_cut_fixture(&lab, &request, None).expect("build cut");
    lab.jj_work(["bookmark", "create", "doctored-cut", "-r", cut.as_str()]);
    lab.jj_work(["edit", "doctored-cut"]);
    std::fs::write(lab.work.join("uv.lock"), "pkg-a\n").expect("regenerate lockfile");
    lab.jj_work(["bookmark", "set", "doctored-cut", "-r", "@"]);
    lab.jj_work(["new"]);
    let cut = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit("doctored-cut")
        .expect("resolve doctored cut");

    // When: the partially regenerated cut is audited against captured tips.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        knives::commands::release::CutSubject::Committed(&cut),
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: the divergent member fails instead of being passed as inconclusive.
    assert_eq!(audit.missing, vec!["feat/alpha".to_owned()], "{audit:?}");
    assert!(!audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_judges_the_captured_tip_not_the_moved_bookmark() {
    // Given: a faithful cut and its captured feature tips.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = committed_cut_fixture(&lab, &request, None).expect("build cut");
    lab.jj_work(["new", "-r", "feat/beta", "-m", "beta moved after planning"]);
    std::fs::write(lab.work.join("beta-next.txt"), "new beta work\n").expect("write moved beta");
    lab.jj_work(["bookmark", "set", "feat/beta", "-r", "@"]);
    lab.jj_work(["new"]);

    // When: the audit receives the original tip rather than resolving the bookmark.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        knives::commands::release::CutSubject::Committed(&cut),
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: later work on the bookmark does not make the already faithful cut fail.
    assert!(audit.passed(), "{audit:?}");
}

fn resolved_two_branch_cut(lab: &Lab) -> (CommitId, CommitId, CommitId) {
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let gamma = repo.resolve_commit("feat/gamma").expect("resolve gamma");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-04".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = committed_cut_fixture(lab, &request, None).expect("build first cut");
    lab.jj_work([
        "bookmark",
        "create",
        "release/2026-08-04",
        "-r",
        cut.as_str(),
    ]);
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::write(lab.work.join("shared.txt"), "resolved\n").expect("resolve conflict");
    lab.jj_work(["bookmark", "set", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);
    (alpha, beta, gamma)
}

/// A committed, unnamed cut for fixtures. The doctored-tree audit tests and
/// content assertions need a commit that real jj commands can edit and read,
/// which the live cut path's scratch candidate deliberately is not.
fn committed_cut_fixture(
    lab: &Lab,
    request: &knives::commands::release::Cut,
    previous: Option<&CommitId>,
) -> Result<CommitId, knives::jj::JjError> {
    knives::jj::write_release(
        &lab.work,
        &knives::jj::ReleaseWrite {
            source: previous,
            parents: &request.parents,
            message: Some(&request.message()),
            bookmark: None,
            operation: &format!("knives: cut {} (fixture)", request.name),
        },
    )
}

/// The commit each parent of a named release sits at right now.
#[test]
fn an_incremental_recut_preserves_the_previous_cuts_conflict_resolutions() {
    // Given: a resolved two-branch cut plus a third branch to include.
    let lab = Lab::new();
    let (alpha, beta, gamma) = resolved_two_branch_cut(&lab);
    let previous = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("release/2026-08-04")
        .expect("resolve previous cut");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha, beta, gamma],
        provenance: Vec::new(),
    };

    // When: the next cut duplicates the resolved cut onto the new parent set.
    let cut =
        committed_cut_fixture(&lab, &request, Some(&previous)).expect("build incremental cut");

    // Then: the resolution, new branch content, and new message all survive.
    assert_eq!(
        file_at_revision(&lab, cut.as_str(), "shared.txt"),
        "resolved\n"
    );
    assert_eq!(file_at_revision(&lab, cut.as_str(), "gamma.txt"), "gamma\n");
    assert_eq!(
        lab.revision(&lab.work, cut.as_str(), "description"),
        request.message().trim_end()
    );
    assert!(
        knives::jj::conflicted_files(&lab.work, cut.as_str())
            .expect("list conflicts")
            .is_empty()
    );
}

#[test]
fn a_rebase_preserves_the_previous_releases_conflict_resolution() {
    // Given: two release members whose conflict was resolved by hand in the prior release.
    let lab = Lab::new();
    let _members = resolved_two_branch_cut(&lab);
    let (home, _consumer) = release_test_home(&lab);
    lab.advance_upstream("upstream advance\n");

    // When: the real binary rebases the release onto the advanced upstream.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: duplicating the old release carries its resolution without a new conflict.
    assert!(
        output.status.success(),
        "rebase failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let release = "release/2026-08-04";
    assert_eq!(file_at_revision(&lab, release, "shared.txt"), "resolved\n");
    assert!(
        knives::jj::conflicted_files(&lab.work, release)
            .expect("list release conflicts")
            .is_empty(),
        "rebase re-created the resolved conflict"
    );
}

#[test]
fn dropping_a_resolved_branch_surfaces_a_focused_conflict_not_silence() {
    // Given: a resolved two-branch cut where beta's content is entangled in the resolution.
    let lab = Lab::new();
    let (alpha, _beta, _gamma) = resolved_two_branch_cut(&lab);
    let previous = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("release/2026-08-04")
        .expect("resolve previous cut");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha],
        provenance: Vec::new(),
    };

    // When: the next cut drops beta while preserving the prior resolution diff.
    let cut =
        committed_cut_fixture(&lab, &request, Some(&previous)).expect("build incremental cut");

    // Then: jj reports the one entangled file as a conflict instead of silently retaining beta.
    assert_eq!(
        knives::jj::conflicted_files(&lab.work, cut.as_str()).expect("list conflicts"),
        vec!["shared.txt".to_owned()]
    );
}
