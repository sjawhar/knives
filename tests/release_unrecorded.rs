//! Writes over a release in hand that carries a change knives did not write
//! or adopt (T5 (q2)–(q21)).
//!
//! The release in hand is unrecorded when the recorded write its members
//! match is in this repository and its tree is not that write rebased onto
//! them: `cut`, `include`, `drop`, `advance` and `rebase` then refuse before
//! they write, naming `knives release adopt`, or the published remedy for a
//! published release the publish remote does not hold. Where the matched
//! write is absent, no recorded write has the release's members, or the
//! ledger records no write of the name, knives cannot check it: the write
//! proceeds, says so, marks its ledger event `; unchecked: …`, and audit
//! notes it; every later write over it is marked too until `adopt` records
//! it. A member rewritten in place, a fast-forward carrier, a reset to an
//! earlier recorded write and a name with no recorded write are not refused.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use std::path::{Path, PathBuf};

use knives::ids::CommitId;
use knives::jj::Repo;
use knives::ledger::{Entry, Kind};
use lab::{
    Lab, ReleaseOutput, commit_at, extend_branch, file_at_revision, home_after_first_cut,
    knives_release, operation_ids, release_command, release_parents, release_test_home,
};

const DATED: &str = "release/2026-08-04";
const NEXT: &str = "release/2026-08-05";
const FIXED: &str = "integration";

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

/// A release verb run in checkout `dir`, whatever its exit.
fn release_in(
    lab: &Lab,
    home: &tempfile::TempDir,
    dir: &Path,
    args: &[&str],
) -> std::process::Output {
    release_command(lab, home, ReleaseOutput::Text, args)
        .current_dir(dir)
        .output()
        .expect("run knives release")
}

/// A release verb run in checkout `dir` that must go through: it is neither
/// refused nor a usage error, though the plan around it may exit with
/// findings of its own (a lagging trunk, a consumer pin). Its stdout.
fn ok_in(lab: &Lab, home: &tempfile::TempDir, dir: &Path, args: &[&str]) -> String {
    let output = release_in(lab, home, dir, args);
    assert!(
        matches!(output.status.code(), Some(0 | 1)),
        "{args:?} did not go through: {}",
        text(&output)
    );
    stdout(&output)
}

/// [`ok_in`] in the work checkout.
fn release_ran(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> String {
    ok_in(lab, home, &lab.work, args)
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

/// A registry home whose entry releases on the fixed branch `integration`,
/// with an empty local consumer.
fn fixed_home(lab: &Lab) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\n\
             release_branch = \"{FIXED}\"\n",
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

/// `knives --json audit demo --no-github` run in checkout `dir`, parsed,
/// against the lab's own origin with the same ledger; the registry is put
/// back afterwards.
fn audit_in(lab: &Lab, home: &tempfile::TempDir, dir: &Path) -> serde_json::Value {
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
    let output = lab::knives_command(
        dir,
        home.path(),
        lab.temp_path(),
        &["--json", "audit", "demo", "--no-github"],
    )
    .output()
    .expect("run knives audit");
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

/// Whether the audit notes something holding every one of `parts`; a report
/// with no notes holds none.
fn notes_hold(report: &serde_json::Value, parts: &[&str]) -> bool {
    report["notes"].as_array().is_some_and(|notes| {
        notes
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|note| parts.iter().all(|part| note.contains(part)))
    })
}

/// Step the working copy of `dir` off every release and member.
fn step_off_onto_trunk_in(lab: &Lab, dir: &Path) {
    lab.jj_at(dir, ["new", "main@origin"]);
}

fn step_off_onto_trunk(lab: &Lab) {
    step_off_onto_trunk_in(lab, &lab.work);
}

/// The commit `revision` resolves to in checkout `dir`.
fn commit_in(dir: &Path, revision: &str) -> CommitId {
    Repo::open(dir)
        .expect("open a checkout")
        .resolve_commit(revision)
        .expect("resolve a revision")
}

/// Edit release `name` in checkout `dir` in place, as a person finishing it
/// does: `jj edit`, the files written, the working copy stepped off.
fn edit_in(lab: &Lab, dir: &Path, name: &str, files: &[(&str, &str)]) {
    lab.jj_at(dir, ["edit", name]);
    for (file, content) in files {
        std::fs::write(dir.join(file), content).expect("write the edit");
    }
    step_off_onto_trunk_in(lab, dir);
}

fn edit_by_hand(lab: &Lab, name: &str, files: &[(&str, &str)]) {
    edit_in(lab, &lab.work, name, files);
}

/// A branch `name` forked from the fork's trunk in checkout `dir`, writing
/// `<last part of name>.txt`.
fn branch_in(lab: &Lab, dir: &Path, name: &str) {
    let short = name.rsplit('/').next().unwrap_or(name);
    lab.jj_at(dir, ["new", "-r", "main@origin", "-m", name]);
    std::fs::write(dir.join(format!("{short}.txt")), format!("{short}\n"))
        .expect("write the branch");
    lab.jj_at(dir, ["bookmark", "create", name, "-r", "@"]);
    step_off_onto_trunk_in(lab, dir);
}

/// `release/2026-08-04` cut from the lab's branches, which conflict: the cut
/// is written with its conflicts, whatever its exit says about them.
fn conflicted_first_cut(lab: &Lab) -> tempfile::TempDir {
    let (home, _consumer) = release_test_home(lab);
    let cut = knives_release(lab, &home, &["cut", DATED]);
    assert!(
        stdout(&cut).contains(&format!("cut {DATED} as")),
        "the first cut was refused: {}",
        text(&cut)
    );
    home
}

/// The second clone made a checkout of the fork as the work checkout is:
/// given the upstream remote and everything both remotes hold now.
fn second_clone(lab: &Lab) -> PathBuf {
    let second = lab.second.clone();
    let upstream = lab.upstream.display().to_string();
    lab.jj_at(
        &second,
        ["git", "remote", "add", "upstream", upstream.as_str()],
    );
    lab.jj_at(&second, ["git", "fetch", "--all-remotes"]);
    second
}

/// A new jj clone of the lab's origin at `name` in the lab's directory,
/// made a checkout of the fork as [`second_clone`] makes one.
fn fresh_clone(lab: &Lab, name: &str) -> PathBuf {
    let clone = lab.temp_path().join(name);
    let (origin, path) = (
        lab.temp_origin().display().to_string(),
        clone.display().to_string(),
    );
    lab.jj_at(
        lab.temp_path(),
        ["git", "clone", origin.as_str(), path.as_str()],
    );
    lab.jj_at(
        &clone,
        ["config", "set", "--repo", "user.name", "Knives Lab"],
    );
    lab.jj_at(
        &clone,
        [
            "config",
            "set",
            "--repo",
            "user.email",
            "knives-lab@example.test",
        ],
    );
    let upstream = lab.upstream.display().to_string();
    lab.jj_at(
        &clone,
        ["git", "remote", "add", "upstream", upstream.as_str()],
    );
    lab.jj_at(&clone, ["git", "fetch", "--all-remotes"]);
    clone
}

/// The fork recipe's fast-forward in checkout `dir`: the release rebased onto
/// its parents and the publish remote's commit of it, its tree put back.
/// Returns the write it rebased.
fn fast_forward_in(lab: &Lab, dir: &Path, name: &str) -> CommitId {
    let before = commit_in(dir, name);
    let destination = format!("parents({name}) | {name}@origin");
    lab.jj_at(dir, ["rebase", "-r", name, "-d", destination.as_str()]);
    lab.jj_at(dir, ["restore", "--from", before.as_str(), "--into", name]);
    before
}

fn fast_forward(lab: &Lab, name: &str) -> CommitId {
    fast_forward_in(lab, &lab.work, name)
}

/// Assert `output` is a refusal that names `knives release adopt <name>` for
/// an unrecorded release in hand.
fn assert_refused_naming_adopt(output: &std::process::Output, name: &str) {
    let out = stdout(output);
    assert_eq!(output.status.code(), Some(3), "{}", text(output));
    assert!(
        out.contains("carries a change no knives write or adopt recorded"),
        "{out}"
    );
    assert!(
        out.contains(&format!("`knives release adopt {name}`")),
        "{out}"
    );
}

/// Why knives could not check a release in hand, as the line, the ledger
/// mark and the audit note say it.
#[derive(Clone, Copy)]
enum Why {
    WriteAbsent,
    NoMatch,
    /// Unchecked since a marked write the release in hand is, or stands on,
    /// first marked because its matched write was absent.
    SinceWriteAbsent,
    /// The ledger records no write of the name.
    NoRecord,
    /// Unchecked since a marked write the release in hand is, or stands on,
    /// first marked because the ledger recorded no write of the name.
    SinceNoRecord,
}

impl Why {
    const fn line(self) -> &'static str {
        match self {
            Self::WriteAbsent => "is not in this repository; knives did not fetch",
            Self::NoMatch => {
                "its members match no recorded write, so they changed outside knives; knives did \
                 not fetch"
            }
            Self::SinceWriteAbsent => "the release in hand is unchecked since the recorded edit",
            Self::NoRecord => {
                "against its recorded writes: the ledger records no write of the name; knives \
                 did not fetch; a write over it is marked unchecked until `knives release \
                 adopt` records the name it writes"
            }
            Self::SinceNoRecord => "(no record); knives did not fetch; `knives release adopt",
        }
    }

    const fn mark(self) -> &'static str {
        match self {
            Self::WriteAbsent | Self::SinceWriteAbsent => "W absent",
            Self::NoMatch => "no match",
            Self::NoRecord | Self::SinceNoRecord => "no record",
        }
    }
}

/// Assert a write over an unchecked release in hand went through, said so in
/// one line, and marked the event the ledger records under `subject`.
fn assert_unchecked(
    out: &str,
    (release, subject): (&str, &str),
    home: &tempfile::TempDir,
    why: Why,
) {
    assert!(
        out.contains(&format!("demo: knives could not check {release} at ")),
        "{out}"
    );
    assert!(out.contains(why.line()), "{out}");
    let event = newest_event(home, subject);
    assert!(
        event
            .text
            .ends_with(&format!("; unchecked: {}", why.mark())),
        "{}",
        event.text
    );
}

#[test]
fn every_write_over_an_unrecorded_fixed_release_refuses_until_it_is_adopted() {
    // Given (q2): a fixed release cut, a member included, then the release
    // edited by hand, and work for each verb to do: a member to include, one
    // to drop, one advanced, and an upstream that moved.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    release_ran(&lab, &home, &["include", "feat/gamma"]);
    edit_by_hand(&lab, FIXED, &[("notes.txt", "by hand\n")]);
    let edited = commit_at(&lab, FIXED);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha, grown\n");
    step_off_onto_trunk(&lab);
    lab.advance_upstream("advance\n");
    let (ledger, operations) = (ledger_entries(&home), operation_ids(&lab.work));

    // When/Then: the cut and each edit verb refuse, naming adopt, and write
    // nothing.
    for args in [
        vec!["cut"],
        vec!["include", "feat/delta"],
        vec!["drop", "feat/beta", "--why", "a test"],
        vec!["advance", "feat/alpha"],
        vec!["rebase", "main@upstream"],
    ] {
        let output = knives_release(&lab, &home, &args);
        assert_refused_naming_adopt(&output, FIXED);
        assert_eq!(commit_at(&lab, FIXED), edited, "{args:?} moved the release");
    }
    assert_eq!(ledger_entries(&home), ledger);
    assert_eq!(operation_ids(&lab.work), operations);

    // When: the hand edit is adopted.
    release_ran(&lab, &home, &["adopt", "--why", "a note added by hand"]);

    // Then: the writes go through, the edit kept.
    release_ran(&lab, &home, &["include", "feat/delta"]);
    assert_eq!(file_at_revision(&lab, FIXED, "notes.txt"), "by hand\n");
    release_ran(&lab, &home, &["cut"]);
}

#[test]
fn a_fixed_cut_over_a_hand_edit_of_a_held_write_is_refused_until_adopted() {
    // Given (q21): a fixed release cut, then edited by hand, its cut held
    // here.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    edit_by_hand(&lab, FIXED, &[("alpha.txt", "alpha, repaired\n")]);
    let ledger = ledger_entries(&home);

    // When: it is cut again.
    let refused = knives_release(&lab, &home, &["cut"]);

    // Then: refused, naming adopt; nothing launders the edit.
    assert_refused_naming_adopt(&refused, FIXED);
    assert_eq!(ledger_entries(&home), ledger);

    // When: it is adopted, then cut.
    release_ran(&lab, &home, &["adopt", "--why", "a repair"]);
    let out = release_ran(&lab, &home, &["cut"]);

    // Then: the cut writes and holds the repair.
    assert!(out.contains(&format!("cut {FIXED} as")), "{out}");
    assert_eq!(
        file_at_revision(&lab, FIXED, "alpha.txt"),
        "alpha, repaired\n"
    );
}

#[test]
fn a_name_the_ledger_records_no_write_of_is_not_refused() {
    // Given (q3): the hand-resolved shape of a recut, with the ledger removed
    // after the cut, so the name has no recorded write.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    release_ran(&lab, &home, &["cut", DATED]);
    let ledger = home.path().join("ledger").join("demo");
    std::fs::remove_dir_all(&ledger).expect("remove the ledger");
    edit_by_hand(&lab, DATED, &[("shared.txt", "merged\n")]);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    let before = operation_ids(&lab.work);

    // When/Then: an edit over it is not refused, says knives could not check
    // it, and marks its event.
    let out = release_ran(&lab, &home, &["include", "feat/gamma"]);
    assert_unchecked(&out, (DATED, DATED), &home, Why::NoRecord);

    // When: from the same state, the composition is cut under a new name.
    lab.jj_work(["op", "restore", before[0].as_str()]);
    std::fs::remove_dir_all(&ledger).expect("remove the ledger again");
    let cut = release_in(&lab, &home, &lab.work, &["cut", NEXT]);
    let out = stdout(&cut);

    // Then: it is written, not refused, and exits 0, and the resolution is
    // carried; it says knives could not check the release in hand, and marks
    // its event.
    assert!(out.contains(&format!("cut {NEXT} as")), "{}", text(&cut));
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
    assert_eq!(file_at_revision(&lab, NEXT, "shared.txt"), "merged\n");
    assert_unchecked(&out, (DATED, NEXT), &home, Why::NoRecord);
    // And: nothing measured the divergence each member's row reports, so
    // the row says the ledger records no write of the release in hand and to
    // check it by hand, never that a recorded resolution was carried forward.
    for member in ["feat/alpha", "feat/beta"] {
        assert!(
            out.contains(&format!(
                "  {member}: the ledger records no write of the release in hand, so nothing \
                 measured its divergence from this member; check that divergence by hand\n"
            )),
            "{out}"
        );
    }
    assert!(!out.contains("diverges where"), "{out}");
    assert!(!out.contains("carried forward"), "{out}");
    assert!(!out.contains("recorded resolution"), "{out}");
}

/// A dated release of alpha, beta and gamma cut, alpha and beta both writing
/// `shared.txt`; then the ledger removed, so the name has no recorded write,
/// and the conflict resolved by hand. Returns the lab and its home.
fn a_release_with_no_recorded_write() -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let (home, _consumer) = release_test_home(&lab);
    release_ran(&lab, &home, &["cut", DATED]);
    std::fs::remove_dir_all(home.path().join("ledger").join("demo")).expect("remove the ledger");
    edit_by_hand(&lab, DATED, &[("shared.txt", "merged\n")]);
    (lab, home)
}

/// Assert each of alpha's and beta's carried rows in `out` says `row`.
fn assert_carried_rows(out: &str, row: &str) {
    for member in ["feat/alpha", "feat/beta"] {
        assert!(
            out.lines()
                .any(|line| line.starts_with(&format!("  {member}")) && line.ends_with(row)),
            "{member}: {out}"
        );
    }
}

/// Assert a write over [`DATED`], whose name the ledger records no write of,
/// went through, printing `out`, and that what it wrote, `written`, stays
/// unchecked until adopted: the write said knives could not check the
/// release and marked its event; a cut over `written` stays marked and names
/// no recorded write; `adopt` records that cut; the cut after it is checked.
/// `cuts` names the two cuts.
fn assert_marked_until_adopted(
    (lab, home): (&Lab, &tempfile::TempDir),
    out: &str,
    written: &str,
    cuts: [&str; 2],
) {
    // Then: the write says knives could not check the release in hand, why,
    // and what records it, and its event carries the mark.
    assert_unchecked(out, (DATED, written), home, Why::NoRecord);

    // When: what it wrote is cut.
    let sticky = release_ran(lab, home, &["cut", cuts[0]]);

    // Then: it stays unchecked: the cut says so, its rows say the release in
    // hand is unrecorded and never that it is a recorded write, and its
    // event keeps the mark.
    assert!(sticky.contains(&format!("cut {} as", cuts[0])), "{sticky}");
    assert!(
        sticky.contains("the release in hand is unchecked since the recorded"),
        "{sticky}"
    );
    assert_unchecked(&sticky, (written, cuts[0]), home, Why::SinceNoRecord);
    assert_carried_rows(
        &sticky,
        "diverges where the unrecorded in-hand release already does; record it with `knives \
         release adopt` and cut again",
    );
    assert!(!sticky.contains("(a recorded write)"), "{sticky}");

    // When: that cut is adopted.
    let adopted = release_ran(lab, home, &["adopt", cuts[0]]);

    // Then: it is recorded unmarked, measured against an automerge.
    assert!(adopted.contains("as the newest write of"), "{adopted}");
    assert!(
        adopted.contains("baseline: an automerge of its")
            && adopted.contains("because knives could not check the release the recorded cut"),
        "{adopted}"
    );
    assert!(!newest_event(home, cuts[0]).text.contains("unchecked"));

    // When: it is cut again.
    let clean = release_ran(lab, home, &["cut", cuts[1]]);

    // Then: that cut is checked, its rows carry a recorded write's
    // divergence forward, and its event carries no mark.
    assert!(clean.contains(&format!("cut {} as", cuts[1])), "{clean}");
    assert!(!clean.contains("could not check"), "{clean}");
    assert_carried_rows(
        &clean,
        "diverges where the release in hand (a recorded write) already does; carried forward",
    );
    assert!(!newest_event(home, cuts[1]).text.contains("unchecked"));
}

#[test]
fn an_include_over_a_name_with_no_recorded_write_is_marked_until_adopted() {
    // Given: a release whose name has no recorded write, and a branch.
    let (lab, home) = a_release_with_no_recorded_write();
    lab.branch("feat/delta", "delta.txt", "delta\n");
    step_off_onto_trunk(&lab);

    // When: the branch is included.
    let out = release_ran(&lab, &home, &["include", "feat/delta"]);

    // Then: marked, and every write after it too, until adopt.
    assert_marked_until_adopted((&lab, &home), &out, DATED, [NEXT, "release/2026-08-06"]);
}

#[test]
fn an_include_of_a_member_over_a_name_with_no_recorded_write_states_the_rule_and_records_nothing() {
    // Given: a release whose name has no recorded write, gamma among its
    // members.
    let (lab, home) = a_release_with_no_recorded_write();
    let (ledger, commit) = (ledger_entries(&home), commit_at(&lab, DATED));

    // When: gamma is included again.
    let output = release_in(&lab, &home, &lab.work, &["include", "feat/gamma"]);
    let out = stdout(&output);

    // Then: the line saying knives could not check the release states the
    // rule for a write over it, never that this include's write is marked;
    // the include says the release already carries gamma, exits 0, and
    // writes nothing, so the ledger holds no new event.
    assert!(
        out.contains(&format!(
            "demo: knives could not check {DATED} at {} {}\n",
            commit.short(),
            Why::NoRecord.line()
        )),
        "{}",
        text(&output)
    );
    assert!(!out.contains("the write is marked"), "{out}");
    assert!(
        out.contains(&format!("demo: {DATED} already carries feat/gamma\n")),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert_eq!(ledger_entries(&home), ledger);
}

#[test]
fn a_drop_over_a_name_with_no_recorded_write_is_marked_until_adopted() {
    // Given: a release whose name has no recorded write.
    let (lab, home) = a_release_with_no_recorded_write();

    // When: gamma, a member outside the resolved conflict, is dropped.
    let out = release_ran(&lab, &home, &["drop", "feat/gamma", "--why", "not ready"]);

    // Then: marked, and every write after it too, until adopt.
    assert_marked_until_adopted((&lab, &home), &out, DATED, [NEXT, "release/2026-08-06"]);
}

#[test]
fn an_advance_over_a_name_with_no_recorded_write_is_marked_until_adopted() {
    // Given: a release whose name has no recorded write, and gamma grown.
    let (lab, home) = a_release_with_no_recorded_write();
    extend_branch(&lab, "feat/gamma", "gamma.txt", "gamma, grown\n");
    step_off_onto_trunk(&lab);

    // When: gamma is advanced.
    let out = release_ran(&lab, &home, &["advance", "feat/gamma"]);

    // Then: marked, and every write after it too, until adopt.
    assert!(out.contains("advanced feat/gamma"), "{out}");
    assert_marked_until_adopted((&lab, &home), &out, DATED, [NEXT, "release/2026-08-06"]);
}

#[test]
fn a_rebase_over_a_name_with_no_recorded_write_is_marked_until_adopted() {
    // Given: a release whose name has no recorded write, and an upstream
    // that moved.
    let (lab, home) = a_release_with_no_recorded_write();
    lab.advance_upstream("advance\n");

    // When: the release is rebased onto it.
    let out = release_ran(&lab, &home, &["rebase", "main@upstream"]);

    // Then: marked, and every write after it too, until adopt.
    assert_marked_until_adopted((&lab, &home), &out, DATED, [NEXT, "release/2026-08-06"]);
}

#[test]
fn a_cut_over_a_name_with_no_recorded_write_is_marked_until_adopted() {
    // Given: a release whose name has no recorded write.
    let (lab, home) = a_release_with_no_recorded_write();

    // When: it is cut under a new name.
    let out = release_ran(&lab, &home, &["cut", NEXT]);

    // Then: written, and nothing measured the divergence each member's row
    // reports, so the row says to check it by hand.
    assert!(out.contains(&format!("cut {NEXT} as")), "{out}");
    assert_carried_rows(
        &out,
        "the ledger records no write of the release in hand, so nothing measured its divergence \
         from this member; check that divergence by hand",
    );
    assert!(!out.contains("(a recorded write)"), "{out}");

    // And: marked, and every write after it too, until adopt.
    assert_marked_until_adopted(
        (&lab, &home),
        &out,
        NEXT,
        ["release/2026-08-06", "release/2026-08-07"],
    );
}

#[test]
fn a_member_rewritten_in_place_is_not_refused() {
    // Given (q4): a cut, then one member reworded, which makes jj rebase the
    // release over its rewritten parent.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let cut = commit_at(&lab, DATED);
    lab.jj_work(["describe", "-r", "feat/alpha", "-m", "alpha, reworded"]);
    assert_ne!(commit_at(&lab, DATED), cut, "jj rebased the release");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);

    // When/Then: an include and then a cut go through, checked.
    let out = release_ran(&lab, &home, &["include", "feat/gamma"]);
    assert!(!out.contains("could not check"), "{out}");
    let out = release_ran(&lab, &home, &["cut", NEXT]);
    assert!(out.contains(&format!("cut {NEXT} as")), "{out}");
}

#[test]
fn a_fast_forward_pushed_without_adopt_is_not_refused_and_its_carrier_is_left_out() {
    // Given (q5): a fixed release cut and pushed, then a member included and
    // the write made a fast-forward of the pushed position and pushed, with
    // no adopt.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    let carrier = commit_at(&lab, FIXED);
    lab.push_branch(FIXED);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    release_ran(&lab, &home, &["include", "feat/gamma"]);
    fast_forward(&lab, FIXED);
    lab.push_branch(FIXED);
    assert!(release_parents(&lab, FIXED).contains(&carrier));
    lab.branch("feat/delta", "delta.txt", "delta\n");
    step_off_onto_trunk(&lab);

    // When/Then: an include goes through, checked, and leaves the carrier
    // out.
    let out = release_ran(&lab, &home, &["include", "feat/delta"]);
    assert!(!out.contains("could not check"), "{out}");
    assert!(!release_parents(&lab, FIXED).contains(&carrier));

    // And: so does the cut.
    release_ran(&lab, &home, &["cut"]);
    assert!(!release_parents(&lab, FIXED).contains(&carrier));
    assert_eq!(file_at_revision(&lab, FIXED, "delta.txt"), "delta\n");
}

/// A dated cut of alpha and beta made in the work checkout and never pushed,
/// its members pushed, and the release made again by hand in the second
/// clone from those members with `files` written: a release whose matched
/// write that clone does not hold. Returns the lab, its home, the clone and
/// the cut.
fn made_again_in_second_clone(
    files: &[(&str, &str)],
) -> (Lab, tempfile::TempDir, PathBuf, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let cut = commit_at(&lab, DATED);
    lab.push_branch("feat/alpha");
    lab.push_branch("feat/beta");
    let second = second_clone(&lab);
    lab.jj_at(
        &second,
        ["new", "feat/alpha@origin", "feat/beta@origin", "-m", DATED],
    );
    for (file, content) in files {
        std::fs::write(second.join(file), content).expect("write by hand");
    }
    lab.jj_at(&second, ["bookmark", "create", DATED, "-r", "@"]);
    step_off_onto_trunk_in(&lab, &second);
    assert!(
        Repo::open(&second)
            .expect("open")
            .resolve_commit(cut.as_str())
            .is_err()
    );
    (lab, home, second, cut)
}

#[test]
fn a_write_over_a_release_whose_matched_write_is_absent_is_unchecked_not_refused() {
    // Given (q6): the cut made in another clone and never pushed, and the
    // release in hand made from its members here, with a note by hand.
    let (lab, home, second, cut) = made_again_in_second_clone(&[("notes.txt", "by hand\n")]);
    branch_in(&lab, &second, "feat/gamma");

    // When: an include runs over it.
    let out = ok_in(&lab, &home, &second, &["include", "feat/gamma"]);

    // Then: it goes through, says knives could not check the release against
    // the absent cut, and marks its event; audit notes the write.
    assert_unchecked(&out, (DATED, DATED), &home, Why::WriteAbsent);
    assert!(
        out.contains(&format!("the recorded cut {}", cut.short())),
        "{out}"
    );
    let report = audit_in(&lab, &home, &second);
    assert!(
        notes_hold(&report, &[DATED, "could not check", "(W absent)"]),
        "{report}"
    );
}

#[test]
fn adopt_records_a_release_whose_matched_write_is_absent_and_the_next_write_is_checked() {
    // Given (q6): the same release in hand, its cut absent here.
    let (lab, home, second, _cut) = made_again_in_second_clone(&[("notes.txt", "by hand\n")]);
    branch_in(&lab, &second, "feat/gamma");

    // When: it is adopted, then included into.
    ok_in(
        &lab,
        &home,
        &second,
        &["adopt", "--why", "a note added by hand"],
    );
    let out = ok_in(&lab, &home, &second, &["include", "feat/gamma"]);

    // Then: the include is checked against the adopted write.
    assert!(!out.contains("could not check"), "{out}");
    assert!(!newest_event(&home, DATED).text.contains("unchecked"));
}

#[test]
fn the_live_shape_with_an_absent_write_and_a_carrier_is_unchecked_and_leaves_the_carrier_out() {
    // Given (q7): a fixed release cut and pushed, a member included in the
    // work checkout and never pushed, and in the second clone the release
    // made by hand from the include's members and the pushed cut, a carrier.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    let carrier = commit_at(&lab, FIXED);
    lab.push_branch(FIXED);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    release_ran(&lab, &home, &["include", "feat/gamma"]);
    lab.push_branch("feat/alpha");
    lab.push_branch("feat/gamma");
    let second = second_clone(&lab);
    lab.jj_at(
        &second,
        [
            "new",
            "feat/alpha@origin",
            "feat/gamma@origin",
            "integration@origin",
            "-m",
            FIXED,
        ],
    );
    lab.jj_at(&second, ["bookmark", "create", FIXED, "-r", "@"]);
    step_off_onto_trunk_in(&lab, &second);
    branch_in(&lab, &second, "feat/delta");

    // When: an include runs over it.
    let out = ok_in(&lab, &home, &second, &["include", "feat/delta"]);

    // Then: unchecked, not refused, and the carrier is left out.
    assert_unchecked(&out, (FIXED, FIXED), &home, Why::WriteAbsent);
    let parents = Repo::open(&second)
        .expect("open")
        .parent_commits(FIXED)
        .expect("parents");
    assert!(!parents.contains(&carrier), "{parents:?}");

    // And: the cut that follows goes through and stays unchecked: nothing
    // measured the include it duplicates.
    let out = ok_in(&lab, &home, &second, &["cut"]);
    assert!(
        out.contains("the release in hand is unchecked since"),
        "{out}"
    );
}

/// The fork recipe on a fixed release: cut and push, include, fast-forward
/// onto the publish ref, `adopt` when `adopt` says so, push; then the second
/// clone takes the pushed release. Returns the lab, its home and the clone.
fn recipe(adopt: bool) -> (Lab, tempfile::TempDir, PathBuf) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    lab.push_branch(FIXED);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    release_ran(&lab, &home, &["include", "feat/gamma"]);
    let pre = fast_forward(&lab, FIXED);
    if adopt {
        let why = format!("fast-forward of {} onto the published head", pre.short());
        release_ran(&lab, &home, &["adopt", "--why", why.as_str()]);
    }
    lab.push_branch(FIXED);
    let second = second_clone(&lab);
    lab.jj_at(&second, ["bookmark", "track", "integration@origin"]);
    branch_in(&lab, &second, "feat/delta");
    (lab, home, second)
}

#[test]
fn the_recipe_with_its_adopt_leaves_a_second_clone_checked_and_without_drift() {
    // Given (q8): the recipe with its adopt, and a second clone that never
    // held the write it replaced.
    let (lab, home, second) = recipe(true);

    // When: the clone includes and cuts.
    let included = ok_in(&lab, &home, &second, &["include", "feat/delta"]);
    let cut = ok_in(&lab, &home, &second, &["cut"]);

    // Then: neither is unchecked, and audit finds no drift.
    assert!(!included.contains("could not check"), "{included}");
    assert!(!cut.contains("could not check"), "{cut}");
    let report = audit_in(&lab, &home, &second);
    assert!(findings(&report, "release-drift").is_empty(), "{report}");
}

#[test]
fn the_recipe_without_its_adopt_leaves_a_second_clone_unchecked_not_refused() {
    // Given (q8 control): the recipe with no adopt.
    let (lab, home, second) = recipe(false);

    // When: the clone includes.
    let out = ok_in(&lab, &home, &second, &["include", "feat/delta"]);

    // Then: it goes through, unchecked: the write the push copied is absent.
    assert_unchecked(&out, (FIXED, FIXED), &home, Why::WriteAbsent);
}

/// A fixed release cut of alpha and beta (`W0`) and pushed; the second clone
/// takes it; the work checkout includes gamma (`W1`) and pushes a
/// fast-forward copy of it without adopt; the second clone, which never
/// fetches that copy, records a write in the shared ledger and never pushes
/// it: gamma's include when `same_members`, delta's otherwise. Returns the
/// lab and its home.
fn second_clone_write_never_pushed(same_members: bool) -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    lab.push_branch(FIXED);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    lab.push_branch("feat/gamma");
    let second = second_clone(&lab);
    lab.jj_at(&second, ["bookmark", "track", "integration@origin"]);
    release_ran(&lab, &home, &["include", "feat/gamma"]);
    fast_forward(&lab, FIXED);
    lab.push_branch(FIXED);
    if same_members {
        lab.jj_at(&second, ["bookmark", "track", "feat/gamma@origin"]);
        ok_in(&lab, &home, &second, &["include", "feat/gamma"]);
    } else {
        branch_in(&lab, &second, "feat/delta");
        ok_in(&lab, &home, &second, &["include", "feat/delta"]);
    }
    lab.branch("feat/eps", "eps.txt", "eps\n");
    step_off_onto_trunk(&lab);
    (lab, home)
}

#[test]
fn a_write_another_clone_recorded_on_other_members_leaves_the_published_head_checked() {
    // Given (q10): the second clone's unpushed write changed the parent set,
    // so the published head's members still match the work checkout's
    // include, which it holds.
    let (lab, home) = second_clone_write_never_pushed(false);

    // When/Then: the work checkout includes and cuts, checked.
    let out = release_ran(&lab, &home, &["include", "feat/eps"]);
    assert!(!out.contains("could not check"), "{out}");
    let out = release_ran(&lab, &home, &["cut"]);
    assert!(!out.contains("could not check"), "{out}");
}

#[test]
fn a_write_another_clone_recorded_on_the_same_members_leaves_the_head_unchecked() {
    // Given (q10 control): the second clone's unpushed write has the
    // published head's members, so it is the matched write, absent here.
    let (lab, home) = second_clone_write_never_pushed(true);

    // When: the work checkout includes.
    let out = release_ran(&lab, &home, &["include", "feat/eps"]);

    // Then: unchecked, not refused.
    assert_unchecked(&out, (FIXED, FIXED), &home, Why::WriteAbsent);
}

/// A fixed release cut of alpha and pushed, then the local name reset to the
/// publish remote's commit, the cut, past a second write never pushed:
/// `keep_parents` makes it an adopted hand edit on the same parents,
/// otherwise an include. Returns the lab, its home and the cut.
fn reset_to_the_published_cut(keep_parents: bool) -> (Lab, tempfile::TempDir, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    let cut = commit_at(&lab, FIXED);
    lab.push_branch(FIXED);
    if keep_parents {
        edit_by_hand(&lab, FIXED, &[("notes.txt", "by hand\n")]);
        release_ran(&lab, &home, &["adopt", "--why", "a note added by hand"]);
    } else {
        lab.branch("feat/gamma", "gamma.txt", "gamma\n");
        step_off_onto_trunk(&lab);
        release_ran(&lab, &home, &["include", "feat/gamma"]);
    }
    assert_ne!(commit_at(&lab, FIXED), cut);
    lab.jj_work([
        "bookmark",
        "set",
        FIXED,
        "-r",
        "integration@origin",
        "--allow-backwards",
    ]);
    assert_eq!(commit_at(&lab, FIXED), cut);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    step_off_onto_trunk(&lab);
    (lab, home, cut)
}

/// (q9): the reset head is not refused, and `adopt` records it as the
/// newest write, after which audit finds no drift.
fn a_reset_to_an_earlier_recorded_write(keep_parents: bool) {
    // Given: the reset.
    let (lab, home, cut) = reset_to_the_published_cut(keep_parents);
    let reset = operation_ids(&lab.work);

    // When/Then: an include is not refused.
    let out = release_ran(&lab, &home, &["include", "feat/delta"]);
    assert!(!out.contains("could not check"), "{out}");

    // When: from the reset again, it is adopted.
    lab.jj_work(["op", "restore", reset[0].as_str()]);
    let out = release_ran(&lab, &home, &["adopt"]);

    // Then: the reset head is recorded as the newest write, and the audit
    // finds no drift.
    assert!(
        out.contains(&format!("adopted {} as the newest write", cut.short())),
        "{out}"
    );
    assert_eq!(
        newest_event(&home, FIXED)
            .evidence
            .first()
            .map(String::as_str),
        Some(cut.as_str())
    );
    let report = audit_in(&lab, &home, &lab.work);
    assert!(findings(&report, "release-drift").is_empty(), "{report}");

    // And: the next write goes through, checked, then the cut.
    let out = release_ran(&lab, &home, &["include", "feat/delta"]);
    assert!(!out.contains("could not check"), "{out}");
    let out = release_ran(&lab, &home, &["cut"]);
    assert!(out.contains(&format!("cut {FIXED} as")), "{out}");
}

#[test]
fn a_reset_past_a_write_that_changed_the_parent_set_is_not_refused_and_adopts_clean() {
    a_reset_to_an_earlier_recorded_write(false);
}

#[test]
fn a_reset_past_a_write_that_kept_the_parent_set_is_not_refused_and_adopts_clean() {
    a_reset_to_an_earlier_recorded_write(true);
}

#[test]
fn a_member_reworded_after_the_write_is_recorded_by_the_recipes_adopt() {
    // Given (q11): a fixed release cut and pushed, a member included, then
    // the member reworded (jj rebases the release over it), then the
    // recipe's fast-forward onto the published position.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    lab.push_branch(FIXED);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    release_ran(&lab, &home, &["include", "feat/gamma"]);
    lab.jj_work(["describe", "-r", "feat/gamma", "-m", "gamma, reworded"]);
    let pre = fast_forward(&lab, FIXED);
    let release = commit_at(&lab, FIXED);

    // When: the recipe's adopt runs.
    let why = format!("fast-forward of {} onto the published head", pre.short());
    let out = release_ran(&lab, &home, &["adopt", "--why", why.as_str()]);

    // Then: the release is recorded as the newest write.
    assert!(
        out.contains(&format!("adopted {} as the newest write", release.short())),
        "{out}"
    );
    assert_eq!(
        newest_event(&home, FIXED)
            .evidence
            .first()
            .map(String::as_str),
        Some(release.as_str())
    );
}

#[test]
fn a_member_added_by_hand_is_unchecked_for_the_writes_and_refused_by_adopt() {
    // Given (q12): a dated cut, then a member added by hand outside knives.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.branch("feat/delta", "delta.txt", "delta\n");
    step_off_onto_trunk(&lab);
    let reset = operation_ids(&lab.work);
    lab.jj_work([
        "rebase",
        "-r",
        DATED,
        "-d",
        "parents(release/2026-08-04) | feat/gamma",
    ]);

    // When: adopt runs over it.
    let adopted = knives_release(&lab, &home, &["adopt"]);

    // Then: refused: the change belongs in a knives write that changes the
    // composition.
    assert_eq!(adopted.status.code(), Some(3), "{}", text(&adopted));
    assert!(
        stdout(&adopted).contains(&format!(
            "demo: {DATED}'s members differ from every recorded write; make the change with \
             `knives release include`, `drop` or `advance` (a member added by hand: drop it, then \
             include it); that write records the new composition, marked unchecked"
        )),
        "{}",
        text(&adopted)
    );

    // When: an include runs over it.
    let out = release_ran(&lab, &home, &["include", "feat/delta"]);

    // Then: it goes through, unchecked, with the mark.
    assert_unchecked(&out, (DATED, DATED), &home, Why::NoMatch);

    // When (control): from the cut again, the member is added by `include`.
    lab.jj_work(["op", "restore", reset[0].as_str()]);
    let out = release_ran(&lab, &home, &["include", "feat/gamma"]);

    // Then: checked.
    assert!(!out.contains("could not check"), "{out}");
}

#[test]
fn a_resolution_made_as_a_child_of_the_release_is_refused_naming_the_squash() {
    // Given (q12): a cut whose conflict is resolved in a new commit on top of
    // it, the release name moved onto it: its only parent is a carrier.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let home = conflicted_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let cut = commit_at(&lab, DATED);
    lab.jj_work(["new", DATED, "-m", "resolve shared.txt"]);
    std::fs::write(lab.work.join("shared.txt"), "alpha\nbeta\n").expect("resolve");
    lab.jj_work(["bookmark", "set", DATED, "-r", "@"]);
    step_off_onto_trunk(&lab);
    let child = commit_at(&lab, DATED);
    let squash = format!(
        "every parent of {DATED} is an earlier write: squash the resolution into the release \
         commit (`jj squash --from {} --into {} --use-destination-message`), then adopt",
        child.short(),
        cut.short()
    );
    let child_state = operation_ids(&lab.work);

    // When/Then: a write and adopt both refuse, naming the squash.
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert_eq!(included.status.code(), Some(3), "{}", text(&included));
    assert!(stdout(&included).contains(&squash), "{}", text(&included));
    let adopted = knives_release(&lab, &home, &["adopt"]);
    assert_eq!(adopted.status.code(), Some(3), "{}", text(&adopted));
    assert!(stdout(&adopted).contains(&squash), "{}", text(&adopted));

    // When (control): the squash is done into the release, then adopted.
    lab.jj_work([
        "squash",
        "--from",
        child.as_str(),
        "--into",
        cut.as_str(),
        "--use-destination-message",
    ]);
    release_ran(&lab, &home, &["adopt", DATED]);

    // Then: the write goes through, checked.
    let out = release_ran(&lab, &home, &["include", "feat/gamma"]);
    assert!(!out.contains("could not check"), "{out}");

    // When (control): from the child again, it is given one member parent
    // beside the carrier.
    lab.jj_work(["op", "restore", child_state[0].as_str()]);
    let destination = format!("{} | feat/gamma", cut.as_str());
    lab.jj_work(["rebase", "-r", DATED, "-d", destination.as_str()]);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    step_off_onto_trunk(&lab);
    let out = release_ran(&lab, &home, &["include", "feat/delta"]);

    // Then: it has a member to compose from, so the write goes through,
    // unchecked: its members match no recorded write.
    assert_unchecked(&out, (DATED, DATED), &home, Why::NoMatch);
}

/// A resolution made by hand as a commit on top of release `name` in the
/// work checkout, the name moved onto it. Returns the release commit it sits
/// on and the commit.
fn resolution_on_top(lab: &Lab, name: &str) -> (CommitId, CommitId) {
    let under = commit_at(lab, name);
    lab.jj_work(["new", name, "-m", "a resolution on top"]);
    std::fs::write(lab.work.join("notes.txt"), "by hand\n").expect("write the resolution");
    lab.jj_work(["bookmark", "set", name, "-r", "@"]);
    step_off_onto_trunk(lab);
    (under, commit_at(lab, name))
}

/// The release `name` moved onto its parent `under` and the member `gamma`,
/// from the commit on top: one member parent beside the carrier.
fn one_member_beside_the_carrier(lab: &Lab, name: &str, under: &CommitId) {
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.branch("feat/delta", "delta.txt", "delta\n");
    step_off_onto_trunk(lab);
    let destination = format!("{} | feat/gamma", under.as_str());
    lab.jj_work(["rebase", "-r", name, "-d", destination.as_str()]);
}

#[test]
fn a_resolution_on_top_of_a_published_fixed_release_names_the_squash_past_immutability() {
    // Given (q12): a fixed release cut and pushed, a resolution made on top
    // of it; the release commit it sits on is the publish remote's.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    lab.push_branch(FIXED);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    let (under, child) = resolution_on_top(&lab, FIXED);
    let before = operation_ids(&lab.work);

    // When: a write runs over it.
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: refused, naming the squash past immutability and the adopts.
    assert_eq!(included.status.code(), Some(3), "{}", text(&included));
    assert!(
        stdout(&included).contains(&format!(
            "every parent of {FIXED} is an earlier write: squash the resolution into the release \
             commit (`jj squash --from {} --into {} --use-destination-message \
             --ignore-immutable`, as that commit is published), then adopt, and adopt again \
             after the fast-forward rebase",
            child.short(),
            under.short()
        )),
        "{}",
        text(&included)
    );

    // When (control): one member parent beside the carrier.
    lab.jj_work(["op", "restore", before[0].as_str()]);
    lab.jj_work(["bookmark", "delete", "feat/gamma"]);
    one_member_beside_the_carrier(&lab, FIXED, &under);
    let out = release_ran(&lab, &home, &["include", "feat/delta"]);

    // Then: unchecked, not refused.
    assert_unchecked(&out, (FIXED, FIXED), &home, Why::NoMatch);
}

#[test]
fn a_resolution_on_top_of_a_tagged_dated_release_gets_the_published_remedy() {
    // Given (q12): a dated release pushed and tagged, a resolution made on
    // top of it and not pushed.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.push_branch(DATED);
    lab.jj_work(["tag", "set", "v1.0.0", "-r", DATED]);
    let (under, child) = resolution_on_top(&lab, DATED);
    let before = operation_ids(&lab.work);

    // When: the next cut runs.
    let cut = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: refused with the published remedy, from the cut the resolution
    // sits on (F19: the remote lacks it, so the cut states the drop and the
    // resolution is abandoned after; no double-cut note), and no squash.
    assert_eq!(cut.status.code(), Some(3), "{}", text(&cut));
    assert!(
        stdout(&cut).contains(&format!(
            "{DATED} is published (tag v1.0.0 names {}), so a hand edit cannot be recorded under \
             it: move {DATED} to {}, the recorded write the resolution sits on (`jj bookmark set \
             {DATED} -r {} --allow-backwards`), then if the working copy is on {} or on an empty \
             commit on it, `jj new 'trunk()'`, then cut a new dated name with `--allow-drop`, \
             which must print \
             `--allow-drop: nothing dropped` naming exactly {} (any other answer is a stop), \
             then `jj restore --from {} --into <new name>`, then adopt the new name, then `jj \
             abandon {}`, then `jj bookmark forget {DATED}` (never `jj bookmark delete`, which \
             a push would carry to the remote)",
            under.short(),
            under.short(),
            under.short(),
            child.short(),
            child.short(),
            child.short(),
            child.short()
        )),
        "{}",
        text(&cut)
    );
    assert!(!stdout(&cut).contains("jj squash"), "{}", text(&cut));
    assert!(
        !stdout(&cut).contains("that cut exits 1 once"),
        "{}",
        text(&cut)
    );

    // When (control): one member parent beside the carrier, then the cut.
    lab.jj_work(["op", "restore", before[0].as_str()]);
    one_member_beside_the_carrier(&lab, DATED, &under);
    let cut = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: no all-carrier refusal; the release is unchecked.
    assert!(
        stdout(&cut).contains("knives could not check"),
        "{}",
        text(&cut)
    );
    assert!(
        !stdout(&cut).contains("is an earlier write:"),
        "{}",
        text(&cut)
    );
}

#[test]
fn a_resolution_on_top_of_a_release_with_no_recorded_write_is_refused_to_run_again() {
    // Given (q12): a release merged by hand, no write of it recorded, pushed,
    // then a resolution made on top of it: its only parent is the publish
    // remote's commit of the name, a carrier.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(DATED, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.push_branch(DATED);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    let (under, child) = resolution_on_top(&lab, DATED);
    let before = operation_ids(&lab.work);

    // When: a write runs over it.
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: refused, naming the squash and the write to run again.
    assert_eq!(included.status.code(), Some(3), "{}", text(&included));
    assert!(
        stdout(&included).contains(&format!(
            "every parent of {DATED} is an earlier write: squash the resolution into the release \
             commit (`jj squash --from {} --into {} --use-destination-message \
             --ignore-immutable`, as that commit is published), then run the write again",
            child.short(),
            under.short()
        )),
        "{}",
        text(&included)
    );

    // When (control): one member parent beside the carrier.
    lab.jj_work(["op", "restore", before[0].as_str()]);
    lab.jj_work(["bookmark", "delete", "feat/gamma"]);
    one_member_beside_the_carrier(&lab, DATED, &under);
    let out = release_ran(&lab, &home, &["include", "feat/delta"]);

    // Then: a name with no recorded write is not refused: the write goes
    // through, says knives could not check the release, and marks its event.
    assert_unchecked(&out, (DATED, DATED), &home, Why::NoRecord);
}

/// A fixed release cut of alpha (grown to two commits) and beta, pushed
/// alone with `jj git push --bookmark integration`, then `rewrite`, a jj
/// command on alpha's tip, run in the work checkout and the release pushed
/// alone again; a fresh clone takes it. Returns the lab, its home and the
/// clone.
fn member_tip_rewritten_then_pushed_alone<const N: usize>(
    rewrite: [&str; N],
) -> (Lab, tempfile::TempDir, PathBuf) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    extend_branch(&lab, "feat/alpha", "alpha-more.txt", "more\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    lab.jj_work(["git", "push", "--bookmark", FIXED]);
    lab.jj_work(rewrite);
    lab.jj_work(["git", "push", "--bookmark", FIXED]);
    let second = second_clone(&lab);
    lab.jj_at(&second, ["bookmark", "track", "integration@origin"]);
    branch_in(&lab, &second, "feat/delta");
    (lab, home, second)
}

#[test]
fn a_member_tip_squashed_into_its_parent_is_unchecked_in_a_fresh_clone() {
    // Given (q20): alpha's tip squashed into its parent, which makes jj
    // rebase the release onto a commit of another change id; the release
    // pushed alone; a fresh clone that holds no recorded write.
    let (lab, home, second) = member_tip_rewritten_then_pushed_alone([
        "squash",
        "--use-destination-message",
        "-r",
        "feat/alpha",
    ]);

    // When/Then: an include there goes through, unchecked: no match.
    let out = ok_in(&lab, &home, &second, &["include", "feat/delta"]);
    assert_unchecked(&out, (FIXED, FIXED), &home, Why::NoMatch);
}

#[test]
fn a_member_tip_abandoned_is_unchecked_in_a_fresh_clone() {
    // Given (q20): alpha's tip abandoned, the release pushed alone, and a
    // fresh clone.
    let (lab, home, second) = member_tip_rewritten_then_pushed_alone(["abandon", "feat/alpha"]);

    // When/Then: an include there goes through, unchecked: no match.
    let out = ok_in(&lab, &home, &second, &["include", "feat/delta"]);
    assert_unchecked(&out, (FIXED, FIXED), &home, Why::NoMatch);
}

#[test]
fn in_the_clone_that_made_it_a_member_tip_squashed_into_its_parent_is_unchecked() {
    // Given (q12): a fixed release of alpha (two commits) and beta, in the
    // clone that holds the cut; alpha's tip squashed into its parent, whose
    // rewrite keeps the parent's change id, not the tip's.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    extend_branch(&lab, "feat/alpha", "alpha-more.txt", "more\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    step_off_onto_trunk(&lab);
    lab.jj_work(["squash", "--use-destination-message", "-r", "feat/alpha"]);

    // When/Then: an include goes through, unchecked: no match.
    let out = release_ran(&lab, &home, &["include", "feat/delta"]);
    assert_unchecked(&out, (FIXED, FIXED), &home, Why::NoMatch);
}

#[test]
fn in_the_clone_holding_the_write_a_reworded_member_is_recorded_and_a_hand_edit_refused() {
    // Given (q20 control): a fixed release in the clone that holds its cut,
    // then a member reworded in place (same change id), which makes jj
    // rebase the release over it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    step_off_onto_trunk(&lab);
    let cut = commit_at(&lab, FIXED);
    lab.jj_work(["describe", "-r", "feat/alpha", "-m", "alpha, reworded"]);
    assert_ne!(commit_at(&lab, FIXED), cut, "jj rebased the release");
    let reworded = operation_ids(&lab.work);

    // When/Then: an include is checked and goes through.
    let out = release_ran(&lab, &home, &["include", "feat/delta"]);
    assert!(!out.contains("could not check"), "{out}");

    // When: from the reword again, a hand edit on top.
    lab.jj_work(["op", "restore", reworded[0].as_str()]);
    edit_by_hand(&lab, FIXED, &[("notes.txt", "by hand\n")]);
    let refused = knives_release(&lab, &home, &["include", "feat/delta"]);

    // Then: refused, naming adopt.
    assert_refused_naming_adopt(&refused, FIXED);
}

/// A dated release of alpha and beta, pushed, then tagged and repaired by
/// hand in the order `pushed_edit` names: the edit pushed before the tag, or
/// the tag on the pushed cut and the edit left unpushed. Returns the lab, its
/// home, the tagged commit and the edited one.
fn tagged_release(pushed_edit: bool) -> (Lab, tempfile::TempDir, CommitId, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    if pushed_edit {
        edit_by_hand(&lab, DATED, &[("alpha.txt", "alpha, repaired\n")]);
        lab.push_branch(DATED);
        lab.jj_work(["tag", "set", "v1.0.0", "-r", DATED]);
    } else {
        lab.push_branch(DATED);
        lab.jj_work(["tag", "set", "v1.0.0", "-r", DATED]);
        // A tag makes its commit immutable to jj, so the edit is made as a
        // new commit on the release's parents holding its tree, the name
        // moved onto it.
        let parents = release_parents(&lab, DATED);
        lab.jj_work(["new", parents[0].as_str(), parents[1].as_str(), "-m", DATED]);
        lab.jj_work(["restore", "--from", DATED]);
        std::fs::write(lab.work.join("alpha.txt"), "alpha, repaired\n").expect("repair");
        lab.jj_work(["bookmark", "set", DATED, "-r", "@", "--allow-backwards"]);
        step_off_onto_trunk(&lab);
    }
    let tagged = commit_at(&lab, "v1.0.0");
    let edited = commit_at(&lab, DATED);
    (lab, home, tagged, edited)
}

#[test]
fn a_hand_edit_of_a_tagged_release_the_publish_remote_lacks_gets_the_published_remedy() {
    // Given (q13): a dated release pushed, then tagged, then edited by hand
    // and not pushed.
    let (lab, home, tagged, edited) = tagged_release(false);
    let remedy = format!(
        "{DATED} is published (tag v1.0.0 names {}), so a hand edit cannot be recorded under it: \
         move {DATED} back to its published commit ({DATED}@origin, {}) (`jj bookmark set \
         {DATED} -r {} --allow-backwards`), then if the working copy is on {} or on an empty \
         commit on it, `jj new 'trunk()'`, then cut a new dated name, then `jj restore --from {} \
         --into <new name>`, \
         then adopt the new name, then `jj abandon {}`, then `jj bookmark forget {DATED}` \
         (never `jj bookmark delete`, which a push would carry to the remote)",
        tagged.short(),
        tagged.short(),
        tagged.short(),
        edited.short(),
        edited.short(),
        edited.short()
    );
    let (ledger, operations) = (ledger_entries(&home), operation_ids(&lab.work));

    // When: the next cut runs, stating the drop the orphan gate would ask
    // about (the tag keeps the pre-edit commit beside the edit).
    let cut = knives_release(&lab, &home, &["cut", NEXT, "--allow-drop"]);

    // Then: it prints the published remedy and writes nothing.
    assert_eq!(cut.status.code(), Some(3), "{}", text(&cut));
    assert!(stdout(&cut).contains(&remedy), "{}", text(&cut));
    assert!(
        !stdout(&cut).contains("`knives release adopt"),
        "{}",
        text(&cut)
    );

    // When: it is adopted.
    let adopted = knives_release(&lab, &home, &["adopt", "--why", "a repair"]);

    // Then: the same remedy, and nothing written.
    assert_eq!(adopted.status.code(), Some(3), "{}", text(&adopted));
    assert!(stdout(&adopted).contains(&remedy), "{}", text(&adopted));
    assert_eq!(ledger_entries(&home), ledger);
    assert_eq!(operation_ids(&lab.work), operations);

    // When (control): the remedy is followed: the name moved back to its
    // published commit, a new dated name cut, the edit redone there.
    lab.jj_work([
        "bookmark",
        "set",
        DATED,
        "-r",
        tagged.as_str(),
        "--allow-backwards",
    ]);
    // The edit left behind is the person's to drop: the cut states it.
    let out = release_ran(&lab, &home, &["cut", NEXT, "--allow-drop"]);
    assert!(out.contains(&format!("cut {NEXT} as")), "{out}");
    edit_by_hand(&lab, NEXT, &[("alpha.txt", "alpha, repaired\n")]);
    let redone = commit_at(&lab, NEXT);
    let out = release_ran(&lab, &home, &["adopt", NEXT, "--why", "a repair"]);

    // Then: adopt records the new name's edit.
    assert!(
        out.contains(&format!(
            "adopted {} as the newest write of {NEXT}",
            redone.short()
        )),
        "{out}"
    );
}

#[test]
fn a_hand_edit_pushed_and_then_tagged_is_named_for_adopt_and_then_cut() {
    // Given (q14): a dated release whose hand edit was pushed and then
    // tagged before any adopt: the publish remote holds it.
    let (lab, home, tagged, edited) = tagged_release(true);
    assert_eq!(tagged, edited);

    // When: the next cut runs.
    let cut = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: it names adopt, not the published remedy: consumers were given
    // the edit itself.
    assert_refused_naming_adopt(&cut, DATED);
    assert!(!stdout(&cut).contains("is published ("), "{}", text(&cut));

    // When: it is adopted, then cut.
    let out = release_ran(&lab, &home, &["adopt", "--why", "a repair"]);
    assert!(
        out.contains(&format!("adopted {} as the newest write", edited.short())),
        "{out}"
    );
    let out = release_ran(&lab, &home, &["cut", NEXT]);

    // Then: the cut writes, holding the repair.
    assert!(out.contains(&format!("cut {NEXT} as")), "{out}");
    assert_eq!(
        file_at_revision(&lab, NEXT, "alpha.txt"),
        "alpha, repaired\n"
    );
}

#[test]
fn a_fresh_clone_after_two_unadopted_pushes_is_unchecked_then_adopted_then_checked() {
    // Given (q15): a fixed release cut (W1), re-signed and pushed (F1), a
    // member included (W2), fast-forwarded and pushed (F2), all without
    // adopt; a fresh clone holding F1 and F2 but neither write.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    step_off_onto_trunk(&lab);
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    lab.jj_work([
        "metaedit",
        "--force-rewrite",
        "--ignore-immutable",
        "-r",
        FIXED,
    ]);
    lab.push_branch(FIXED);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    step_off_onto_trunk(&lab);
    release_ran(&lab, &home, &["include", "feat/gamma"]);
    fast_forward(&lab, FIXED);
    lab.push_branch(FIXED);
    let second = second_clone(&lab);
    lab.jj_at(&second, ["bookmark", "track", "integration@origin"]);
    let pushed = commit_in(&second, FIXED);
    branch_in(&lab, &second, "feat/delta");

    // When: the clone includes.
    let out = ok_in(&lab, &home, &second, &["include", "feat/delta"]);

    // Then: unchecked, not refused: the include it copies is absent.
    assert_unchecked(&out, (FIXED, FIXED), &home, Why::WriteAbsent);

    // When: a second fresh clone adopts F2 first, then includes and cuts.
    let third = fresh_clone(&lab, "third");
    lab.jj_at(&third, ["bookmark", "track", "integration@origin"]);
    branch_in(&lab, &third, "feat/eps");
    let out = ok_in(&lab, &home, &third, &["adopt"]);
    assert!(
        out.contains(&format!("adopted {} as the newest write", pushed.short())),
        "{out}"
    );
    let included = ok_in(&lab, &home, &third, &["include", "feat/eps"]);
    let cut = ok_in(&lab, &home, &third, &["cut"]);

    // Then: both are checked.
    assert!(!included.contains("could not check"), "{included}");
    assert!(!cut.contains("could not check"), "{cut}");
}

/// Alpha and beta conflicting on one file, cut in the work checkout and
/// never pushed; in the second clone the release made again from the
/// members, the conflict resolved by hand, then an include there (E), which
/// knives could not check: the cut is absent. Returns the lab, its home and
/// the clone.
fn an_unchecked_write_in_the_second_clone() -> (Lab, tempfile::TempDir, PathBuf) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let home = conflicted_first_cut(&lab);
    lab.push_branch("feat/alpha");
    lab.push_branch("feat/beta");
    let second = second_clone(&lab);
    lab.jj_at(
        &second,
        ["new", "feat/alpha@origin", "feat/beta@origin", "-m", DATED],
    );
    std::fs::write(second.join("shared.txt"), "merged\n").expect("resolve by hand");
    lab.jj_at(&second, ["bookmark", "create", DATED, "-r", "@"]);
    step_off_onto_trunk_in(&lab, &second);
    branch_in(&lab, &second, "feat/gamma");
    let out = ok_in(&lab, &home, &second, &["include", "feat/gamma"]);
    assert_unchecked(&out, (DATED, DATED), &home, Why::WriteAbsent);
    (lab, home, second)
}

#[test]
fn a_write_over_an_unchecked_write_stays_unchecked_until_adopt() {
    // Given (sticky): an unchecked write E in the second clone.
    let (lab, home, second) = an_unchecked_write_in_the_second_clone();

    // When: a cut runs over E in the same clone.
    let cut = stdout(&release_in(&lab, &home, &second, &["cut", NEXT]));

    // Then: E stays unchecked: the cut says so, its carried rows say the
    // release in hand is unrecorded, its event keeps E's mark, and audit
    // notes it.
    assert!(cut.contains(&format!("cut {NEXT} as")), "{cut}");
    assert!(
        cut.contains("the release in hand is unchecked since the recorded edit")
            && cut.contains("(W absent); knives did not fetch; `knives release adopt"),
        "{cut}"
    );
    // The members are held here only as the publish remote's refs, so their
    // rows name them by commit.
    let rows = cut
        .lines()
        .filter(|line| {
            line.ends_with(
                "diverges where the unrecorded in-hand release already does; record it with \
                 `knives release adopt` and cut again",
            )
        })
        .count();
    assert_eq!(rows, 2, "{cut}");
    let event = newest_event(&home, NEXT);
    assert!(
        event.text.ends_with("; unchecked: W absent"),
        "{}",
        event.text
    );
    // Audit reads the publish remote's refs live, so the cut is pushed for
    // it to name.
    lab.jj_at(
        &second,
        ["git", "push", "--remote", "origin", "--bookmark", NEXT],
    );
    let report = audit_in(&lab, &home, &second);
    assert!(
        notes_hold(&report, &[NEXT, "could not check", "(W absent)"]),
        "{report}"
    );

    // When (k2): the cut, the newest recorded write but marked, is adopted.
    let adopted = ok_in(&lab, &home, &second, &["adopt", NEXT]);

    // Then: it is recorded unmarked, measured against an automerge, and the
    // audit note is gone.
    assert!(adopted.contains("as the newest write of"), "{adopted}");
    assert!(
        adopted.contains("baseline: an automerge of its")
            && adopted.contains("because knives could not check the release the recorded cut"),
        "{adopted}"
    );
    assert!(!newest_event(&home, NEXT).text.contains("unchecked"));
    let report = audit_in(&lab, &home, &second);
    assert!(!notes_hold(&report, &[NEXT, "could not check"]), "{report}");

    // When: a member is included, checked, then it is cut again (the cut
    // was pushed, so an identical one would ship nothing).
    branch_in(&lab, &second, "feat/delta");
    let included = ok_in(&lab, &home, &second, &["include", "feat/delta"]);
    assert!(!included.contains("could not check"), "{included}");
    let clean = stdout(&release_in(
        &lab,
        &home,
        &second,
        &["cut", "release/2026-08-06"],
    ));

    // Then: that cut is checked, its rows name a recorded write, and its
    // event carries no mark.
    assert!(clean.contains("cut release/2026-08-06 as"), "{clean}");
    assert!(!clean.contains("could not check"), "{clean}");
    assert!(!clean.contains("unrecorded"), "{clean}");
    assert!(
        clean.contains("diverges where the release in hand (a recorded write) already does"),
        "{clean}"
    );
    assert!(
        !newest_event(&home, "release/2026-08-06")
            .text
            .contains("unchecked")
    );
}

#[test]
fn a_hand_edit_over_an_unchecked_write_this_clone_holds_is_unchecked_not_refused() {
    // Given: an unchecked write E in the second clone, then E edited by hand.
    let (lab, home, second) = an_unchecked_write_in_the_second_clone();
    edit_in(&lab, &second, DATED, &[("notes.txt", "by hand\n")]);
    branch_in(&lab, &second, "feat/delta");

    // When: an include runs over it.
    let out = ok_in(&lab, &home, &second, &["include", "feat/delta"]);

    // Then: nothing measured E, so the edit on it is unchecked too, with E's
    // mark, not refused.
    assert_unchecked(&out, (DATED, DATED), &home, Why::SinceWriteAbsent);
    assert!(
        out.contains("the release in hand is unchecked since the recorded edit")
            && out.contains("(W absent); knives did not fetch; `knives release adopt"),
        "{out}"
    );
}

/// The git repository behind jj checkout `dir`: its own `.git` when it is
/// colocated, else the one jj keeps in its store.
fn git_dir(dir: &Path) -> String {
    let colocated = dir.join(".git");
    let dir = if colocated.exists() {
        colocated
    } else {
        dir.join(".jj/repo/store/git")
    };
    dir.display().to_string()
}

/// `git ls-remote` of the lab's origin for `DATED` and the tag `v1.0.0`.
fn published_refs(lab: &Lab) -> String {
    let origin = lab.temp_origin().display().to_string();
    lab::git_output(
        lab.temp_path(),
        [
            "ls-remote",
            origin.as_str(),
            &format!("refs/heads/{DATED}"),
            "refs/tags/v1.0.0",
        ],
    )
}

/// F20's state: a dated cut W of alpha and beta, then a resolution C made on
/// top of it, pushed to the name and tagged, the tag pushed too; then a
/// fresh clone takes the remedy: the name moved to W, a new dated cut (kept
/// the old name, exit 0: D16), the resolution restored from C into it,
/// adopted. Returns the lab, its home, the clone, and [`published_refs`]
/// before the clone.
fn published_resolution_redone_in_a_fresh_clone() -> (Lab, tempfile::TempDir, PathBuf, String) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let (cut, child) = resolution_on_top(&lab, DATED);
    lab.push_branch(DATED);
    lab.jj_work(["tag", "set", "v1.0.0", "-r", DATED]);
    let work_git = git_dir(&lab.work);
    lab::git_output(
        &lab.work,
        [
            "--git-dir",
            work_git.as_str(),
            "push",
            "origin",
            "refs/tags/v1.0.0",
        ],
    );
    // A pod tracks its release's members, as one that cut it would.
    lab.push_branch("feat/alpha");
    lab.push_branch("feat/beta");
    let before = published_refs(&lab);
    let clone = fresh_clone(&lab, "third");
    lab.jj_at(
        &clone,
        ["bookmark", "track", "feat/alpha@origin", "feat/beta@origin"],
    );
    assert_eq!(commit_in(&clone, &format!("{DATED}@origin")), child);
    lab.jj_at(
        &clone,
        [
            "bookmark",
            "set",
            DATED,
            "-r",
            cut.as_str(),
            "--allow-backwards",
        ],
    );
    let out = ok_in(&lab, &home, &clone, &["cut", NEXT]);
    assert!(out.contains(&format!("cut {NEXT} as")), "{out}");
    assert!(out.contains(&format!("demo: kept {DATED}: ")), "{out}");
    lab.jj_at(
        &clone,
        ["restore", "--from", child.as_str(), "--into", NEXT],
    );
    let adopted = release_in(&lab, &home, &clone, &["adopt", NEXT, "--why", "redo C"]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    lab.jj_at(&clone, ["bookmark", "forget", DATED]);
    (lab, home, clone, before)
}

/// Assert a reap in `dir` keeps `DATED` and exits `code`.
fn assert_reap_keeps_the_old_name(lab: &Lab, home: &tempfile::TempDir, dir: &Path, code: i32) {
    let reaped = release_in(lab, home, dir, &["reap"]);
    assert!(
        stdout(&reaped).contains(&format!("demo: kept {DATED}: ")),
        "{}",
        text(&reaped)
    );
    assert_eq!(reaped.status.code(), Some(code), "{}", text(&reaped));
}

/// Assert a dated cut `name` in `dir` writes, keeps `DATED` quietly, reads
/// no double cut, and exits 0.
fn assert_quiet_cut(lab: &Lab, home: &tempfile::TempDir, dir: &Path, name: &str) {
    let cut = release_in(lab, home, dir, &["cut", name]);
    let out = stdout(&cut);
    assert!(out.contains(&format!("cut {name} as")), "{}", text(&cut));
    assert!(
        out.contains(&format!("demo: kept {DATED}: ")),
        "{}",
        text(&cut)
    );
    assert!(!out.contains("names both"), "{}", text(&cut));
    assert_eq!(cut.status.code(), Some(0), "{}", text(&cut));
}

#[test]
fn a_published_resolution_redone_on_new_cuts_is_kept_without_findings() {
    // Given (D16): F20's remedy taken in a fresh clone, ending with the old
    // name forgotten there.
    let (lab, home, clone, before) = published_resolution_redone_in_a_fresh_clone();

    // When/Then: a second dated cut keeps the old name for the resolution
    // it published, reads no double cut, and exits 0.
    assert_quiet_cut(&lab, &home, &clone, "release/2026-08-06");

    // When/Then: a fetch brings the old name's ref back, untracked, and a
    // further cut is as quiet.
    lab.jj_at(&clone, ["git", "fetch", "--all-remotes"]);
    assert_quiet_cut(&lab, &home, &clone, "release/2026-08-07");
    assert_reap_keeps_the_old_name(&lab, &home, &clone, 0);

    // And: audit finds one release drift, on the old published name, and no
    // orphan; the publish remote's old name and tag are as they were.
    let report = audit_in(&lab, &home, &clone);
    let drift = findings(&report, "release-drift");
    assert_eq!(drift.len(), 1, "{report}");
    assert!(drift[0].to_string().contains(DATED), "{report}");
    assert!(findings(&report, "orphan-commit").is_empty(), "{report}");
    assert_eq!(published_refs(&lab), before);
}

#[test]
fn a_published_resolution_kept_beside_a_local_bookmark_or_a_workspace_exits_with_findings() {
    // Given (D16 controls): F20's remedy taken in a fresh clone, then a
    // local bookmark on the published resolution.
    let (lab, home, clone, _before) = published_resolution_redone_in_a_fresh_clone();
    let resolution = format!("{DATED}@origin");
    let taken = operation_ids(&clone);
    lab.jj_at(
        &clone,
        ["bookmark", "create", "keep/c", "-r", resolution.as_str()],
    );

    // When/Then: the reap keeps the old name, and that is a finding.
    assert_reap_keeps_the_old_name(&lab, &home, &clone, 1);

    // When: from the remedy again, a workspace on the resolution instead.
    lab.jj_at(&clone, ["op", "restore", taken[0].as_str()]);
    let workspace = lab.temp_path().join("ws-on-c");
    let path = workspace.display().to_string();
    lab.jj_at(
        &clone,
        [
            "workspace",
            "add",
            "--name",
            "on-c",
            "-r",
            resolution.as_str(),
            path.as_str(),
        ],
    );

    // Then: kept again, and a finding again.
    assert_reap_keeps_the_old_name(&lab, &home, &clone, 1);

    // When: from the remedy again, a bookmark two clones moved apart, one
    // side on the resolution, instead.
    lab.jj_at(&clone, ["op", "restore", taken[0].as_str()]);
    divergent_keep_in(&lab, &clone, resolution.as_str());

    // Then: kept again, and a finding again.
    assert_reap_keeps_the_old_name(&lab, &home, &clone, 1);
}

#[test]
fn the_published_remedy_taken_in_the_clone_that_pushed_the_resolution_ends_quiet() {
    // Given (q12): a dated cut W, a resolution C on top of it, pushed to
    // the name and tagged, all in the work checkout.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let (cut, child) = resolution_on_top(&lab, DATED);
    lab.push_branch(DATED);
    lab.jj_work(["tag", "set", "v1.0.0", "-r", DATED]);
    let refused = knives_release(&lab, &home, &["cut", NEXT]);
    assert_eq!(refused.status.code(), Some(3), "{}", text(&refused));

    // When: the published remedy is taken here, step by step: the name moved
    // back to W, then a new dated cut.
    lab.jj_work([
        "bookmark",
        "set",
        DATED,
        "-r",
        cut.as_str(),
        "--allow-backwards",
    ]);
    lab.jj_work(["new", "trunk()"]);
    let new_cut = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: the cut is written and exits 1 once, its only `!!` line the
    // double cut the remedy says to expect, and it keeps the old name.
    let out = stdout(&new_cut);
    assert!(
        out.contains(&format!("cut {NEXT} as")),
        "{}",
        text(&new_cut)
    );
    let flagged: Vec<&str> = out.lines().filter(|line| line.contains("!!")).collect();
    assert_eq!(flagged.len(), 1, "{}", text(&new_cut));
    assert!(
        flagged.iter().all(|line| {
            line.contains(&format!("{DATED} names both "))
                && line.contains("and their trees differ")
        }),
        "{}",
        text(&new_cut)
    );
    assert!(
        out.contains(&format!("demo: kept {DATED}: ")),
        "{}",
        text(&new_cut)
    );
    assert_eq!(new_cut.status.code(), Some(1), "{}", text(&new_cut));
    assert!(
        newest_event(&home, NEXT)
            .text
            .starts_with(&format!("cut {NEXT} as"))
    );
    let after_cut = operation_ids(&lab.work);

    // When (control): the resolution is restored without `--into`, so into
    // the working copy, and the new name adopted.
    lab.jj_work(["new", NEXT]);
    lab.jj_work(["restore", "--from", child.as_str()]);
    let adopted = knives_release(&lab, &home, &["adopt", NEXT, "--why", "redo C"]);

    // Then: adopt has only the cut to record, without the resolution.
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    assert!(
        stdout(&adopted).contains("nothing to adopt"),
        "{}",
        text(&adopted)
    );
    assert!(
        newest_event(&home, NEXT)
            .text
            .starts_with(&format!("cut {NEXT} as"))
    );

    // When: the rest of the remedy as it prints it: the resolution restored
    // into the new name and adopted.
    lab.jj_work(["op", "restore", after_cut[0].as_str()]);
    lab.jj_work(["restore", "--from", child.as_str(), "--into", NEXT]);
    let adopted = knives_release(&lab, &home, &["adopt", NEXT, "--why", "redo C"]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
    assert!(
        stdout(&adopted).contains("as the newest write"),
        "{}",
        text(&adopted)
    );

    // Then: before the forget, a reap keeps the old name, and quietly.
    assert_reap_keeps_the_old_name(&lab, &home, &lab.work, 0);

    // When: last, the old name is forgotten.
    lab.jj_work(["bookmark", "forget", DATED]);

    // Then: the next dated cut keeps the old name quietly, with no double
    // cut, and the publish remote still holds the resolution.
    assert_quiet_cut(&lab, &home, &lab.work, "release/2026-08-06");
    assert_eq!(commit_at(&lab, &format!("{DATED}@origin")), child);
}

#[test]
fn an_all_carrier_release_held_only_as_the_remote_ref_names_the_track_before_the_squash() {
    // Given (q12): a fixed release cut, a resolution made on top of it and
    // pushed from the work checkout; a fresh clone holds the name only as
    // `integration@origin`.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    let (under, child) = resolution_on_top(&lab, FIXED);
    lab.push_branch(FIXED);
    let clone = fresh_clone(&lab, "third");

    // When: it is adopted there.
    let adopted = release_in(&lab, &home, &clone, &["adopt", FIXED]);

    // Then: refused, naming the track before the squash.
    assert_eq!(adopted.status.code(), Some(3), "{}", text(&adopted));
    assert!(
        stdout(&adopted).contains(&format!(
            "every parent of {FIXED} is an earlier write: track it (`jj bookmark track \
             {FIXED}@origin`), then squash the resolution into the release commit (`jj squash \
             --from {} --into {} --use-destination-message --ignore-immutable`, as that commit \
             is published)",
            child.short(),
            under.short()
        )),
        "{}",
        text(&adopted)
    );

    // When (control): the name is tracked, then adopted.
    lab.jj_at(&clone, ["bookmark", "track", "integration@origin"]);
    let adopted = release_in(&lab, &home, &clone, &["adopt", FIXED]);

    // Then: the same refusal, with no track step.
    assert_eq!(adopted.status.code(), Some(3), "{}", text(&adopted));
    assert!(
        stdout(&adopted).contains(&format!(
            "every parent of {FIXED} is an earlier write: squash the resolution into the release \
             commit (`jj squash --from {} --into {} --use-destination-message \
             --ignore-immutable`",
            child.short(),
            under.short()
        )),
        "{}",
        text(&adopted)
    );

    // When (control): the squash the remedy names, run without the track,
    // then adopt.
    lab.jj_at(&clone, ["bookmark", "forget", FIXED]);
    let before = ledger_entries(&home);
    lab.jj_at(
        &clone,
        [
            "squash",
            "--from",
            child.as_str(),
            "--into",
            under.as_str(),
            "--use-destination-message",
            "--ignore-immutable",
        ],
    );
    let adopted = release_in(&lab, &home, &clone, &["adopt", FIXED, "--why", "redo"]);

    // Then: the squash moved no local name, so adopt records nothing.
    assert!(
        !stdout(&adopted).contains("as the newest write"),
        "{}",
        text(&adopted)
    );
    assert_eq!(ledger_entries(&home), before);
}

/// The content of `file` at `revision` in checkout `dir`.
fn file_in(dir: &Path, revision: &str, file: &str) -> String {
    let output = std::process::Command::new("jj")
        .args([
            "--ignore-working-copy",
            "file",
            "show",
            "-r",
            revision,
            file,
        ])
        .current_dir(dir)
        .output()
        .expect("run jj file show");
    assert!(
        output.status.success(),
        "jj file show failed: {}",
        text(&output)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A fixed release whose publish remote holds an all-carrier commit C, a
/// resolution on top of the cut W; in a fresh clone the name is tracked, C
/// squashed into W, adopted, fast-forwarded onto C, adopted again and
/// pushed, then a member included (q12, D13). When `push_first`, the push
/// comes before that second adopt, which then refuses: C, no longer the
/// publish remote's commit, reads as a member. Returns the include's and the
/// plan's output, the release's parents, C, and the resolution's file at the
/// release.
fn squashed_resolution_fast_forwarded(
    push_first: bool,
) -> (String, Vec<CommitId>, CommitId, String) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let home = fixed_home(&lab);
    release_ran(&lab, &home, &["cut"]);
    let (under, child) = resolution_on_top(&lab, FIXED);
    lab.push_branch(FIXED);
    lab.push_branch("feat/alpha");
    lab.push_branch("feat/beta");
    let clone = fresh_clone(&lab, "third");
    lab.jj_at(
        &clone,
        [
            "bookmark",
            "track",
            "integration@origin",
            "feat/alpha@origin",
            "feat/beta@origin",
        ],
    );
    lab.jj_at(
        &clone,
        [
            "squash",
            "--from",
            child.as_str(),
            "--into",
            under.as_str(),
            "--use-destination-message",
            "--ignore-immutable",
        ],
    );
    let out = ok_in(
        &lab,
        &home,
        &clone,
        &["adopt", FIXED, "--why", "the resolution"],
    );
    assert!(out.contains("as the newest write"), "{out}");
    fast_forward_in(&lab, &clone, FIXED);
    let push = |lab: &Lab| {
        lab.jj_at(
            &clone,
            ["git", "push", "--remote", "origin", "--bookmark", FIXED],
        );
    };
    if push_first {
        push(&lab);
    }
    let adopted = release_in(&lab, &home, &clone, &["adopt", FIXED]);
    if push_first {
        // Pushed, C is no longer the publish remote's commit, so it reads as
        // a member, and the release's members match no recorded write.
        assert_eq!(adopted.status.code(), Some(3), "{}", text(&adopted));
        assert!(
            stdout(&adopted).contains(&format!(
                "demo: {FIXED}'s members differ from every recorded write; "
            )),
            "{}",
            text(&adopted)
        );
    } else {
        assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
        assert!(
            stdout(&adopted).contains("as the newest write"),
            "{}",
            text(&adopted)
        );
        push(&lab);
    }

    // When: one more write runs: a member is included.
    branch_in(&lab, &clone, "feat/delta");
    let included = ok_in(&lab, &home, &clone, &["include", "feat/delta"]);
    let plan = ok_in(&lab, &home, &clone, &[]);
    let parents = Repo::open(&clone)
        .expect("open the clone")
        .parent_commits(FIXED)
        .expect("read the release's parents");
    let notes = file_in(&clone, FIXED, "notes.txt");
    (format!("{included}\n{plan}"), parents, child, notes)
}

#[test]
fn a_squashed_resolution_fast_forwarded_onto_is_a_carrier_by_its_recorded_label() {
    // Given/When: the fast-forwarded resolution adopted again, then pushed,
    // then a member included.
    let (out, parents, child, notes) = squashed_resolution_fast_forwarded(false);

    // Then: C, which only the adopt's record of its fast-forward parent
    // names, is left out as a carrier: nothing reads as stacked, and the
    // resolution is kept.
    assert!(!out.contains("stacked"), "{out}");
    assert!(!parents.contains(&child), "{parents:?}");
    assert_eq!(parents.len(), 3, "{parents:?}");
    assert_eq!(notes, "by hand\n");
}

#[test]
fn a_fast_forward_parent_adopted_after_the_push_is_unlabelled_and_reads_as_stacked() {
    // Given/When (control): the same, pushed before the second adopt, so
    // C is no longer the publish remote's commit and the adopt refuses
    // rather than label it.
    let (out, parents, child, _) = squashed_resolution_fast_forwarded(true);

    // Then: C is carried as a member, stacked on a prior merge.
    assert!(out.contains("stacked on a prior merge"), "{out}");
    assert!(parents.contains(&child), "{parents:?}");
}

/// The working-copy commit of checkout `dir`, snapshotted.
fn working_copy_in(dir: &Path) -> CommitId {
    let output = std::process::Command::new("jj")
        .args(["log", "--no-graph", "-r", "@", "-T", "commit_id"])
        .current_dir(dir)
        .output()
        .expect("run jj log");
    assert!(output.status.success(), "jj log failed: {}", text(&output));
    CommitId::new(String::from_utf8_lossy(&output.stdout).trim())
}

/// Make bookmark `keep` divergent in checkout `dir`, one side on `side` and
/// the other on an empty commit of its own on the trunk: two operations run
/// concurrently from the same one, the first creating it on that commit, the
/// second on `side`, and the next command merges them. jj's git view keeps
/// the first, so only the divergent bookmark names `side`. Neither side
/// descends from the other, which jj would resolve to the descendant.
fn divergent_keep_in(lab: &Lab, dir: &Path, side: &str) {
    lab.jj_at(dir, ["new", "main@origin", "-m", "apart"]);
    let apart = working_copy_in(dir);
    step_off_onto_trunk_in(lab, dir);
    let base = operation_ids(dir)[0].clone();
    for target in [apart.as_str(), side] {
        lab.jj_at(
            dir,
            [
                "--at-op",
                base.as_str(),
                "bookmark",
                "create",
                "keep",
                "-r",
                target,
            ],
        );
    }
    lab.jj_at(dir, ["bookmark", "list"]);
    let divergent = Repo::open(dir)
        .expect("open the checkout")
        .conflicted_bookmarks()
        .expect("read divergent bookmarks");
    assert!(
        divergent
            .iter()
            .any(|(reference, targets)| reference.to_string() == "keep" && targets.len() == 2),
        "keep is not divergent: {divergent:?}"
    );
}

/// F19's state: a dated cut W pushed with its members and tagged, the tag
/// pushed too; a fresh clone makes the resolution R a child of
/// `DATED@origin`, never pushed, with a commit D on it when `with_work`, and
/// moves the name onto R. Returns the lab, its home, the clone, W, R and D.
fn resolution_never_pushed_in_a_fresh_clone(
    with_work: bool,
) -> (
    Lab,
    tempfile::TempDir,
    PathBuf,
    CommitId,
    CommitId,
    Option<CommitId>,
) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.push_branch(DATED);
    lab.push_branch("feat/alpha");
    lab.push_branch("feat/beta");
    lab.jj_work(["tag", "set", "v1.0.0", "-r", DATED]);
    let work_git = git_dir(&lab.work);
    lab::git_output(
        &lab.work,
        [
            "--git-dir",
            work_git.as_str(),
            "push",
            "origin",
            "refs/tags/v1.0.0",
        ],
    );
    let written = commit_at(&lab, DATED);
    let clone = fresh_clone(&lab, "two");
    let published = format!("{DATED}@origin");
    lab.jj_at(&clone, ["new", published.as_str(), "-m", "resolution"]);
    std::fs::write(clone.join("alpha.txt"), "alpha\nresolved\n").expect("resolve");
    let resolution = working_copy_in(&clone);
    let on_top = with_work.then(|| {
        lab.jj_at(&clone, ["new", "-m", "work on R"]);
        std::fs::write(clone.join("d.txt"), "d\n").expect("write D");
        working_copy_in(&clone)
    });
    lab.jj_at(
        &clone,
        [
            "bookmark",
            "set",
            DATED,
            "-r",
            resolution.as_str(),
            "--allow-backwards",
        ],
    );
    lab.jj_at(&clone, ["new", "main@origin"]);
    (lab, home, clone, written, resolution, on_top)
}

/// The remedy up to its adopt in `clone`: the name moved back to W, the
/// working copy to the trunk, a new dated cut with `--allow-drop`, which
/// must find exactly `left` still visible, the resolution restored into it,
/// adopted.
fn remedy_to_its_adopt(
    (lab, home, clone): (&Lab, &tempfile::TempDir, &Path),
    (written, resolution): (&CommitId, &CommitId),
    left: &[&CommitId],
) {
    lab.jj_at(
        clone,
        [
            "bookmark",
            "set",
            DATED,
            "-r",
            written.as_str(),
            "--allow-backwards",
        ],
    );
    lab.jj_at(clone, ["new", "trunk()"]);
    let out = ok_in(lab, home, clone, &["cut", NEXT, "--allow-drop"]);
    assert!(out.contains(&format!("cut {NEXT} as")), "{out}");
    // The flag waives the drop guard alone: the recorded-member check has
    // nothing to waive.
    assert!(!out.contains("member(s) the previous cut"), "{out}");
    let nothing = out
        .lines()
        .find(|line| line.contains("--allow-drop: nothing dropped"))
        .unwrap_or_else(|| panic!("no nothing-dropped line: {out}"));
    assert!(
        nothing.contains(&format!("the {} commit(s)", left.len())),
        "{nothing}"
    );
    for commit in left {
        assert!(nothing.contains(commit.short()), "{nothing}");
    }
    lab.jj_at(
        clone,
        ["restore", "--from", resolution.as_str(), "--into", NEXT],
    );
    let adopted = release_in(lab, home, clone, &["adopt", NEXT, "--why", "redo R"]);
    assert_eq!(adopted.status.code(), Some(0), "{}", text(&adopted));
}

/// Assert `adopt` in `clone` refuses with the published remedy for F19:
/// the cut states the drop naming exactly `left`, no double-cut note, and
/// the abandon of R exactly when `abandon`.
fn assert_f19_remedy(
    (lab, home, clone): (&Lab, &tempfile::TempDir, &Path),
    (resolution, left): (&CommitId, &[&CommitId]),
    abandon: bool,
) -> String {
    let refused = release_in(lab, home, clone, &["adopt", DATED]);
    let out = stdout(&refused);
    assert_eq!(refused.status.code(), Some(3), "{}", text(&refused));
    let named: Vec<&str> = left.iter().map(|commit| commit.short()).collect();
    assert!(
        out.contains(&format!(
            "cut a new dated name with `--allow-drop`, which must print `--allow-drop: nothing \
             dropped` naming exactly {} (any other answer is a stop)",
            named.join(", ")
        )),
        "{out}"
    );
    assert!(!out.contains("that cut exits 1 once"), "{out}");
    assert!(!out.contains("jj squash"), "{out}");
    assert_eq!(
        out.contains(&format!("`jj abandon {}`", resolution.short())),
        abandon,
        "{out}"
    );
    out
}

#[test]
fn a_resolution_never_pushed_redone_and_abandoned_leaves_no_orphan() {
    // Given (q12, F19 a): the resolution R made in a fresh clone on the
    // published W, never pushed; adopt prints the remedy.
    let (lab, home, clone, written, resolution, _) =
        resolution_never_pushed_in_a_fresh_clone(false);
    assert_f19_remedy((&lab, &home, &clone), (&resolution, &[&resolution]), true);
    let (ledger, operations) = (ledger_entries(&home), operation_ids(&clone));

    // When (control): the remedy's cut runs without `--allow-drop`.
    lab.jj_at(
        &clone,
        [
            "bookmark",
            "set",
            DATED,
            "-r",
            written.as_str(),
            "--allow-backwards",
        ],
    );
    lab.jj_at(&clone, ["new", "trunk()"]);
    let plain = release_in(&lab, &home, &clone, &["cut", NEXT]);

    // Then: it exits 3 and writes nothing.
    assert_eq!(plain.status.code(), Some(3), "{}", text(&plain));
    assert_eq!(ledger_entries(&home), ledger);
    lab.jj_at(&clone, ["op", "restore", operations[0].as_str()]);

    // When: the remedy is taken, with the abandon, then the forget.
    remedy_to_its_adopt(
        (&lab, &home, &clone),
        (&written, &resolution),
        &[&resolution],
    );
    lab.jj_at(&clone, ["abandon", resolution.as_str()]);
    lab.jj_at(&clone, ["bookmark", "forget", DATED]);

    // Then: the reap exits 0, a later cut exits 0, and audit finds no orphan.
    let reaped = release_in(&lab, &home, &clone, &["reap"]);
    assert_eq!(reaped.status.code(), Some(0), "{}", text(&reaped));
    let later = release_in(&lab, &home, &clone, &["cut", "release/2026-08-06"]);
    assert_eq!(later.status.code(), Some(0), "{}", text(&later));
    let report = audit_in(&lab, &home, &clone);
    assert!(findings(&report, "orphan-commit").is_empty(), "{report}");
}

#[test]
fn a_resolution_never_pushed_redone_but_not_abandoned_is_an_orphan() {
    // Given (q12, F19 b): the same remedy taken without the abandon.
    let (lab, home, clone, written, resolution, _) =
        resolution_never_pushed_in_a_fresh_clone(false);
    remedy_to_its_adopt(
        (&lab, &home, &clone),
        (&written, &resolution),
        &[&resolution],
    );
    lab.jj_at(&clone, ["bookmark", "forget", DATED]);

    // When: the reap runs, then audit.
    let reaped = release_in(&lab, &home, &clone, &["reap"]);
    let report = audit_in(&lab, &home, &clone);

    // Then: the reap keeps the name with a finding, and audit names R as an
    // orphan.
    assert_eq!(reaped.status.code(), Some(1), "{}", text(&reaped));
    let orphans = findings(&report, "orphan-commit");
    assert!(
        orphans
            .iter()
            .any(|orphan| orphan.to_string().contains(resolution.short())),
        "{report}"
    );
}

#[test]
fn a_resolution_never_pushed_with_work_on_it_names_that_work_and_no_abandon() {
    // Given (q12, F19 c): a commit D on the resolution R.
    let (lab, home, clone, written, resolution, on_top) =
        resolution_never_pushed_in_a_fresh_clone(true);
    let on_top = on_top.expect("D was made");

    // When/Then: adopt's remedy names R and D for the cut's drop check,
    // names D as what rests on R, and prints no abandon.
    let out = assert_f19_remedy(
        (&lab, &home, &clone),
        (&resolution, &[&resolution, &on_top]),
        false,
    );
    assert!(
        out.contains(&format!(
            "{} is not to be abandoned, as",
            resolution.short()
        )) && out.contains(on_top.short()),
        "{out}"
    );

    // And: the remedy's cut finds R and D still visible.
    remedy_to_its_adopt(
        (&lab, &home, &clone),
        (&written, &resolution),
        &[&resolution, &on_top],
    );
}

#[test]
fn a_resolution_never_pushed_a_divergent_bookmark_names_is_named_and_not_abandoned() {
    // Given (q12, F19 d): the resolution R, and a bookmark that two clones
    // moved apart, one side on R.
    let (lab, home, clone, _written, resolution, _) =
        resolution_never_pushed_in_a_fresh_clone(false);
    divergent_keep_in(&lab, &clone, resolution.as_str());

    // When: adopt prints the published remedy.
    let refused = release_in(&lab, &home, &clone, &["adopt", DATED]);

    // Then: it names the divergent bookmark as what keeps R, and prints no
    // abandon of R.
    let out = stdout(&refused);
    assert_eq!(refused.status.code(), Some(3), "{}", text(&refused));
    assert!(
        out.contains(&format!(
            "{} is not to be abandoned, as keep names it",
            resolution.short()
        )),
        "{out}"
    );
    assert!(
        !out.contains(&format!("`jj abandon {}`", resolution.short())),
        "{out}"
    );
}
