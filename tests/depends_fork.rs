//! `knives depends` names what a branch waits for: a pull request on the
//! fork's upstream, a review pull request on the fork's own repository, or a
//! member branch of the fork, and the record says which. A number both
//! repositories have is refused, never guessed, and a requirement recorded
//! wrongly can be taken back or corrected by an entry every machine reads.
//! `knives track --pr` states an upstream pull request, and refuses the
//! number of one only the fork has.

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

/// A fork with a dependent branch, `feat/alpha`, and member branches
/// `feat/base` and `feat/landed` (whose content upstream's trunk has); its
/// config home; and a forge on which the upstream has pull requests #7 and
/// #12 and the fork's own repository has #12 (head `feat/alpha`), #13 (head
/// `feat/base`) and #14 (head `feat/landed`).
struct Fixture {
    lab: Lab,
    home: tempfile::TempDir,
    shim: tempfile::TempDir,
    log: PathBuf,
}

fn fixture() -> Fixture {
    fixture_with_origin(ORIGIN)
}

/// [`fixture`] with the checkout's `origin` remote spelled `origin`.
fn fixture_with_origin(origin: &str) -> Fixture {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/base", "base.txt", "base\n");
    lab.branch("feat/landed", "landed.txt", "landed\n");
    lab.upstream_trunk_file("landed.txt", "landed\n");
    lab.upstream_at_forge_url(UPSTREAM);
    lab.origin_at_forge_url(origin);
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
        "[{},{},{}]",
        pull_record(12, "OPEN", "feat/alpha", None),
        pull_record(13, "OPEN", "feat/base", None),
        pull_record(14, "OPEN", "feat/landed", None)
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
        self.depends_of("feat/alpha", args)
    }

    /// `knives --text depends <branch> <args>`, which must succeed; its
    /// stdout.
    fn depends_of(&self, branch: &str, args: &[&str]) -> String {
        let output = self.knives(&[&["--text", "depends", branch], args].concat());
        assert!(output.status.success(), "{}", said(&output));
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// `knives --text depends <branch> <args>`, which must be refused (exit
    /// 2) with a message holding each of `reasons`; nothing is written.
    fn refused(&self, branch: &str, args: &[&str], reasons: &[&str]) {
        let before = self.entries().len();
        let output = self.knives(&[&["--text", "depends", branch], args].concat());
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", said(&output));
        let stderr = String::from_utf8_lossy(&output.stderr);
        for reason in reasons {
            assert!(stderr.contains(reason), "{args:?}: {stderr}");
        }
        assert_eq!(self.entries().len(), before, "{args:?} wrote an entry");
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
fn a_base_a_release_or_a_fetched_pull_head_is_no_member_branch() {
    // Given: the trunk, a dated release and a fetched pull request head, each
    // a bookmark here, none a branch `status` gives a row.
    let fixture = fixture();
    for name in ["release/2026-11-01", "pr-7"] {
        fixture
            .lab
            .jj_work(["bookmark", "create", name, "-r", "main@origin"]);
    }

    // When / Then: each is refused as a member branch, saying what it is.
    fixture.refused("feat/alpha", &["--on", "main"], &["main is a base"]);
    fixture.refused(
        "feat/alpha",
        &["--on", "release/2026-11-01"],
        &["release/2026-11-01 is a release of demo"],
    );
    fixture.refused(
        "feat/alpha",
        &["--on", "pr-7"],
        &["pr-7 is a fetched pull request head"],
    );
}

#[test]
fn a_branchs_own_review_pull_request_is_refused() {
    // Given: the fork's #12, whose head is feat/alpha.
    let fixture = fixture();

    // When / Then: feat/alpha cannot require its own review pull request,
    // however it is named; another branch can.
    for on in ["acme/demo#12", "origin:acme/demo#12"] {
        fixture.refused(
            "feat/alpha",
            &["--on", on],
            &["acme/demo#12 is feat/alpha's own review pull request"],
        );
    }
    let stdout = fixture.depends_of("feat/base", &["--on", "acme/demo#12"]);
    assert_eq!(
        stdout.trim_end(),
        "demo/feat/base now requires origin:acme/demo#12"
    );
}

#[test]
fn a_renamed_fork_repository_is_named_by_any_spelling_and_forgotten_by_any() {
    // Given: a fork whose checkout and registry both still spell its own
    // repository `acme-old/demo`, which the forge calls `acme/demo` now.
    let fixture = fixture_with_origin(REGISTRY_ORIGIN);

    // When / Then: an ambiguous number suggests the spelling `--on` takes,
    // and that spelling and the record's own each record the fork's pull
    // request under the forge's name.
    fixture.refused(
        "feat/alpha",
        &["--on", "demo#12"],
        &["`--on maintainer/demo#12`", "`--on acme-old/demo#12`"],
    );
    let named = fixture.depends_of("feat/base", &["--on", "acme-old/demo#12"]);
    assert_eq!(
        named.trim_end(),
        "demo/feat/base now requires origin:acme/demo#12"
    );
    let recorded = fixture.depends(&["--on", "origin:acme/demo#13"]);
    assert_eq!(
        recorded.trim_end(),
        "demo/feat/alpha now requires origin:acme/demo#13"
    );

    // And: each is forgotten by the registry name or the old spelling.
    fixture.depends_of("feat/base", &["--forget", "demo#12"]);
    fixture.depends(&["--forget", "acme-old/demo#13"]);
    assert_eq!(fixture.required("feat/base"), Vec::<String>::new());
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
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
    // a forget of the list and the list restated without it, which every
    // machine running this knives folds the same. (A knives from before fork
    // requirements cannot read a list that holds a member branch, and reads
    // this one as requiring nothing.)
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
    fixture.depends_of("feat/base", &["--on", "maintainer/demo#12"]);

    // When: the record is corrected.
    let stdout = fixture.depends_of(
        "feat/base",
        &["--forget", "maintainer/demo#12", "--on", "acme/demo#12"],
    );

    // Then: the branch requires the fork's #12 alone.
    assert_eq!(
        stdout.trim_end(),
        "demo/feat/base no longer requires maintainer/demo#12; it requires origin:acme/demo#12"
    );
    assert_eq!(fixture.required("feat/base"), ["origin:acme/demo#12"]);
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
    fixture.depends_of(
        "feat/base",
        &["--on", "maintainer/demo#12", "--on", "acme/demo#12"],
    );

    // When / Then: forgetting demo#12 is refused, naming both; nothing is
    // written.
    fixture.refused(
        "feat/base",
        &["--forget", "demo#12"],
        &["maintainer/demo#12", "origin:acme/demo#12"],
    );
}

#[test]
fn track_refuses_a_number_only_the_forks_own_repository_has() {
    // Given: #13 is a review pull request on the fork only, #99 is on neither.
    let fixture = fixture();

    // When / Then: `track` states the upstream pull request a branch becomes,
    // so each is refused, the first naming the fork's pull request it is,
    // and nothing is written.
    for (number, reason) in [
        ("13", "acme/demo#13"),
        (
            "99",
            "neither maintainer/demo nor acme/demo has a pull request #99",
        ),
    ] {
        let output = fixture.knives(&["--text", "track", "feat/alpha", "--pr", number]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "#{number}: {}",
            said(&output)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(reason), "#{number}: {stderr}");
    }
    assert!(fixture.entries().is_empty(), "{:?}", fixture.entries());
}

#[test]
fn track_states_a_number_the_upstream_has() {
    // Given: #7 is on the upstream only; #12 is on both.
    let fixture = fixture();

    // When: each is stated as the branch's pull request.
    let mut said_lines = Vec::new();
    for number in ["7", "12"] {
        let output = fixture.knives(&["--text", "track", "feat/alpha", "--pr", number]);
        assert!(output.status.success(), "#{number}: {}", said(&output));
        said_lines.push(
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned(),
        );
    }

    // Then: both are recorded, as `track` always read a number: the
    // upstream's.
    assert_eq!(
        said_lines,
        ["demo/feat/alpha is #7", "demo/feat/alpha is #12"]
    );
    assert_eq!(
        Statements::from_entries(&fixture.entries()).pull("feat/alpha"),
        Some(12)
    );
}

/// The details of a status report's findings of `kind`; a report with no
/// findings leaves the field out.
fn details(report: &Value, kind: &str) -> Vec<String> {
    report["findings"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter(|group| group["kind"] == kind)
        .flat_map(|group| group["items"].as_array().expect("items").iter())
        .map(|item| item["detail"].as_str().expect("detail").to_owned())
        .collect()
}

/// `knives --json status demo`: its exit code and report.
fn status(fixture: &Fixture) -> (Option<i32>, Value) {
    let output = fixture.knives(&["--json", "status", "demo"]);
    let report: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("status JSON ({error}): {}", said(&output)));
    (output.status.code(), report)
}

/// The problems of a status report; a report with none leaves the field out.
fn problems(report: &Value) -> Vec<String> {
    report["problems"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .map(|problem| problem.as_str().expect("problem text").to_owned())
        .collect()
}

#[test]
fn status_judges_a_fork_requirement_by_the_member_it_waits_for() {
    // Given: feat/alpha requires the fork's review pull request #13, whose
    // head feat/base has not landed; feat/base requires the member branch
    // feat/landed, whose content upstream's trunk already has, and the
    // fork's #14, whose head is feat/landed; and feat/landed requires the
    // member branch feat/base.
    let fixture = fixture();
    fixture.depends(&["--on", "acme/demo#13"]);
    fixture.depends_of(
        "feat/base",
        &["--on", "feat/landed", "--on", "acme/demo#14"],
    );
    fixture.depends_of("feat/landed", &["--on", "feat/base"]);

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: each review pull request was asked of the fork's repository and
    // is judged by its head: #13's has not landed, #14's has. The landed
    // member is met; the one that has not landed is not. Nothing is left
    // unanswered, so the report exits 1 for its findings.
    let unmet = details(&report, "unmet-dependency");
    assert!(
        unmet.contains(
            &"feat/alpha requires origin:acme/demo#13 (head feat/base, open), which has not \
              landed (no-pr)"
                .to_owned()
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
    let problems = problems(&report);
    assert!(
        !problems
            .iter()
            .any(|problem| problem.contains("needs") || problem.contains("requires")),
        "{problems:?}\n{report}"
    );
    assert_eq!(code, Some(1), "{report}");
    assert!(
        fixture.forge_calls().contains("owner=acme"),
        "{}",
        fixture.forge_calls()
    );
}

#[test]
fn status_judges_a_member_this_checkout_knows_only_on_origin() {
    // Given: two members another clone pushed to origin, which this checkout
    // has fetched and holds no bookmark of: remote-landed, whose content
    // upstream's trunk has, and remote-open, whose content it has not; and
    // feat/alpha requires both.
    let fixture = fixture();
    fixture
        .lab
        .foreign_origin_branch("main@origin", "remote-open", "open\n");
    fixture
        .lab
        .foreign_origin_branch("main@origin", "remote-landed", "landed\n");
    fixture
        .lab
        .upstream_trunk_file("remote-landed.txt", "landed\n");
    fixture.lab.jj_work(["git", "fetch", "--remote", "origin"]);
    fixture.depends(&["--on", "remote-open", "--on", "remote-landed"]);

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: each is judged from its origin copy: the landed one is met, the
    // other is unmet; nothing is left unanswered.
    let unmet = details(&report, "unmet-dependency");
    assert_eq!(
        unmet,
        [
            "feat/alpha requires origin:acme/demo:remote-open, which has not landed \
          (remote-open@origin not-in-trunk)"
        ],
        "{report}"
    );
    assert_eq!(problems(&report), Vec::<String>::new(), "{report}");
    assert_eq!(code, Some(1), "{report}");
}
