//! A fork branch states the non-fork alternative it rejected before it exists.
//!
//! `start` refuses a new branch with no placement verdict and asks the forcing
//! question; a verdict on the command line is recorded on the branch as a
//! `placement: verdict:` note — on a claim, a resume or a seizure alike — and a
//! `CONSUMER` verdict starts nothing. `release include` and `release advance`
//! read the note back: the newest verdict decides, `CONSUMER` refuses even a
//! member some cut or edit recorded, and only a branch with no verdict at all
//! falls back to grandfathering — a first-time name is refused. A `placement:`
//! note that carries no `verdict:` is prose, not a verdict.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::ledger::{Kind, Ledger};
use knives::placement::{
    CONSUMER_REFUSAL, Verdict, forcing_question, missing_member_refusal, notch_remedy, recorded,
};
use lab::{
    Lab, home_after_first_cut, knives, knives_command, knives_release, placement_file,
    release_parents, release_test_home, state_placement,
};

/// `knives start <branch> --repo demo --why test [extra…]` without the lab
/// helper's default verdict, so a test states exactly what the command gets.
fn start(
    lab: &Lab,
    home: &tempfile::TempDir,
    branch: &str,
    extra: &[&str],
) -> std::process::Output {
    let mut args = vec!["--text", "start", branch, "--repo", "demo", "--why", "test"];
    args.extend_from_slice(extra);
    knives_command(&lab.work, home.path(), lab.temp_path(), &args)
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("run knives start")
}

fn ledger(home: &tempfile::TempDir) -> Ledger {
    Ledger::at(home.path().join("ledger").join("demo"))
}

#[test]
fn a_new_branch_without_a_verdict_is_refused_with_the_forcing_question() {
    // Given: a registered fork and a branch name nothing of ours carries.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);

    // When: it is started with a reason but no placement.
    let output = start(&lab, &home, "feat/gamma", &[]);

    // Then: refused verbatim, nothing claimed, no workspace, no ledger entry.
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim_end(),
        forcing_question("feat/gamma")
    );
    assert!(
        !lab.work
            .parent()
            .expect("parent")
            .join("feat-gamma")
            .exists(),
        "a refused start opened a workspace"
    );
    assert!(
        ledger(&home).entries().expect("read ledger").is_empty(),
        "a refused start wrote to the ledger"
    );
    let state = std::fs::read_to_string(home.path().join("state.json")).unwrap_or_default();
    assert!(
        !state.contains("feat/gamma"),
        "a refused start claimed: {state}"
    );
}

#[test]
fn a_verdict_on_the_command_line_is_recorded_on_the_branch() {
    // Given: a verdict file the red-team produced.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let verdict = placement_file(&home, "UPSTREAM");

    // When: the branch is started with it.
    let output = start(
        &lab,
        &home,
        "feat/gamma",
        &["--placement", verdict.to_str().expect("utf-8 path")],
    );

    // Then: the workspace opens and the ledger carries the verdict as a note
    // after the claim event, marked so the release verbs and `gh` find it.
    assert!(output.status.success(), "{output:?}");
    let entries = ledger(&home).entries().expect("read ledger");
    assert_eq!(entries.len(), 2, "was: {entries:?}");
    assert_eq!(entries[1].kind, Kind::Note);
    assert_eq!(entries[1].subject.as_deref(), Some("feat/gamma"));
    assert!(
        entries[1]
            .text
            .starts_with("placement: verdict: UPSTREAM\n"),
        "was: {}",
        entries[1].text
    );
    assert!(
        entries[1].text.contains("judge: lab-red-team"),
        "the whole file is kept: {}",
        entries[1].text
    );
}

#[test]
fn a_consumer_verdict_starts_no_branch() {
    // A verdict that says the effect belongs in the consumer answers the forcing
    // question with "no fork"; a branch started on it would contradict it.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let verdict = placement_file(&home, "CONSUMER");

    let output = start(
        &lab,
        &home,
        "feat/gamma",
        &["--placement", verdict.to_str().expect("utf-8 path")],
    );

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(CONSUMER_REFUSAL), "{stderr}");
    assert!(
        !lab.work
            .parent()
            .expect("parent")
            .join("feat-gamma")
            .exists(),
        "a consumer verdict opened a workspace"
    );
}

#[test]
fn a_verdict_file_without_a_verdict_line_is_refused_before_anything_happens() {
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let file = home.path().join("no-verdict.md");
    std::fs::write(&file, "alternative: none considered\n").expect("write file");

    let output = start(
        &lab,
        &home,
        "feat/gamma",
        &["--placement", file.to_str().expect("utf-8 path")],
    );

    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("verdict: CONSUMER | FORK | UPSTREAM"),
        "{stderr}"
    );
    assert!(
        !lab.work
            .parent()
            .expect("parent")
            .join("feat-gamma")
            .exists(),
        "an unreadable verdict opened a workspace"
    );
}

#[test]
fn an_existing_branch_is_continued_without_a_verdict() {
    // The gate is on creation: a branch already on one of our remotes was
    // started before, and continuing it asks nothing.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.jj_work(["bookmark", "forget", "feat/alpha"]);
    let (home, _consumer) = release_test_home(&lab);

    let output = start(&lab, &home, "feat/alpha", &[]);

    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("feat/alpha's tip"),
        "{output:?}"
    );
}

#[test]
fn a_recorded_verdict_lets_the_branch_be_started_again_without_the_file() {
    // Given: a branch started with a verdict, then handed back before it was
    // ever pushed — so nothing of ours names it and its workspace is gone.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let verdict = placement_file(&home, "FORK");
    let first = start(
        &lab,
        &home,
        "feat/gamma",
        &["--placement", verdict.to_str().expect("utf-8 path")],
    );
    assert!(first.status.success(), "{first:?}");
    let finished = knives_command(
        &lab.work,
        home.path(),
        lab.temp_path(),
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    )
    .env("KNIVES_OWNER", "agent-one")
    .output()
    .expect("run finish");
    assert!(finished.status.success(), "{finished:?}");

    // When: it is started again with no file.
    let again = start(&lab, &home, "feat/gamma", &[]);

    // Then: the ledger's note answers for it.
    assert!(again.status.success(), "{again:?}");
    assert!(
        lab.work
            .parent()
            .expect("parent")
            .join("feat-gamma")
            .exists(),
        "the workspace was not rebuilt"
    );
}

#[test]
fn include_refuses_a_new_member_without_a_verdict_and_a_consumer_one() {
    // Given: a cut release and a branch no composition has carried.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let before = release_parents(&lab, "release/2026-08-04");

    // When: included with no verdict behind it.
    let unplaced = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: refused with the member text, and the release is untouched.
    assert_eq!(unplaced.status.code(), Some(3), "{unplaced:?}");
    assert_eq!(
        String::from_utf8_lossy(&unplaced.stdout).trim_end(),
        format!("demo: {}", missing_member_refusal("feat/gamma"))
    );
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);

    // When: the verdict says the change belongs in the consumer.
    state_placement(&lab, &home, "feat/gamma", "CONSUMER");
    let consumer = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: refused with the consumer text.
    assert_eq!(consumer.status.code(), Some(3), "{consumer:?}");
    assert_eq!(
        String::from_utf8_lossy(&consumer.stdout).trim_end(),
        format!("demo: {CONSUMER_REFUSAL}")
    );
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);

    // When: a newer verdict rules it a fork member.
    state_placement(&lab, &home, "feat/gamma", "FORK");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: it joins.
    assert!(included.status.success(), "{included:?}");
    assert_eq!(
        release_parents(&lab, "release/2026-08-04").len(),
        before.len() + 1
    );
}

#[test]
fn a_member_some_cut_recorded_is_grandfathered_into_include_and_advance() {
    // Given: a release cut from alpha and beta — the cut event records both as
    // parents — with no verdict behind either, as every member predating the
    // gate stands. Beta is then dropped.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let dropped = knives_release(&lab, &home, &["drop", "feat/beta", "--why", "recut"]);
    assert!(dropped.status.success(), "{dropped:?}");

    // When: beta is included again, still with no verdict.
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);

    // Then: it is an existing member and comes back unasked.
    assert!(included.status.success(), "{included:?}");
    assert!(
        String::from_utf8_lossy(&included.stdout).contains("included feat/beta"),
        "{included:?}"
    );

    // And: a member that grows is advanced unasked — the repository ties its
    // tip to a current parent.
    lab::extend_branch(&lab, "feat/alpha", "alpha2.txt", "more alpha\n");
    let advanced = knives_release(&lab, &home, &["advance", "feat/alpha"]);
    assert!(advanced.status.success(), "{advanced:?}");
    assert!(
        String::from_utf8_lossy(&advanced.stdout).contains("advanced feat/alpha"),
        "{advanced:?}"
    );
}

#[test]
fn a_verdict_on_a_held_claim_is_recorded_on_resume() {
    // Given: a branch started with FORK and still held — the common case for a
    // re-verdict: the workspace is open and a pull request is about to be opened.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let fork = placement_file(&home, "FORK");
    let first = start(
        &lab,
        &home,
        "feat/gamma",
        &["--placement", fork.to_str().expect("utf-8 path")],
    );
    assert!(first.status.success(), "{first:?}");

    // When: the red-team re-rules it UPSTREAM and `start` is run again with it.
    let upstream = placement_file(&home, "UPSTREAM");
    let again = start(
        &lab,
        &home,
        "feat/gamma",
        &["--placement", upstream.to_str().expect("utf-8 path")],
    );

    // Then: the claim is resumed and the newest recorded verdict is UPSTREAM.
    assert!(again.status.success(), "{again:?}");
    assert!(
        String::from_utf8_lossy(&again.stdout).starts_with("resumed"),
        "{again:?}"
    );
    let entries = ledger(&home).entries().expect("read ledger");
    let newest = recorded(&entries, "feat/gamma")
        .expect("a verdict is recorded")
        .expect("and it reads");
    assert_eq!(newest.verdict, Verdict::Upstream);
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.kind == Kind::Note)
            .count(),
        2,
        "both verdicts are kept: {entries:?}"
    );
}

#[test]
fn a_consumer_verdict_on_an_existing_branch_is_recorded_and_says_finish() {
    // Given: a branch that exists — a bookmark in the checkout — with no verdict.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let consumer = placement_file(&home, "CONSUMER");

    // When: the red-team rules it CONSUMER.
    let output = start(
        &lab,
        &home,
        "feat/alpha",
        &["--placement", consumer.to_str().expect("utf-8 path")],
    );

    // Then: refused, the verdict is now the branch's newest, and the line says
    // how the branch is retired.
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(CONSUMER_REFUSAL), "{stderr}");
    assert!(
        stderr.contains("`knives finish feat/alpha` retires it"),
        "{stderr}"
    );
    assert!(!stderr.contains("no branch is started"), "{stderr}");
    let entries = ledger(&home).entries().expect("read ledger");
    assert_eq!(
        recorded(&entries, "feat/alpha")
            .expect("a verdict is recorded")
            .expect("and it reads")
            .verdict,
        Verdict::Consumer
    );
    assert!(
        !lab.work
            .parent()
            .expect("parent")
            .join("feat-alpha")
            .exists(),
        "a consumer verdict opened a workspace"
    );

    // And: a branch that does not exist is told so, and nothing is recorded.
    let absent = start(
        &lab,
        &home,
        "feat/gamma",
        &["--placement", consumer.to_str().expect("utf-8 path")],
    );
    assert_eq!(absent.status.code(), Some(2), "{absent:?}");
    assert!(
        String::from_utf8_lossy(&absent.stderr).contains("no branch is started for feat/gamma"),
        "{absent:?}"
    );
    assert!(recorded(&ledger(&home).entries().expect("read ledger"), "feat/gamma").is_none());
}

#[test]
fn advance_refuses_a_never_composed_branch_that_succeeds_a_member_by_ancestry() {
    // Given: a release cut from alpha alone, and a new bookmark on a child of
    // alpha's tip — succession by ancestry, which is how a member that grew
    // looks too, but this name no composition ever carried and no verdict names.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.jj_work(["new", "feat/alpha", "-m", "sneaky"]);
    std::fs::write(lab.work.join("sneaky.txt"), "x\n").expect("write sneaky");
    lab.jj_work(["bookmark", "create", "feat/sneaky", "-r", "@"]);
    lab.jj_work(["new"]);
    let before = release_parents(&lab, "release/2026-08-04");

    // When: it is advanced by name, and by a bare advance.
    for args in [&["advance", "feat/sneaky"][..], &["advance"][..]] {
        let output = knives_release(&lab, &home, args);

        // Then: refused as a first-time member; the release is untouched.
        assert_eq!(output.status.code(), Some(3), "{args:?}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.starts_with(&format!(
                "demo: {}\n",
                missing_member_refusal("feat/sneaky")
            )),
            "{args:?}: {stdout}"
        );
        assert!(
            stdout.contains("nothing advanced; 1 of 1 branch(es)"),
            "{args:?}: {stdout}"
        );
        assert_eq!(
            release_parents(&lab, "release/2026-08-04"),
            before,
            "{args:?}"
        );
    }

    // And: with a verdict behind it, the named advance moves the parent.
    state_placement(&lab, &home, "feat/sneaky", "FORK");
    let advanced = knives_release(&lab, &home, &["advance", "feat/sneaky"]);
    assert!(advanced.status.success(), "{advanced:?}");
    assert!(
        String::from_utf8_lossy(&advanced.stdout).contains("advanced feat/sneaky"),
        "{advanced:?}"
    );
}

#[test]
fn a_newer_consumer_verdict_refuses_even_a_grandfathered_member() {
    // Given: a release cut from alpha and beta, beta dropped, and then the
    // red-team ruling beta CONSUMER — newer than the cut that composed it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let dropped = knives_release(&lab, &home, &["drop", "feat/beta", "--why", "recut"]);
    assert!(dropped.status.success(), "{dropped:?}");
    let before = release_parents(&lab, "release/2026-08-04");
    state_placement(&lab, &home, "feat/beta", "CONSUMER");

    // When: beta is included again.
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);

    // Then: the newest verdict wins over the composition record.
    assert_eq!(included.status.code(), Some(3), "{included:?}");
    assert_eq!(
        String::from_utf8_lossy(&included.stdout).trim_end(),
        format!("demo: {CONSUMER_REFUSAL}")
    );
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);
}

#[test]
fn a_prose_placement_note_is_ordinary_and_an_unknown_verdict_names_the_branch_and_remedy() {
    // Given: a cut release and a branch whose only `placement:` note is
    // workflow prose — the shape ledgers held before the gate.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let notch = |text: &str| {
        let output = knives(
            &lab,
            &home,
            &["notch", "feat/gamma", "--repo", "demo", "-m", text],
        );
        assert!(output.status.success(), "{output:?}");
    };
    notch("placement: belongs upstream eventually; the fork member ships the fix now");

    // When: it is included.
    let prose = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: the note is no verdict, so the refusal is the missing-verdict one,
    // not an unreadable ledger.
    assert_eq!(prose.status.code(), Some(3), "{prose:?}");
    assert_eq!(
        String::from_utf8_lossy(&prose.stdout).trim_end(),
        format!("demo: {}", missing_member_refusal("feat/gamma"))
    );
    assert!(prose.stderr.is_empty(), "{prose:?}");

    // When: a note carries the marker with a verdict knives does not know.
    notch("placement: verdict: MAYBE\nalternative: none");
    let unknown = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: that is an error, and it names the branch and the way forward —
    // both lines the tool reads, since a one-line notch is itself refused —
    // and says it once.
    assert_eq!(unknown.status.code(), Some(3), "{unknown:?}");
    let stderr = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        stderr.contains("feat/gamma's newest placement note is not a verdict knives can read"),
        "{stderr}"
    );
    assert!(stderr.contains(&notch_remedy("feat/gamma")), "{stderr}");
    assert_eq!(
        stderr.matches("the first line was").count(),
        1,
        "the cause is printed once: {stderr}"
    );

    // And: a one-line notch — the verdict without its alternative — is the
    // same error, so the remedy must show both lines; following it works.
    notch("placement: verdict: FORK");
    let bare = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert_eq!(bare.status.code(), Some(3), "{bare:?}");
    assert!(
        String::from_utf8_lossy(&bare.stderr).contains("`alternative:` line"),
        "{bare:?}"
    );
    notch("placement: verdict: FORK\nalternative: none");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(included.status.success(), "{included:?}");
}

#[test]
fn include_gates_the_branch_whatever_spelling_names_its_commit() {
    // The gate is a branch gate, read from the bookmarks at the commit: a
    // remote-only ref (a colleague's pushed branch), the sha a branch's tip
    // happens to be, and a bare commit nothing names.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.foreign_origin_branch("main@origin", "pushed-fix", "remote\n");
    lab.fetch_work();
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let gamma = lab::commit_at(&lab, "feat/gamma");
    let before = release_parents(&lab, "release/2026-08-04");

    // When: the pushed branch is included by its remote spelling.
    let remote = knives_release(&lab, &home, &["include", "pushed-fix@origin"]);

    // Then: refused as the branch it is, by its branch name.
    assert_eq!(remote.status.code(), Some(3), "{remote:?}");
    assert_eq!(
        String::from_utf8_lossy(&remote.stdout).trim_end(),
        format!("demo: {}", missing_member_refusal("pushed-fix"))
    );

    // When: an unverdicted branch's tip is included by its sha.
    let by_sha = knives_release(&lab, &home, &["include", gamma.as_str()]);

    // Then: the sha is that branch, and refused as it.
    assert_eq!(by_sha.status.code(), Some(3), "{by_sha:?}");
    assert_eq!(
        String::from_utf8_lossy(&by_sha.stdout).trim_end(),
        format!("demo: {}", missing_member_refusal("feat/gamma"))
    );
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);

    // When: the pushed branch has a verdict and is included again.
    state_placement(&lab, &home, "pushed-fix", "FORK");
    let included = knives_release(&lab, &home, &["include", "pushed-fix@origin"]);

    // Then: it joins, and the record carries its branch name — so a later
    // track and advance find it an existing member.
    assert!(included.status.success(), "{included:?}");
    let entries = ledger(&home).entries().expect("read ledger");
    let recorded = knives::release_model::last_recorded_parents(&entries, "release/2026-08-04");
    assert!(
        recorded
            .iter()
            .any(|parent| parent.branches.iter().any(|name| name == "pushed-fix")),
        "the remote branch's name was not recorded: {recorded:?}"
    );
    assert!(knives::placement::composed(&entries, "pushed-fix"));
}

#[test]
fn a_member_of_a_release_with_no_parent_record_is_grandfathered_by_the_repository() {
    // Given: a release whose cut left no record behind — the ledger of a
    // release cut before parent records existed, or with no cut event at all —
    // and a member that has since grown.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    std::fs::remove_dir_all(home.path().join("ledger")).expect("forget the cut record");
    lab::extend_branch(&lab, "feat/alpha", "alpha2.txt", "more alpha\n");

    // When: it is advanced by name, with no verdict anywhere.
    let named = knives_release(&lab, &home, &["advance", "feat/alpha"]);

    // Then: the release in hand carries it — its old parent stands vacated
    // behind its tip — and it moves unasked.
    assert!(named.status.success(), "{named:?}");
    assert!(
        String::from_utf8_lossy(&named.stdout).contains("advanced feat/alpha"),
        "{named:?}"
    );

    // And: a bare advance treats the other member the same way.
    std::fs::remove_dir_all(home.path().join("ledger")).expect("forget the advance record");
    lab::extend_branch(&lab, "feat/beta", "beta2.txt", "more beta\n");
    let bare = knives_release(&lab, &home, &["advance"]);
    assert!(bare.status.success(), "{bare:?}");
    assert!(
        String::from_utf8_lossy(&bare.stdout).contains("advanced feat/beta"),
        "{bare:?}"
    );

    // But: a branch stacked on a member still at its tip is not that member,
    // whatever the ledger lacks.
    lab.jj_work(["new", "feat/alpha", "-m", "sneaky"]);
    std::fs::write(lab.work.join("sneaky.txt"), "x\n").expect("write sneaky");
    lab.jj_work(["bookmark", "create", "feat/sneaky", "-r", "@"]);
    lab.jj_work(["new"]);
    let sneaky = knives_release(&lab, &home, &["advance", "feat/sneaky"]);
    assert_eq!(sneaky.status.code(), Some(3), "{sneaky:?}");
    assert!(
        String::from_utf8_lossy(&sneaky.stdout).contains(&missing_member_refusal("feat/sneaky")),
        "{sneaky:?}"
    );
}

#[test]
fn advance_names_every_unplaced_branch_and_moves_nothing() {
    // Given: two members, each with an unverdicted branch stacked on its tip.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    for (base, name) in [
        ("feat/alpha", "feat/sneaky-a"),
        ("feat/beta", "feat/sneaky-b"),
    ] {
        lab.jj_work(["new", base, "-m", name]);
        std::fs::write(
            lab.work.join(format!("{}.txt", name.replace('/', "-"))),
            "x\n",
        )
        .expect("write stacked");
        lab.jj_work(["bookmark", "create", name, "-r", "@"]);
        lab.jj_work(["new"]);
    }
    let before = release_parents(&lab, "release/2026-08-04");

    // When: a bare advance finds both.
    let output = knives_release(&lab, &home, &["advance"]);

    // Then: every refused name is reported, nothing moves, and the summary
    // counts them.
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&missing_member_refusal("feat/sneaky-a")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&missing_member_refusal("feat/sneaky-b")),
        "{stdout}"
    );
    assert!(
        stdout.contains("nothing advanced; 2 of 2 branch(es) would enter release/2026-08-04"),
        "{stdout}"
    );
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);

    // And: with one placed, the other still holds everything back.
    state_placement(&lab, &home, "feat/sneaky-a", "FORK");
    let partial = knives_release(&lab, &home, &["advance"]);
    assert_eq!(partial.status.code(), Some(3), "{partial:?}");
    assert!(
        String::from_utf8_lossy(&partial.stdout)
            .contains("nothing advanced; 1 of 2 branch(es) would enter"),
        "{partial:?}"
    );
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);
}

#[test]
fn advance_from_refuses_an_unplaced_first_time_name() {
    // Given: a release with alpha, and a branch that is not a member rebuilt
    // with no history back to any parent.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let old_alpha = lab::commit_at(&lab, "feat/alpha");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let before = release_parents(&lab, "release/2026-08-04");

    // When: `--from` asserts gamma replaces alpha's parent, on the caller's
    // word alone.
    let output = knives_release(
        &lab,
        &home,
        &["advance", "feat/gamma", "--from", old_alpha.as_str()],
    );

    // Then: a first-time name is gated like an include; nothing moved.
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&missing_member_refusal("feat/gamma")),
        "{output:?}"
    );
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);
}
