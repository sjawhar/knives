//! `knives depends` names what a branch waits for: a pull request on the
//! fork's upstream, a review pull request on the fork's own repository, or a
//! member branch of the fork, and the record says which. A number both
//! repositories have is refused, never guessed, and a requirement recorded
//! wrongly can be taken back or corrected by an entry every machine reads.

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

use std::path::PathBuf;
use std::process::Output;

use forge_shim::{Repository, install_repository_gh, path_with_gh_shim, pull_record};
use knives::ledger::{Entry, Ledger};
use knives::statement::{Statement, StatementKind, Statements};
use lab::Lab;
use serde_json::Value;

/// The fork's upstream, as its registry entry and its checkout spell it.
const UPSTREAM: &str = "https://forge.invalid/maintainer/demo";
/// What the fork's ledger is kept under: [`UPSTREAM`]'s `<owner>/<name>`.
const KEPT_UNDER: &str = "maintainer/demo";
/// The fork's own repository, as its checkout's `origin` remote spells it.
const ORIGIN: &str = "https://forge.invalid/acme/demo";
/// The fork's own repository as the registry still spells it, from before
/// the repository moved to `acme`: the forge answers for it under its new
/// name, as GitHub does for a transferred repository.
const REGISTRY_ORIGIN: &str = "https://forge.invalid/acme-old/demo";

/// A fork with a dependent branch, `feat/alpha`, and a member branch,
/// `feat/base`; its config home; and a forge on which the upstream has pull
/// requests #7 and #12 and the fork's own repository has #12 and #13.
struct Fixture {
    lab: Lab,
    home: tempfile::TempDir,
    shim: tempfile::TempDir,
    log: PathBuf,
}

fn fixture() -> Fixture {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/base", "base.txt", "base\n");
    lab.branch("feat/landed", "landed.txt", "landed\n");
    lab.upstream_trunk_file("landed.txt", "landed\n");
    lab.upstream_at_forge_url(UPSTREAM);
    lab.origin_at_forge_url(ORIGIN);
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!("[repos.demo]\nupstream = \"{UPSTREAM}\"\norigin = \"{REGISTRY_ORIGIN}\"\n"),
    )
    .expect("write the registry");
    let shim = tempfile::tempdir().expect("gh shim directory");
    let log = shim.path().join("calls.log");
    let upstream_pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "main", None),
        pull_record(12, "OPEN", "upstream-twelve", None)
    );
    let fork_pulls = format!(
        "[{},{}]",
        pull_record(12, "OPEN", "feat/alpha", None),
        pull_record(13, "OPEN", "feat/base", None)
    );
    install_repository_gh(
        shim.path(),
        &[
            Repository {
                named: KEPT_UNDER,
                canonical: KEPT_UNDER,
                pulls: &upstream_pulls,
            },
            Repository {
                named: "acme-old/demo",
                canonical: "acme/demo",
                pulls: "[]",
            },
            Repository {
                named: "acme/demo",
                canonical: "acme/demo",
                pulls: &fork_pulls,
            },
        ],
        &log,
    );
    Fixture {
        lab,
        home,
        shim,
        log,
    }
}

impl Fixture {
    /// `knives <args>` in the fork's checkout, with the fake forge first on
    /// `PATH`.
    fn knives(&self, args: &[&str]) -> Output {
        lab::knives_command(&self.lab.work, self.home.path(), self.lab.temp_path(), args)
            .env("PATH", path_with_gh_shim(self.shim.path()))
            .env("KNIVES_OWNER", "ses_depends")
            .output()
            .expect("run knives")
    }

    /// `knives --text depends feat/alpha <args>`, which must succeed; its
    /// stdout.
    fn depends(&self, args: &[&str]) -> String {
        let output = self.knives(&[&["--text", "depends", "feat/alpha"], args].concat());
        assert!(output.status.success(), "{}", said(&output));
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn entries(&self) -> Vec<Entry> {
        let read = Ledger::at(self.home.path().join("ledger").join(KEPT_UNDER))
            .read()
            .expect("read the ledger");
        assert!(read.problems.is_empty(), "{:?}", read.problems);
        read.entries
    }

    /// What `branch` requires now, as the ledger states it.
    fn required(&self, branch: &str) -> Vec<String> {
        Statements::from_entries(&self.entries()).depends(branch)
    }

    fn forge_calls(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

fn said(output: &Output) -> String {
    format!(
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The `depends` statement of `value` an entry is expected to carry, a
/// forget when it is `None`.
fn stating(value: Option<&str>) -> Statement {
    Statement {
        kind: StatementKind::Depends,
        value: value.map(str::to_owned),
    }
}

#[test]
fn a_review_pull_request_named_by_the_forks_own_repository_is_recorded_as_the_forks() {
    // Given: a fork whose own repository, acme/demo, has review pull request #13.
    let fixture = fixture();

    // When: a branch is said to need acme/demo#13.
    let stdout = fixture.depends(&["--on", "acme/demo#13"]);

    // Then: the record names the fork's own repository and its role, never
    // the upstream's #13.
    assert_eq!(
        stdout.trim_end(),
        "demo/feat/alpha now requires origin:acme/demo#13"
    );
    assert_eq!(fixture.required("feat/alpha"), ["origin:acme/demo#13"]);
}

#[test]
fn a_number_only_one_repository_has_is_recorded_as_that_repositorys() {
    // Given: #13 is a review pull request on the fork only, #7 a pull request
    // on the upstream only.
    let fixture = fixture();

    // When: each is named by the fork's registry name.
    let fork = fixture.depends(&["--on", "demo#13"]);
    let upstream = fixture.depends(&["--on", "demo#7"]);

    // Then: the forge decided which each is, and each record says so. The
    // fork's repository is named as the forge names it now, not as the
    // registry still spells it.
    assert_eq!(
        fork.trim_end(),
        "demo/feat/alpha now requires origin:acme/demo#13"
    );
    assert_eq!(
        upstream.trim_end(),
        "demo/feat/alpha now requires maintainer/demo#7"
    );
    assert_eq!(
        fixture.required("feat/alpha"),
        ["maintainer/demo#7", "origin:acme/demo#13"]
    );
}

#[test]
fn a_number_both_repositories_have_is_refused_naming_both() {
    // Given: #12 is a pull request on the upstream and on the fork.
    let fixture = fixture();

    // When: a branch is said to need demo#12.
    let output = fixture.knives(&["--text", "depends", "feat/alpha", "--on", "demo#12"]);

    // Then: knives does not guess. It names both and records nothing.
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("maintainer/demo#12"), "{stderr}");
    assert!(stderr.contains("acme/demo#12"), "{stderr}");
    assert!(fixture.entries().is_empty(), "{:?}", fixture.entries());
}

#[test]
fn a_number_neither_repository_has_is_refused() {
    let fixture = fixture();

    let output = fixture.knives(&["--text", "depends", "feat/alpha", "--on", "demo#99"]);

    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    assert!(fixture.entries().is_empty(), "{:?}", fixture.entries());
}

#[test]
fn a_member_branch_of_the_fork_is_recorded_as_the_branch() {
    // Given: a fork with the member branch feat/base.
    let fixture = fixture();

    // When: a branch is said to need it, by its name.
    let stdout = fixture.depends(&["--on", "feat/base"]);

    // Then: the record names the branch and the fork's repository it lives
    // on, and no forge was asked: the branch is in the checkout.
    assert_eq!(
        stdout.trim_end(),
        "demo/feat/alpha now requires origin:acme/demo:feat/base"
    );
    assert_eq!(
        fixture.required("feat/alpha"),
        ["origin:acme/demo:feat/base"]
    );
    assert_eq!(fixture.forge_calls(), "");
}

#[test]
fn a_branch_the_fork_does_not_have_or_the_dependent_itself_is_refused() {
    // Given: a fork with no branch feat/nowhere.
    let fixture = fixture();

    // When / Then: a branch the checkout does not have, and the dependent
    // itself, are each refused for that reason, and nothing is written.
    for (on, reason) in [
        ("feat/nowhere", "this checkout has no branch feat/nowhere"),
        ("feat/alpha", "feat/alpha cannot require itself"),
    ] {
        let output = fixture.knives(&["--text", "depends", "feat/alpha", "--on", on]);
        assert_eq!(output.status.code(), Some(2), "{on}: {}", said(&output));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(reason), "{on}: {stderr}");
    }
    assert!(fixture.entries().is_empty(), "{:?}", fixture.entries());
}

#[test]
fn a_wrong_requirement_is_taken_back_by_an_entry_of_its_own() {
    // Given: a branch recorded as needing the upstream's #12 (the record the
    // defect wrote) and the member branch feat/base.
    let fixture = fixture();
    fixture.depends(&["--on", "maintainer/demo#12"]);
    fixture.depends(&["--on", "feat/base"]);
    let before = fixture.entries().len();

    // When: the wrong one is forgotten, named as it was typed.
    let stdout = fixture.depends(&["--forget", "demo#12"]);

    // Then: only the right one is left, and the removal is in the ledger as
    // a forget of the list and the list restated without it: what every
    // machine's knives, this one's and an older one's alike, folds the same.
    assert_eq!(
        stdout.trim_end(),
        "demo/feat/alpha no longer requires maintainer/demo#12; it requires \
         origin:acme/demo:feat/base"
    );
    assert_eq!(
        fixture.required("feat/alpha"),
        ["origin:acme/demo:feat/base"]
    );
    let entries = fixture.entries();
    let added: Vec<Option<Statement>> = entries[before..]
        .iter()
        .map(|entry| entry.statement.clone())
        .collect();
    assert_eq!(
        added,
        [
            Some(stating(None)),
            Some(stating(Some("origin:acme/demo:feat/base")))
        ],
        "{entries:?}"
    );
    assert_eq!(
        entries[before].text,
        "no longer requires maintainer/demo#12"
    );
}

#[test]
fn a_wrong_requirement_is_corrected_in_one_command() {
    // Given: a branch recorded as needing the upstream's #12, where the fork's
    // review pull request #12 was meant.
    let fixture = fixture();
    fixture.depends(&["--on", "maintainer/demo#12"]);

    // When: the record is corrected.
    let stdout = fixture.depends(&["--forget", "maintainer/demo#12", "--on", "acme/demo#12"]);

    // Then: the branch requires the fork's #12 alone.
    assert_eq!(
        stdout.trim_end(),
        "demo/feat/alpha no longer requires maintainer/demo#12; it requires origin:acme/demo#12"
    );
    assert_eq!(fixture.required("feat/alpha"), ["origin:acme/demo#12"]);
}

#[test]
fn forgetting_the_last_requirement_leaves_the_branch_requiring_nothing() {
    let fixture = fixture();
    fixture.depends(&["--on", "feat/base"]);
    let before = fixture.entries().len();

    let stdout = fixture.depends(&["--forget", "origin:acme/demo:feat/base"]);

    assert_eq!(
        stdout.trim_end(),
        "demo/feat/alpha no longer requires origin:acme/demo:feat/base; it requires nothing"
    );
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
    let added: Vec<Option<Statement>> = fixture.entries()[before..]
        .iter()
        .map(|entry| entry.statement.clone())
        .collect();
    assert_eq!(added, [Some(stating(None))]);
}

#[test]
fn forgetting_what_the_branch_does_not_require_is_refused() {
    // Given: a branch that requires the member branch feat/base only.
    let fixture = fixture();
    fixture.depends(&["--on", "feat/base"]);
    let before = fixture.entries().len();

    // When: something else is forgotten.
    let output = fixture.knives(&["--text", "depends", "feat/alpha", "--forget", "demo#12"]);

    // Then: refused, naming what it does require; nothing is written.
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("origin:acme/demo:feat/base"), "{stderr}");
    assert_eq!(fixture.entries().len(), before);
}

#[test]
fn forgetting_a_number_two_requirements_answer_to_is_refused_naming_both() {
    // Given: a branch that requires both the upstream's #12 and the fork's.
    let fixture = fixture();
    fixture.depends(&["--on", "maintainer/demo#12", "--on", "acme/demo#12"]);
    let before = fixture.entries().len();

    // When: demo#12 is forgotten.
    let output = fixture.knives(&["--text", "depends", "feat/alpha", "--forget", "demo#12"]);

    // Then: refused, naming both; nothing is written.
    assert_eq!(output.status.code(), Some(2), "{}", said(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("maintainer/demo#12"), "{stderr}");
    assert!(stderr.contains("origin:acme/demo#12"), "{stderr}");
    assert_eq!(fixture.entries().len(), before);
}

/// The details of a status report's findings of `kind`.
fn details(report: &Value, kind: &str) -> Vec<String> {
    report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .filter(|group| group["kind"] == kind)
        .flat_map(|group| group["items"].as_array().expect("items").iter())
        .map(|item| item["detail"].as_str().expect("detail").to_owned())
        .collect()
}

#[test]
fn status_checks_a_fork_requirement_on_the_fork_and_names_the_branch_it_waits_for() {
    // Given: feat/alpha requires the fork's review pull request #13, whose
    // head is feat/base; feat/base requires the member branch feat/landed,
    // whose content upstream's trunk already has; and feat/landed requires
    // the member branch feat/base, which has not landed.
    let fixture = fixture();
    fixture.depends(&["--on", "acme/demo#13"]);
    for (branch, on) in [("feat/base", "feat/landed"), ("feat/landed", "feat/base")] {
        let output = fixture.knives(&["--text", "depends", branch, "--on", on]);
        assert!(output.status.success(), "{}", said(&output));
    }

    // When: status reports the fork.
    let output = fixture.knives(&["--json", "status", "demo"]);
    let report: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("status JSON ({error}): {}", said(&output)));

    // Then: the review pull request was asked of the fork's repository and
    // is open, the head it waits for named; the landed branch is met; the
    // branch that has not landed is not.
    let unmet = details(&report, "unmet-dependency");
    assert!(
        unmet.contains(
            &"feat/alpha requires origin:acme/demo#13 (head feat/base), which is open".to_owned()
        ),
        "{unmet:?}\n{report}"
    );
    assert!(
        !unmet
            .iter()
            .any(|detail| detail.starts_with("feat/base requires")),
        "{unmet:?}"
    );
    assert!(
        unmet.iter().any(|detail| detail.starts_with(
            "feat/landed requires origin:acme/demo:feat/base, which has not landed ("
        )),
        "{unmet:?}"
    );
    // A report with no problems leaves the field out.
    let problems = report["problems"].as_array().map_or(&[][..], Vec::as_slice);
    assert!(
        !problems.iter().any(|problem| problem
            .as_str()
            .is_some_and(|text| text.contains("requires"))),
        "{problems:?}\n{report}"
    );
    assert!(
        fixture.forge_calls().contains("owner=acme"),
        "{}",
        fixture.forge_calls()
    );
}
