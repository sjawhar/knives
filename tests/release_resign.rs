//! `knives release adopt` of a release re-signed or metaedited after a knives
//! write: the write's change, parents and tree under a new commit id, as
//! `jj describe`, `jj sign`, `jj metaedit` or a push that signs leave it.
//!
//! Where the write's event bears no `; unchecked:` mark and the write is
//! unpublished, the adopt records the new commit in one event that holds no
//! fix by hand, and says which commit it recorded, so another machine on the
//! same ledger reads the release as recorded once it has fetched it. It
//! changes no content, so a tag on the new commit, while the publish remote
//! does not hold it, refuses nothing, and a write that knives left
//! conflicted is adopted with its conflicts. A write is published where the
//! publish remote holds it, a tag is on it or a `record: published` note
//! names it, under every release scheme: a copy of it is nothing to adopt.
//! Where the event is marked, the adopt reads the copy, or the write itself,
//! as any release knives could not check: it may refuse it as published,
//! and otherwise measures it and records it. These tests drive only the
//! knives binary.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use std::path::Path;
use std::process::Output;

use knives::ids::CommitId;
use knives::ledger::{Entry, Kind, Ledger};
use lab::{
    Lab, UPSTREAM, commit_at, fixed_home, home_after_first_cut, knives_release, release_test_home,
    second_machine, share, shared_home, stdout, text,
};

/// The release every test here cuts and re-signs.
const RELEASE: &str = "release/2026-08-04";
/// The release cut after [`RELEASE`].
const NEXT: &str = "release/2026-08-05";
/// The fixed release branch of a registry that names one.
const FIXED: &str = "integration";
/// [`UPSTREAM`]'s `<owner>/<name>`: what a shared ledger keeps the fork under.
const SHARED_FORK: &str = "acme/demo";
/// What a release home's ledger keeps the fork under: its registry key.
const LOCAL_FORK: &str = "demo";

/// Every entry of `home`'s ledger of `fork`, in the order it reads them.
fn ledger_entries(home: &Path, fork: &str) -> Vec<Entry> {
    Ledger::at(home.join("ledger").join(fork))
        .read()
        .expect("read the ledger")
        .entries
}

/// The newest event `entries` hold about `name`.
fn newest_event<'a>(entries: &'a [Entry], name: &str) -> &'a Entry {
    entries
        .iter()
        .rev()
        .find(|entry| entry.kind == Kind::Event && entry.subject.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no event about {name}: {entries:?}"))
}

/// A lab with alpha and beta, each on a file of its own.
fn two_branches() -> Lab {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab
}

/// Re-describe the release `name` in the work checkout with `jj describe`,
/// keeping its message and adding a review line, as a person does: a new
/// commit with the same change, parents and tree. It passes
/// `--ignore-immutable`, which a pushed or tagged release needs. Returns
/// the new commit.
fn re_describe(lab: &Lab, name: &str) -> CommitId {
    let written = commit_at(lab, name);
    let message = format!(
        "{}\n\nReview: read by hand\n",
        lab.revision(&lab.work, name, "description")
    );
    lab.jj_work([
        "describe",
        "--ignore-immutable",
        "-r",
        name,
        "-m",
        message.as_str(),
    ]);
    let described = commit_at(lab, name);
    assert_ne!(described, written, "the describe made no new commit");
    described
}

/// Assert that `output`, an adopt of `rewritten`, the cut `cut` re-signed
/// or re-described, recorded it as the newest write: it exits 0 and names
/// both commits, and `after`, the ledger after it, holds one entry more
/// than the `before` it held: an event about the release whose evidence
/// starts with `rewritten` and which holds no file as a fix by hand.
fn assert_recorded(
    output: &Output,
    (cut, rewritten): (&CommitId, &CommitId),
    (before, after): (usize, &[Entry]),
) {
    let out = stdout(output);
    assert_eq!(output.status.code(), Some(0), "{}", text(output));
    assert!(
        out.contains(&format!(
            "demo: adopted {0} as the newest write of {RELEASE}, replacing the recorded cut {1}\n  \
             {0} is {1} re-signed or metaedited (same change, parents and tree): no content \
             changed\n  baseline: the recorded cut {1}\n  no file resolved by hand\n",
            rewritten.short(),
            cut.short()
        )),
        "{}",
        text(output)
    );
    assert_eq!(after.len(), before + 1, "{after:?}");
    let event = newest_event(after, RELEASE);
    assert_eq!(
        event.evidence.first().map(String::as_str),
        Some(rewritten.as_str()),
        "{event:?}"
    );
    assert!(
        event.text.contains(&format!(
            " parent(s): adopted: replaces the recorded cut {0}; measured against the recorded \
             cut {0}; parents: ",
            cut.short()
        )),
        "{event:?}"
    );
    assert!(event.fix_files.is_empty(), "{event:?}");
}

/// Assert that `output`, an adopt of `described`, the cut `written` of
/// `name` re-described while that cut is published, recorded nothing: it
/// exits 0 and says the release is the cut re-signed or metaedited, with
/// nothing to adopt, and the ledger `after` it is the one `before` it.
fn assert_nothing_to_adopt(
    output: &Output,
    (name, written, described): (&str, &CommitId, &CommitId),
    (before, after): (&[Entry], &[Entry]),
) {
    assert_eq!(output.status.code(), Some(0), "{}", text(output));
    assert!(
        stdout(output).contains(&format!(
            "demo: {name} is at {}, the recorded cut {} re-signed or metaedited (same change, \
             parents and tree); nothing to adopt\n",
            described.short(),
            written.short()
        )),
        "{}",
        text(output)
    );
    assert_eq!(after, before);
}

/// Assert that `after`, the ledger after an adopt of `adopted` under
/// [`NEXT`], holds one entry more than the `before` it held: an event whose
/// evidence starts with `adopted` and which bears no `; unchecked:` mark,
/// so the adopt ended the mark.
fn assert_mark_ended(adopted: &CommitId, (before, after): (usize, &[Entry])) {
    assert_eq!(after.len(), before + 1, "{after:?}");
    let event = newest_event(after, NEXT);
    assert_eq!(
        event.evidence.first().map(String::as_str),
        Some(adopted.as_str()),
        "{event:?}"
    );
    assert!(!event.text.contains("; unchecked:"), "{event:?}");
}

/// [`two_branches`] cut as [`RELEASE`]; then the ledger removed, so that
/// name has no recorded write; then [`NEXT`] cut over it, which knives
/// could not check, so its event ends `; unchecked: no record`. That cut
/// holds no hand change: its tree is the automerge of its members, the
/// baseline an adopt measures it against. Returns the lab, its home and the
/// cut.
fn a_marked_cut() -> (Lab, tempfile::TempDir, CommitId) {
    let lab = two_branches();
    let (home, _consumer) = home_after_first_cut(&lab);
    std::fs::remove_dir_all(home.path().join("ledger").join(LOCAL_FORK))
        .expect("remove the ledger");
    let cut = knives_release(&lab, &home, &["cut", NEXT]);
    assert!(
        stdout(&cut).contains(&format!("cut {NEXT} as")),
        "{}",
        text(&cut)
    );
    let entries = ledger_entries(home.path(), LOCAL_FORK);
    let event = newest_event(&entries, NEXT);
    assert!(event.text.ends_with("; unchecked: no record"), "{event:?}");
    let written = commit_at(&lab, NEXT);
    (lab, home, written)
}

/// Make the lab's second clone the fork's checkout on another machine: an
/// `upstream` remote at [`UPSTREAM`], as the work checkout has, every remote
/// fetched, and `branches` tracked on origin.
fn second_machine_checkout(lab: &Lab, branches: &[&str]) {
    let rewrite = format!("url.{}.insteadOf", lab.upstream.display());
    lab::git_output(&lab.second, ["config", rewrite.as_str(), UPSTREAM]);
    lab::git_output(&lab.second, ["remote", "add", "upstream", UPSTREAM]);
    lab.jj_at(&lab.second, ["git", "fetch", "--all-remotes"]);
    for &branch in branches {
        lab.jj_at(
            &lab.second,
            ["bookmark", "track", branch, "--remote", "origin"],
        );
    }
}

/// `knives <args>` from the lab's second clone, as the machine whose config
/// home is `home`.
fn knives_in_second(lab: &Lab, home: &Path, args: &[&str]) -> Output {
    lab::knives_command(&lab.second, home, lab.temp_path(), args)
        .output()
        .expect("run knives")
}

/// `knives --json audit demo --no-github` from the lab's second clone,
/// parsed. The registry names an unreachable origin, as every release home
/// does, and the audit reads live remote refs, so it runs against the lab's
/// own origin; the registry is put back afterwards.
fn audit_in_second(lab: &Lab, home: &Path) -> serde_json::Value {
    let registry = home.join("repos.toml");
    let configured = std::fs::read_to_string(&registry).expect("read the registry");
    std::fs::write(
        &registry,
        configured.replace(
            "origin = \"https://forge.invalid/acme/work.git\"",
            &format!("origin = \"{}\"", lab.temp_origin().display()),
        ),
    )
    .expect("point the registry at the lab's origin");
    let output = knives_in_second(lab, home, &["--json", "audit", "demo", "--no-github"]);
    std::fs::write(&registry, configured).expect("restore the registry");
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("audit emits JSON ({error}): {}", text(&output)))
}

/// Push the release, its members and the tag `v1.0.0` from the work
/// checkout to origin, the fork's publish remote.
fn publish(lab: &Lab) {
    for branch in ["feat/alpha", "feat/beta", RELEASE] {
        lab.push_branch(branch);
    }
    lab::git_output(&lab.work, ["push", "origin", "refs/tags/v1.0.0"]);
}

/// `knives ledger sweep` on the machine whose config home is `home`.
fn sweep(home: &Path) -> Output {
    lab::knives_command(home, home, home, &["ledger", "sweep"])
        .output()
        .expect("run knives ledger sweep")
}

#[test]
fn an_adopt_records_a_release_re_described_after_a_knives_write_and_another_machine_reads_it() {
    // Given: two machines sharing one ledger remote. Alpha, in the lab's work
    // checkout, cuts a release of two branches, then re-describes it with
    // `jj describe`, as a person adds a review line before the push.
    let lab = two_branches();
    let alpha = shared_home(&lab);
    let beta = second_machine(alpha.path());
    let (_remote_dir, remote) = lab::bare_remote();
    share(alpha.path(), "alpha", &remote);
    share(beta.path(), "beta", &remote);
    let cut = knives_release(&lab, &alpha, &["cut", RELEASE]);
    assert!(cut.status.success(), "{}", text(&cut));
    let written = commit_at(&lab, RELEASE);
    let described = re_describe(&lab, RELEASE);
    let before = ledger_entries(alpha.path(), SHARED_FORK).len();

    // When: alpha adopts it.
    let adopted = knives_release(&lab, &alpha, &["adopt", RELEASE]);

    // Then: the cut is unpublished, so one event records the described
    // commit, with no fix by hand.
    let after = ledger_entries(alpha.path(), SHARED_FORK);
    assert_recorded(&adopted, (&written, &described), (before, &after));

    // And when: alpha tags the release and pushes it with its members and
    // the tag; beta, in the lab's second clone, sweeps the ledger and
    // fetches them.
    lab.jj_work(["tag", "set", "v1.0.0", "-r", RELEASE]);
    publish(&lab);
    second_machine_checkout(&lab, &["feat/alpha", "feat/beta"]);
    let swept = sweep(beta.path());
    assert!(swept.status.success(), "{}", text(&swept));

    // Then: beta's audit reads the release as recorded: no problem but the
    // skipped pull heads, so no `could not compare`, and no drift.
    let report = audit_in_second(&lab, beta.path());
    assert_eq!(
        report["problems"],
        serde_json::json!(["open pull-head reconciliation was skipped (--no-github)"]),
        "{report}"
    );
    assert!(
        report["findings"]
            .as_array()
            .expect("findings")
            .iter()
            .all(|finding| finding["kind"] != "release-drift"),
        "{report}"
    );

    // And: beta's cut of a next name is not refused for a recorded write it
    // does not hold.
    let next = knives_in_second(
        &lab,
        beta.path(),
        &["--text", "release", "--repo", "demo", "cut", NEXT],
    );
    assert_eq!(next.status.code(), Some(0), "{}", text(&next));
    assert!(
        stdout(&next).contains(&format!("cut {NEXT} as")),
        "{}",
        text(&next)
    );
}

#[test]
fn an_adopt_records_a_re_described_release_tagged_before_its_push() {
    // Given: a cut re-described with `jj describe`, then tagged before its
    // push: the tag is on the new commit, not on the cut, and the publish
    // remote holds neither.
    let lab = two_branches();
    let (home, _consumer) = home_after_first_cut(&lab);
    let cut = commit_at(&lab, RELEASE);
    let described = re_describe(&lab, RELEASE);
    lab.jj_work(["tag", "set", "v1.0.0", "-r", RELEASE]);
    let before = ledger_entries(home.path(), LOCAL_FORK).len();

    // When: it is adopted.
    let output = knives_release(&lab, &home, &["adopt", RELEASE]);

    // Then: the cut is unpublished, and the new commit changes no content,
    // so it is no hand edit of a published release: it is recorded as the
    // newest write, with no published remedy.
    let after = ledger_entries(home.path(), LOCAL_FORK);
    assert_recorded(&output, (&cut, &described), (before, &after));
    assert!(
        !stdout(&output).contains("is published"),
        "{}",
        text(&output)
    );
}

#[test]
fn an_adopt_records_a_re_signed_write_that_holds_a_conflict() {
    // Given: a first cut, whose event bears no `; unchecked:` mark, that
    // left shared.txt conflicted; then re-described with `jj describe`: its
    // tree, the conflict included, is the cut's own.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", RELEASE]);
    assert!(
        stdout(&cut).contains(&format!("cut {RELEASE} as")),
        "{}",
        text(&cut)
    );
    let entries = ledger_entries(home.path(), LOCAL_FORK);
    let event = newest_event(&entries, RELEASE);
    assert!(!event.text.contains("; unchecked:"), "{event:?}");
    let written = commit_at(&lab, RELEASE);
    let described = re_describe(&lab, RELEASE);
    assert_eq!(lab.revision(&lab.work, RELEASE, "conflict"), "true");

    // When: it is adopted.
    let output = knives_release(&lab, &home, &["adopt", RELEASE]);

    // Then: its conflicts are the recorded write's own, so it is not refused
    // as unresolved: it is recorded as the newest write, and exits 0.
    let after = ledger_entries(home.path(), LOCAL_FORK);
    assert_recorded(&output, (&written, &described), (entries.len(), &after));
    assert!(!stdout(&output).contains("still has"), "{}", text(&output));
}

#[test]
fn an_adopt_of_a_pushed_write_re_described_records_nothing() {
    // Given: a cut pushed to the publish remote, then re-described.
    let lab = two_branches();
    let (home, _consumer) = home_after_first_cut(&lab);
    let written = commit_at(&lab, RELEASE);
    lab.push_branch(RELEASE);
    let described = re_describe(&lab, RELEASE);
    let before = ledger_entries(home.path(), LOCAL_FORK);

    // When: it is adopted.
    let output = knives_release(&lab, &home, &["adopt", RELEASE]);

    // Then: the publish remote holds the cut, so the cut is published, and
    // the new commit is nothing to adopt.
    let after = ledger_entries(home.path(), LOCAL_FORK);
    assert_nothing_to_adopt(&output, (RELEASE, &written, &described), (&before, &after));
}

#[test]
fn an_adopt_of_a_tagged_write_re_described_records_nothing() {
    // Given: a cut tagged, then re-described past the immutability the tag
    // gives it. The tag stays on the cut.
    let lab = two_branches();
    let (home, _consumer) = home_after_first_cut(&lab);
    let written = commit_at(&lab, RELEASE);
    lab.jj_work(["tag", "set", "v1.0.0", "-r", RELEASE]);
    let described = re_describe(&lab, RELEASE);
    assert_eq!(commit_at(&lab, "v1.0.0"), written, "the tag left the cut");
    let before = ledger_entries(home.path(), LOCAL_FORK);

    // When: it is adopted.
    let output = knives_release(&lab, &home, &["adopt", RELEASE]);

    // Then: a tag is on the cut, so the cut is published, and the new
    // commit is nothing to adopt.
    let after = ledger_entries(home.path(), LOCAL_FORK);
    assert_nothing_to_adopt(&output, (RELEASE, &written, &described), (&before, &after));
}

#[test]
fn an_adopt_of_a_write_recorded_published_re_described_records_nothing() {
    // Given: a cut that a `record: published` note names, then
    // re-described.
    let lab = two_branches();
    let (home, _consumer) = home_after_first_cut(&lab);
    let written = commit_at(&lab, RELEASE);
    let noted = lab::knives(
        &lab,
        &home,
        &[
            "notch",
            RELEASE,
            "-m",
            "record: published as v1.0.0",
            "--evidence",
            written.as_str(),
            "--repo",
            "demo",
        ],
    );
    assert!(noted.status.success(), "{}", text(&noted));
    let described = re_describe(&lab, RELEASE);
    let before = ledger_entries(home.path(), LOCAL_FORK);

    // When: it is adopted.
    let output = knives_release(&lab, &home, &["adopt", RELEASE]);

    // Then: the note names the cut, so the cut is published, and the new
    // commit is nothing to adopt.
    let after = ledger_entries(home.path(), LOCAL_FORK);
    assert_nothing_to_adopt(&output, (RELEASE, &written, &described), (&before, &after));
}

#[test]
fn an_adopt_of_a_tagged_fixed_release_re_described_records_nothing() {
    // Given: a fixed release cut and tagged, then re-described past the
    // immutability the tag gives it. The tag stays on the cut.
    let lab = two_branches();
    lab.jj_work(["new", "main@origin"]);
    let home = fixed_home(&lab, FIXED);
    let cut = knives_release(&lab, &home, &["cut"]);
    assert!(
        stdout(&cut).contains(&format!("cut {FIXED} as")),
        "{}",
        text(&cut)
    );
    let written = commit_at(&lab, FIXED);
    lab.jj_work(["tag", "set", "v1.0.0", "-r", FIXED]);
    let described = re_describe(&lab, FIXED);
    assert_eq!(commit_at(&lab, "v1.0.0"), written, "the tag left the cut");
    let before = ledger_entries(home.path(), LOCAL_FORK);

    // When: it is adopted.
    let output = knives_release(&lab, &home, &["adopt", FIXED]);

    // Then: a tag is on the cut, so the cut is published under a fixed
    // scheme too, and the new commit is nothing to adopt.
    let after = ledger_entries(home.path(), LOCAL_FORK);
    assert_nothing_to_adopt(&output, (FIXED, &written, &described), (&before, &after));
}

#[test]
fn a_marked_re_signed_write_only_the_publish_remote_holds_is_measured_and_recorded() {
    // Given: a marked cut pushed to the publish remote, then re-described
    // here, so only the publish remote holds the cut itself.
    let (lab, home, written) = a_marked_cut();
    lab.push_branch(NEXT);
    let described = re_describe(&lab, NEXT);
    let before = ledger_entries(home.path(), LOCAL_FORK).len();

    // When: the new commit is adopted.
    let output = knives_release(&lab, &home, &["adopt", NEXT]);

    // Then: the mark sends the adopt the way of any release knives could
    // not check. Nothing marks the release published, so the new commit is
    // measured against an automerge of its members and recorded, which ends
    // the mark. The adopt does not say that no content changed.
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(
        out.contains(&format!(
            "demo: adopted {} as the newest write of {NEXT}, replacing the recorded cut {}\n  \
             baseline: an automerge of its 2 member parent(s)",
            described.short(),
            written.short()
        )),
        "{}",
        text(&output)
    );
    assert!(!out.contains("no content changed"), "{}", text(&output));
    let after = ledger_entries(home.path(), LOCAL_FORK);
    assert_mark_ended(&described, (before, &after));
}

#[test]
fn a_marked_re_signed_write_with_a_tag_on_the_publish_remotes_commit_is_refused_as_published() {
    // Given: a marked cut pushed to the publish remote, then re-described
    // here, and a tag on the cut, the publish remote's commit.
    let (lab, home, written) = a_marked_cut();
    lab.push_branch(NEXT);
    let described = re_describe(&lab, NEXT);
    lab.jj_work(["tag", "set", "v1.0.0", "-r", written.as_str()]);
    let before = ledger_entries(home.path(), LOCAL_FORK);

    // When: the new commit is adopted.
    let output = knives_release(&lab, &home, &["adopt", NEXT]);

    // Then: the tag marks the release published, and the publish remote
    // does not hold the new commit, so the adopt refuses it with the
    // published remedy and records nothing.
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!(
            "demo: {NEXT} is published (tag v1.0.0 names {}), so a hand edit cannot be recorded \
             under it: ",
            written.short()
        )),
        "{}",
        text(&output)
    );
    assert_eq!(commit_at(&lab, NEXT), described);
    assert_eq!(ledger_entries(home.path(), LOCAL_FORK), before);
}

#[test]
fn an_adopt_of_a_marked_write_itself_records_it() {
    // Given: a marked cut, as knives wrote it.
    let (lab, home, written) = a_marked_cut();
    let before = ledger_entries(home.path(), LOCAL_FORK).len();

    // When: it is adopted.
    let output = knives_release(&lab, &home, &["adopt", NEXT]);

    // Then: it is measured and recorded as the newest write, which ends the
    // mark.
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!(
            "demo: adopted {0} as the newest write of {NEXT}, replacing the recorded cut {0}\n",
            written.short()
        )),
        "{}",
        text(&output)
    );
    let after = ledger_entries(home.path(), LOCAL_FORK);
    assert_mark_ended(&written, (before, &after));
}
