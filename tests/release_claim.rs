//! The release claim: `knives start <release name>` takes it, and `knives
//! finish <release name>` releases it.
//!
//! The claim's workspace rests on a new empty commit off the root commit, so
//! its working copy keeps no release commit that a release write replaces.
//! `finish` forgets that workspace and then abandons each earlier write of the
//! release that nothing holds, as `knives release adopt` does after its write,
//! and names each one that something other than the release holds, with what
//! holds it. A newer cut, and `knives release reap`, keep a release that a
//! live claim holds. Each case runs in a local repository in the shape of run
//! A of INFRA-4355's probe, with the main checkout's working copy off every
//! commit of the release unless the case says so. These tests drive only the
//! knives binary.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use std::path::{Path, PathBuf};
use std::process::Output;

use knives::ids::CommitId;
use lab::{
    Lab, commit_at, conflicts_of, fixed_home, home_after_first_cut, is_visible, knives_release,
    release_ok, release_ran, release_ran_in, second_machine, share, shared_home, stdout, text,
};
use tempfile::TempDir;

/// The release each test cuts and claims.
const RELEASE: &str = "release/2026-08-04";
/// The release cut after [`RELEASE`].
const NEXT: &str = "release/2026-08-05";
/// The jj workspace of [`RELEASE`]'s claim, named as `knives start` names it.
const CLAIM: &str = "release-2026-08-04";
/// The owner of the release claim.
const OWNER: &str = "release-owner";
/// How the line of a release claim's `start` ends.
const ROOT_BASE: &str =
    "based on the root commit (a release claim's working copy rests on no release commit)";
/// The id of the root commit.
const ROOT: &str = "0000000000000000000000000000000000000000";
/// The hand change made on the release in the claim's workspace.
const NOTE: &str = "a note added by hand\n";
/// The jj operation in which `finish` abandons the earlier writes of
/// [`RELEASE`].
const SWEEP: &str = "knives: release/2026-08-04: finish abandons earlier writes nothing rests on";

/// A lab with `feat/alpha` and `feat/beta`, [`RELEASE`] cut of the two, and
/// its home, with the main checkout's working copy on the root commit.
fn cut_release() -> (Lab, TempDir) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.jj_work(["new", "root()"]);
    (lab, home)
}

/// `feat/gamma`, a third member made after the cut that writes `file`, with
/// the main checkout's working copy back on the root commit.
fn third_member(lab: &Lab, file: &str) {
    lab.branch("feat/gamma", file, "gamma\n");
    lab.jj_work(["new", "root()"]);
}

/// `knives <args>` from the work checkout as `owner`, against the config
/// home `home`, run to completion.
fn knives_as(lab: &Lab, home: &Path, owner: &str, args: &[&str]) -> Output {
    lab::knives_command(&lab.work, home, lab.temp_path(), args)
        .env("KNIVES_OWNER", owner)
        .output()
        .expect("run knives")
}

/// `knives start <branch> --why claim` as [`OWNER`].
fn start(lab: &Lab, home: &TempDir, branch: &str) -> Output {
    knives_as(
        lab,
        home.path(),
        OWNER,
        &[
            "--text", "start", branch, "--repo", "demo", "--why", "claim",
        ],
    )
}

/// `knives finish <RELEASE>` as [`OWNER`].
fn finish(lab: &Lab, home: &TempDir) -> Output {
    knives_as(
        lab,
        home.path(),
        OWNER,
        &["--text", "finish", RELEASE, "--repo", "demo"],
    )
}

/// Where `knives start` puts the workspace named `name`: beside the checkout.
fn workspace(lab: &Lab, name: &str) -> PathBuf {
    lab.work
        .parent()
        .expect("a checkout has a parent")
        .join(name)
}

/// [`cut_release`], claimed, with a hand change made on [`RELEASE`] as step
/// 2 of `release-wide-writes.md` says: in the claim's workspace `jj new
/// <release>`, [`NOTE`] added, `jj squash`, and `knives release adopt` from
/// there. Returns the lab, its home, the claim's workspace and the adopted
/// commit.
fn hand_resolved_release() -> (Lab, TempDir, PathBuf, CommitId) {
    let (lab, home) = cut_release();
    let started = start(&lab, &home, RELEASE);
    assert!(started.status.success(), "{}", text(&started));
    let claim = workspace(&lab, CLAIM);
    lab.jj_at(&claim, ["new", RELEASE]);
    std::fs::write(claim.join("notes.txt"), NOTE).expect("write a hand change");
    lab.jj_at(&claim, ["squash"]);
    let adopted = release_ran_in(
        &lab,
        &home,
        &claim,
        &["adopt", RELEASE, "--why", "hand fix"],
    );
    assert!(
        adopted.contains(&format!("as the newest write of {RELEASE}")),
        "{adopted}"
    );
    let commit = commit_at(&lab, RELEASE);
    (lab, home, claim, commit)
}

/// A claim workspace as knives 5.1.0 made one: at the claim's workspace path,
/// with its name, its working copy an empty child of [`RELEASE`]'s tip.
/// Returns its path and that tip.
fn workspace_on_the_release(lab: &Lab) -> (PathBuf, CommitId) {
    let claim = workspace(lab, CLAIM);
    knives::jj::add_workspace(&lab.work, CLAIM, &claim, RELEASE)
        .expect("add the claim's workspace as knives 5.1.0 did");
    let cut = commit_at(lab, RELEASE);
    assert_eq!(
        lab.revision(&claim, "parents(@)", "commit_id"),
        cut.as_str()
    );
    (claim, cut)
}

/// Whether the working copy of the workspace at `directory` is empty.
fn empty_working_copy(lab: &Lab, directory: &Path) -> bool {
    lab.revision(directory, "@", r#"if(empty, "empty", "changed")"#) == "empty"
}

/// The line of a release write that keeps `commit`, the release commit it
/// replaced, up to the reason.
fn kept_by_the_write(commit: &CommitId) -> String {
    format!(
        "demo: note: kept {}, the {RELEASE} commit this write replaced: ",
        commit.short()
    )
}

/// The line of `finish` that keeps `commit`, an earlier write, up to the
/// reason.
fn kept_by_finish(commit: &CommitId) -> String {
    format!(
        "demo: note: kept {}, an earlier {RELEASE} commit: ",
        commit.short()
    )
}

/// The line of `finish` that abandons `commit`, an earlier write.
fn abandoned_by_finish(commit: &CommitId) -> String {
    format!(
        "demo: abandoned {}, an earlier {RELEASE} commit nothing rests on any more",
        commit.short()
    )
}

/// The reap's line for a superseded `name` that the release claim of
/// `holder` holds.
fn held_line(name: &str, holder: &str) -> String {
    format!(
        "demo: kept {name}: held by the release claim of {holder}; `knives finish {name}` releases it"
    )
}

/// The description of each operation in the shared operation log.
fn operations(lab: &Lab) -> Vec<String> {
    lab.jj_work_output([
        "--ignore-working-copy",
        "op",
        "log",
        "--no-graph",
        "-T",
        "description ++ \"\\n\"",
    ])
    .lines()
    .map(str::to_owned)
    .collect()
}

/// The names of the bookmarks of the work checkout.
fn bookmarks(lab: &Lab) -> Vec<String> {
    lab.jj_work_output([
        "--ignore-working-copy",
        "bookmark",
        "list",
        "-T",
        "name ++ \"\\n\"",
    ])
    .lines()
    .map(str::to_owned)
    .collect()
}

/// The `orphan-commit` findings of `knives --json audit demo --no-github`,
/// each as JSON. The audit reads the publish remote's live refs, so the
/// registry's origin is pointed at the lab's own first, with the same ledger.
fn orphan_commits(lab: &Lab, home: &TempDir) -> Vec<String> {
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"{}\"\n",
            lab.upstream.display(),
            lab.temp_origin().display(),
        ),
    )
    .expect("point the registry at the lab's origin");
    let output = lab::knives(lab, home, &["--json", "audit", "demo", "--no-github"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("audit emits JSON ({error}): {}", text(&output)));
    assert_eq!(
        report["problems"],
        serde_json::json!(["open pull-head reconciliation was skipped (--no-github)"]),
        "the audit must have read everything but pull heads: {report}"
    );
    report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .filter(|finding| finding["kind"] == "orphan-commit")
        .map(ToString::to_string)
        .collect()
}

#[test]
fn a_release_claim_rests_on_the_root_commit_and_a_write_leaves_no_orphan() {
    // Given (K17 1): R cut of two members.
    let (lab, home) = cut_release();

    // When: R is claimed, a third member is included from the main checkout,
    // the claim is finished, and the fork is audited.
    let started = start(&lab, &home, RELEASE);
    let claim = workspace(&lab, CLAIM);
    let parents = lab.revision(&claim, "parents(@)", r#"commit_id ++ "\n""#);
    let empty = empty_working_copy(&lab, &claim);
    third_member(&lab, "gamma.txt");
    release_ran(&lab, &home, &["include", "feat/gamma"]);
    let finished = finish(&lab, &home);
    let orphans = orphan_commits(&lab, &home);

    // Then: the start's line says its workspace rests on the root commit, its
    // working copy is an empty commit whose only parent is the root commit,
    // the finish exits 0, and the audit names no orphan commit.
    let shown = format!("{}\norphan commits: {orphans:?}", text(&started));
    assert_eq!(started.status.code(), Some(0), "{shown}");
    assert!(stdout(&started).contains(ROOT_BASE), "{shown}");
    assert_eq!(parents, ROOT, "{shown}");
    assert!(empty, "{shown}");
    assert_eq!(finished.status.code(), Some(0), "{}", text(&finished));
    assert!(orphans.is_empty(), "{shown}");
}

#[test]
fn finish_abandons_an_earlier_write_the_claims_working_copy_left_behind() {
    // Given (K17 2): a hand change on R adopted as step 2 says, then a third
    // member included from the claim's workspace, which keeps the adopted
    // commit for the claim's working copy and says so.
    let (lab, home, claim, adopted) = hand_resolved_release();
    third_member(&lab, "gamma.txt");
    let included = release_ran_in(&lab, &home, &claim, &["include", "feat/gamma"]);
    assert!(
        included.contains(&kept_by_the_write(&adopted))
            && included.contains(&format!("the working copy of {CLAIM}")),
        "{included}"
    );

    // When: the claim is finished.
    let finished = finish(&lab, &home);

    // Then: it exits 0 and abandons the adopted commit in one jj operation,
    // printed as `adopt` prints an abandoned earlier write, and the audit
    // names no orphan commit.
    let out = text(&finished);
    assert_eq!(finished.status.code(), Some(0), "{out}");
    assert!(
        stdout(&finished).contains(&abandoned_by_finish(&adopted)),
        "{out}"
    );
    assert!(!is_visible(&lab, &adopted), "{out}");
    let sweeps = operations(&lab)
        .iter()
        .filter(|description| *description == SWEEP)
        .count();
    assert_eq!(sweeps, 1, "{out}");
    let orphans = orphan_commits(&lab, &home);
    assert!(orphans.is_empty(), "{orphans:?}");
}

#[test]
fn finish_keeps_an_earlier_write_another_checkouts_working_copy_rests_on() {
    // Given (K17 3): as K17 2, but the claim's workspace moved off R and the
    // main checkout's working copy on the adopted commit when the include
    // runs.
    let (lab, home, claim, adopted) = hand_resolved_release();
    third_member(&lab, "gamma.txt");
    lab.jj_at(&claim, ["new", "root()"]);
    lab.jj_work(["new", adopted.as_str()]);
    let main = lab.revision(&lab.work, "@", "commit_id");
    let included = release_ran_in(&lab, &home, &claim, &["include", "feat/gamma"]);
    assert!(
        included.contains(&kept_by_the_write(&adopted))
            && included.contains("the working copy of default"),
        "{included}"
    );

    // When: the claim is finished.
    let finished = finish(&lab, &home);

    // Then: it exits 0, keeps the adopted commit and names the main
    // checkout's working copy as why; that working copy and its files are as
    // they were.
    let out = text(&finished);
    assert_eq!(finished.status.code(), Some(0), "{out}");
    let said = stdout(&finished);
    let kept = said
        .lines()
        .find(|line| line.starts_with(&kept_by_finish(&adopted)))
        .unwrap_or_else(|| panic!("finish named no kept earlier write: {out}"));
    assert!(kept.contains("the working copy of default"), "{out}");
    assert!(!said.contains("demo: abandoned "), "{out}");
    assert!(is_visible(&lab, &adopted), "{out}");
    assert_eq!(lab.revision(&lab.work, "@", "commit_id"), main);
    assert_eq!(
        std::fs::read_to_string(lab.work.join("notes.txt")).expect("read the note"),
        NOTE
    );
}

#[test]
fn finish_keeps_an_earlier_write_a_bookmark_names() {
    // Given (K17 3, the bookmark half): as K17 2, but the claim's workspace
    // moved off R and a bookmark naming the adopted commit when the include
    // runs.
    let (lab, home, claim, adopted) = hand_resolved_release();
    third_member(&lab, "gamma.txt");
    lab.jj_at(&claim, ["new", "root()"]);
    lab.jj_work(["bookmark", "create", "keep-me", "-r", adopted.as_str()]);
    let included = release_ran_in(&lab, &home, &claim, &["include", "feat/gamma"]);
    assert!(
        included.contains(&kept_by_the_write(&adopted)),
        "{included}"
    );

    // When: the claim is finished.
    let finished = finish(&lab, &home);

    // Then: it exits 0, keeps the adopted commit and names the bookmark as
    // why.
    let out = text(&finished);
    assert_eq!(finished.status.code(), Some(0), "{out}");
    assert!(
        stdout(&finished).contains(&format!(
            "{}bookmark keep-me points at it",
            kept_by_finish(&adopted)
        )),
        "{out}"
    );
    assert!(is_visible(&lab, &adopted), "{out}");
}

#[test]
fn finish_sweeps_nothing_while_the_release_holds_conflicts() {
    // Given (K17 4): as K17 2, but the third member writes `alpha.txt`, which
    // a member of R writes too and no union joins, so R holds a conflict
    // after the include.
    let (lab, home, claim, adopted) = hand_resolved_release();
    third_member(&lab, "alpha.txt");
    let included = release_ran_in(&lab, &home, &claim, &["include", "feat/gamma"]);
    assert_eq!(conflicts_of(&lab, RELEASE), ["alpha.txt"], "{included}");

    // When: the claim is finished.
    let finished = finish(&lab, &home);

    // Then: it exits 0, abandons nothing, and says so in one line.
    let out = text(&finished);
    assert_eq!(finished.status.code(), Some(0), "{out}");
    let swept_none = format!(
        "demo: swept no earlier {RELEASE} commit: the release holds conflicts, and its earlier \
         writes are the record of their last resolution"
    );
    let said = stdout(&finished);
    assert_eq!(
        said.lines().filter(|line| *line == swept_none).count(),
        1,
        "{out}"
    );
    assert!(!said.contains("demo: abandoned "), "{out}");
    assert!(is_visible(&lab, &adopted), "{out}");
}

#[test]
fn a_release_finish_whose_snapshot_jj_refuses_sweeps_nothing() {
    // Given: K17 2's shape up to its include, then a new file in the claim's
    // workspace over jj's default snapshot.max-new-file-size (1 MiB).
    let (lab, home, claim, adopted) = hand_resolved_release();
    third_member(&lab, "gamma.txt");
    let included = release_ran_in(&lab, &home, &claim, &["include", "feat/gamma"]);
    assert!(
        included.contains(&kept_by_the_write(&adopted)),
        "{included}"
    );
    std::fs::write(claim.join("big.bin"), vec![b'x'; 2 * 1024 * 1024]).expect("write");

    // When: the claim is finished.
    let finished = finish(&lab, &home);

    // Then: it exits 3 as before, sweeps nothing (no line names an earlier
    // commit of R), and the adopted commit and the claim's workspace stay.
    let out = text(&finished);
    assert_eq!(finished.status.code(), Some(3), "{out}");
    assert!(!out.contains(&format!("earlier {RELEASE} commit")), "{out}");
    assert!(is_visible(&lab, &adopted), "{out}");
    assert!(claim.join("big.bin").is_file(), "{out}");
    let names = lab.jj_work_output(["workspace", "list", "-T", "name ++ \"\\n\""]);
    assert!(names.lines().any(|name| name == CLAIM), "{names}");
}

#[test]
fn finish_covers_a_claim_workspace_knives_5_1_0_made() {
    // Given (K17 6): R cut, and a claim workspace as knives 5.1.0 made one,
    // its working copy an empty child of R's tip.
    let (lab, home) = cut_release();
    let (claim, cut) = workspace_on_the_release(&lab);
    let before = lab.revision(&claim, "@", "commit_id");

    // When: R is claimed.
    let started = start(&lab, &home, RELEASE);

    // Then: the start adopts that workspace and leaves it as it is.
    let said = stdout(&started);
    assert_eq!(started.status.code(), Some(0), "{}", text(&started));
    assert!(
        said.contains("adopted existing workspace ") && said.contains("; left as-is"),
        "{said}"
    );
    assert_eq!(lab.revision(&claim, "@", "commit_id"), before, "{said}");

    // When: a third member is included from the main checkout, and the claim
    // is finished.
    third_member(&lab, "gamma.txt");
    release_ran(&lab, &home, &["include", "feat/gamma"]);
    let finished = finish(&lab, &home);

    // Then: the finish exits 0 and abandons the cut that working copy kept,
    // and the audit names no orphan commit.
    let out = text(&finished);
    assert_eq!(finished.status.code(), Some(0), "{out}");
    assert!(
        stdout(&finished).contains(&abandoned_by_finish(&cut)),
        "{out}"
    );
    let orphans = orphan_commits(&lab, &home);
    assert!(orphans.is_empty(), "{orphans:?}");
}

#[test]
fn a_claim_workspace_made_on_a_release_commit_is_left_as_is() {
    // Given: R cut, a claim workspace on its tip as knives 5.1.0 made one,
    // and R claimed, which adopted that workspace.
    let (lab, home) = cut_release();
    let (claim, cut) = workspace_on_the_release(&lab);
    let before = lab.revision(&claim, "@", "commit_id");
    let adopted = start(&lab, &home, RELEASE);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    assert!(
        stdout(&adopted).contains("; left as-is"),
        "{}",
        text(&adopted)
    );

    // When: R is started again, which resumes the claim.
    let resumed = start(&lab, &home, RELEASE);

    // Then: the claim is resumed, and the workspace's working copy is the one
    // it had, an empty child of R's tip.
    let said = stdout(&resumed);
    assert_eq!(resumed.status.code(), Some(0), "{}", text(&resumed));
    assert!(
        said.lines().any(|line| line.starts_with("resumed")),
        "{said}"
    );
    assert!(!said.contains(ROOT_BASE), "{said}");
    assert_eq!(lab.revision(&claim, "@", "commit_id"), before, "{said}");
    assert_eq!(
        lab.revision(&claim, "parents(@)", "commit_id"),
        cut.as_str()
    );
}

#[test]
fn a_newer_cut_and_the_reap_keep_a_release_its_claim_holds() {
    // Given (K17 7): R, the newest cut, claimed.
    let (lab, home) = cut_release();
    let started = start(&lab, &home, RELEASE);
    assert!(started.status.success(), "{}", text(&started));

    // When: a newer release is cut; the reap runs alone; and the reap runs
    // again over another superseded name that nothing holds, with a state
    // file that cannot be read.
    let cut = knives_release(&lab, &home, &["cut", NEXT]);
    let after_cut = bookmarks(&lab);
    let reaped = knives_release(&lab, &home, &["reap"]);
    let after_reap = bookmarks(&lab);
    let older = "release/2026-08-01";
    lab.jj_work(["new", "main@origin", "-m", "an older cut"]);
    lab.jj_work(["bookmark", "create", older, "-r", "@"]);
    lab.jj_work(["new", "root()"]);
    std::fs::write(home.path().join("state.json"), "{ not json").expect("break the state file");
    let unread = knives_release(&lab, &home, &["reap"]);
    let after_unread = bookmarks(&lab);

    // Then: the cut and the reap each exit 1, R's refs stay, and each keeps R
    // for its claim with a line that names `knives finish` and no `--force`.
    let shown = format!(
        "cut:\n{}\nreap:\n{}\nreap with the state file unreadable:\n{}",
        text(&cut),
        text(&reaped),
        text(&unread)
    );
    let held = held_line(RELEASE, OWNER);
    for (output, after) in [(&cut, &after_cut), (&reaped, &after_reap)] {
        assert_eq!(output.status.code(), Some(1), "{shown}");
        assert!(after.iter().any(|name| name == RELEASE), "{shown}");
        assert!(stdout(output).lines().any(|line| line == held), "{shown}");
    }
    // And: the reap that cannot read the state file keeps both names, each
    // with a line that says it could not read the claims, and exits 1.
    assert_eq!(unread.status.code(), Some(1), "{shown}");
    for name in [older, RELEASE] {
        assert!(after_unread.iter().any(|kept| kept == name), "{shown}");
        let unread_line = format!(
            "demo: kept {name}: the claims could not be read, so a release claim may hold it: "
        );
        assert!(
            stdout(&unread)
                .lines()
                .any(|line| line.starts_with(&unread_line)),
            "{shown}"
        );
    }
}

#[test]
fn start_of_a_branch_that_is_no_release_name_rests_on_its_tip() {
    // Given (K17 5): R cut, and an existing branch `feat/x`.
    let (lab, home) = cut_release();
    lab.branch("feat/x", "x.txt", "x\n");
    let tip = commit_at(&lab, "feat/x");

    // When: the branch is started.
    let started = start(&lab, &home, "feat/x");

    // Then: its workspace is an empty child of the branch's tip, and its line
    // says so.
    let said = stdout(&started);
    assert_eq!(started.status.code(), Some(0), "{}", text(&started));
    assert!(
        said.contains(&format!("based on {} (feat/x's tip)", tip.as_str())),
        "{said}"
    );
    let branch = workspace(&lab, "feat-x");
    assert_eq!(
        lab.revision(&branch, "parents(@)", "commit_id"),
        tip.as_str()
    );
    assert!(empty_working_copy(&lab, &branch), "{said}");
}

#[test]
fn start_of_an_earlier_dated_cut_under_a_fixed_entry_rests_on_its_tip() {
    // Given: a fork released on the fixed branch `integration`, and an
    // earlier dated cut, R, of two members.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(RELEASE, "feat/alpha", "feat/beta");
    let tip = commit_at(&lab, RELEASE);
    let home = fixed_home(&lab, "integration");

    // When: R is started.
    let started = start(&lab, &home, RELEASE);

    // Then: R is no release name under that entry: its workspace is an empty
    // child of R's tip, and its line says so.
    let said = stdout(&started);
    assert_eq!(started.status.code(), Some(0), "{}", text(&started));
    assert!(
        said.contains(&format!("based on {} ({RELEASE}'s tip)", tip.as_str())),
        "{said}"
    );
    let claim = workspace(&lab, CLAIM);
    assert_eq!(
        lab.revision(&claim, "parents(@)", "commit_id"),
        tip.as_str()
    );
    assert!(empty_working_copy(&lab, &claim), "{said}");
}

#[test]
fn a_stale_claim_copy_keeps_the_release_and_finish_removes_it() {
    // Given: a fork that shares its ledger between machines one and two, on
    // one checkout; R cut and claimed on machine one; then owner two takes
    // the claim on machine two, so machine one's state file keeps a copy of
    // a claim the ledger no longer holds.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let one = shared_home(&lab);
    let two = second_machine(one.path());
    let (_remote_dir, remote) = lab::bare_remote();
    share(one.path(), "machine-one", &remote);
    share(two.path(), "machine-two", &remote);
    release_ok(&lab, &one, &["cut", RELEASE]);
    lab.jj_work(["new", "root()"]);
    let started = start(&lab, &one, RELEASE);
    assert!(started.status.success(), "{}", text(&started));
    let taken = knives_as(
        &lab,
        two.path(),
        "owner-two",
        &[
            "--text",
            "start",
            RELEASE,
            "--repo",
            "demo",
            "--force",
            "--why",
            "take it over",
        ],
    );
    assert!(taken.status.success(), "{}", text(&taken));

    // When: machine one cuts the next release.
    let cut = knives_release(&lab, &one, &["cut", NEXT]);

    // Then: its reap keeps R for machine one's copy, which names its first
    // holder, and the cut exits 1.
    let out = text(&cut);
    assert_eq!(cut.status.code(), Some(1), "{out}");
    let held = held_line(RELEASE, OWNER);
    assert!(stdout(&cut).lines().any(|line| line == held), "{out}");

    // When: the first holder finishes R on machine one.
    let finished = finish(&lab, &one);

    // Then: the finish removes the copy, names owner two as the live holder,
    // and exits 1.
    let out = text(&finished);
    assert_eq!(finished.status.code(), Some(1), "{out}");
    let stderr = String::from_utf8_lossy(&finished.stderr);
    assert!(
        stderr.contains(&format!(
            "the ledger holds another owner's claim of {RELEASE}"
        )) && stderr.contains("owner-two"),
        "{out}"
    );

    // And: the claim's workspace rests on the root commit, so the next cut
    // on machine one reaps R.
    let claim = workspace(&lab, CLAIM);
    assert_eq!(lab.revision(&claim, "parents(@)", "commit_id"), ROOT);
    let later = knives_release(&lab, &one, &["cut", "release/2026-08-06"]);
    let out = text(&later);
    assert!(
        stdout(&later).contains(&format!(
            "demo: reaped {RELEASE} (refs forgotten everywhere"
        )),
        "{out}"
    );
}
