//! `knives release adopt` of a release re-signed or metaedited after a knives
//! write: the write's change, parents and tree under a new commit id, as
//! `jj describe`, `jj sign`, `jj metaedit` or a push that signs leave it.
//!
//! The adopt records the new commit in one event that holds no fix by hand,
//! and says which commit it recorded, so another machine on the same ledger
//! reads the release as recorded once it has fetched it. It changes no
//! content, so a tag on the new commit, while the publish remote does not
//! hold it, refuses nothing, and a write that knives left conflicted is
//! adopted with its conflicts. These tests drive only the knives binary.

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
use lab::{Lab, commit_at, home_after_first_cut, knives_release, release_test_home};

/// The release every test here cuts and re-signs.
const RELEASE: &str = "release/2026-08-04";
/// The forge repository the lab fork's upstream is reached by where two
/// machines share a ledger: a filesystem upstream names no repository that
/// another machine could share a ledger for.
const UPSTREAM: &str = "https://forge.invalid/acme/demo";
/// [`UPSTREAM`]'s `<owner>/<name>`: what a shared ledger keeps the fork under.
const SHARED_FORK: &str = "acme/demo";
/// What a release home's ledger keeps the fork under: its registry key.
const LOCAL_FORK: &str = "demo";
/// The repository the shared ledger belongs to.
const LEDGER: &str = "acme/ledger";
/// [`LEDGER`] as each ledger root's `origin` spells it; git reaches it at a
/// bare remote through `insteadOf`.
const LEDGER_URL: &str = "https://forge.invalid/acme/ledger";

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Both streams, for an assertion's message.
fn text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Every entry of `home`'s ledger of `fork`, in the order it reads them.
fn ledger_entries(home: &Path, fork: &str) -> Vec<Entry> {
    Ledger::at(home.join("ledger").join(fork))
        .read()
        .expect("read the ledger")
        .entries
}

/// Re-describe the release in the work checkout with `jj describe`, keeping
/// its message and adding a review line, as a person does before the push:
/// a new commit with the same change, parents and tree. Returns it.
fn re_describe(lab: &Lab) -> CommitId {
    let written = commit_at(lab, RELEASE);
    let message = format!(
        "{}\n\nReview: read by hand\n",
        lab.revision(&lab.work, RELEASE, "description")
    );
    lab.jj_work(["describe", "-r", RELEASE, "-m", message.as_str()]);
    let described = commit_at(lab, RELEASE);
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
    let event = after
        .iter()
        .rev()
        .find(|entry| entry.kind == Kind::Event && entry.subject.as_deref() == Some(RELEASE))
        .unwrap_or_else(|| panic!("no event about {RELEASE}: {after:?}"));
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

/// A config home whose registry calls the lab fork `demo`, with [`UPSTREAM`]
/// as its upstream and its ledger shared with [`LEDGER`], and the lab's work
/// checkout pointing there too.
fn shared_home(lab: &Lab) -> tempfile::TempDir {
    let (home, _consumer) = release_test_home(lab);
    lab.upstream_at_forge_url(UPSTREAM);
    let registry = home.path().join("repos.toml");
    let text = std::fs::read_to_string(&registry).expect("read the registry");
    std::fs::write(
        &registry,
        format!(
            "{}ledger = \"{LEDGER}\"\n",
            text.replace(&lab.upstream.display().to_string(), UPSTREAM)
        ),
    )
    .expect("write the registry");
    home
}

/// A second machine's config home: the same registry, nothing else yet.
fn second_machine(first: &Path) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("second config home");
    std::fs::copy(first.join("repos.toml"), home.path().join("repos.toml"))
        .expect("copy the registry");
    home
}

/// Make `home`'s ledger root a repository whose `origin` is [`LEDGER_URL`],
/// reached at `remote`, committing as `machine`.
fn share(home: &Path, machine: &str, remote: &Path) {
    let root = home.join("ledger");
    lab::git_repository(&root, &[("origin", LEDGER_URL)]);
    let reached = format!("url.{}.insteadOf", remote.display());
    lab::git_output(&root, ["config", reached.as_str(), LEDGER_URL]);
    lab::git_output(&root, ["config", "knives.machine", machine]);
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
fn a_release_re_described_and_adopted_on_one_machine_reads_as_recorded_on_another() {
    // Given: two machines sharing one ledger remote. Alpha, in the lab's work
    // checkout, cuts a release of two branches, then re-describes it with
    // `jj describe`, as a person adds a review line before the push.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let alpha = shared_home(&lab);
    let beta = second_machine(alpha.path());
    let (_remote_dir, remote) = lab::bare_remote();
    share(alpha.path(), "alpha", &remote);
    share(beta.path(), "beta", &remote);
    let cut = knives_release(&lab, &alpha, &["cut", RELEASE]);
    assert!(cut.status.success(), "{}", text(&cut));
    let written = commit_at(&lab, RELEASE);
    let described = re_describe(&lab);
    let before = ledger_entries(alpha.path(), SHARED_FORK).len();

    // When: alpha adopts it.
    let adopted = knives_release(&lab, &alpha, &["adopt", RELEASE]);

    // Then: one event records the described commit, with no fix by hand.
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
        &[
            "--text",
            "release",
            "--repo",
            "demo",
            "cut",
            "release/2026-08-05",
        ],
    );
    assert_eq!(next.status.code(), Some(0), "{}", text(&next));
    assert!(
        stdout(&next).contains("cut release/2026-08-05 as"),
        "{}",
        text(&next)
    );
}

#[test]
fn a_re_described_release_tagged_before_its_push_is_adopted_not_refused_as_published() {
    // Given: a cut re-described with `jj describe`, then tagged before its
    // push: the tag marks the new commit published, and the publish remote
    // does not hold it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let cut = commit_at(&lab, RELEASE);
    let described = re_describe(&lab);
    lab.jj_work(["tag", "set", "v1.0.0", "-r", RELEASE]);
    let before = ledger_entries(home.path(), LOCAL_FORK).len();

    // When: it is adopted.
    let output = knives_release(&lab, &home, &["adopt", RELEASE]);

    // Then: it changes no content, so it is no hand edit of a published
    // release: it is recorded as the newest write, with no published remedy.
    let after = ledger_entries(home.path(), LOCAL_FORK);
    assert_recorded(&output, (&cut, &described), (before, &after));
    assert!(
        !stdout(&output).contains("is published"),
        "{}",
        text(&output)
    );
}

#[test]
fn a_re_signed_write_that_knives_left_conflicted_is_adopted_with_its_conflicts() {
    // Given: a cut that left shared.txt conflicted, then re-signed with
    // `jj metaedit`: its tree, the conflict included, is the cut's own.
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
    let written = commit_at(&lab, RELEASE);
    lab.jj_work([
        "metaedit",
        "--force-rewrite",
        "--ignore-immutable",
        "-r",
        RELEASE,
    ]);
    let rewritten = commit_at(&lab, RELEASE);
    assert_ne!(rewritten, written, "the metaedit made no new commit");
    assert_eq!(lab.revision(&lab.work, RELEASE, "conflict"), "true");
    let before = ledger_entries(home.path(), LOCAL_FORK).len();

    // When: it is adopted.
    let output = knives_release(&lab, &home, &["adopt", RELEASE]);

    // Then: its conflicts are the recorded write's own, so it is not refused
    // as unresolved: it is recorded as the newest write, and exits 0.
    let after = ledger_entries(home.path(), LOCAL_FORK);
    assert_recorded(&output, (&written, &rewritten), (before, &after));
    assert!(!stdout(&output).contains("still has"), "{}", text(&output));
}
