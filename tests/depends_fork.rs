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

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;

use forge_shim::{
    Repository, install_repository_gh, path_with_gh_shim, pull_record, pull_record_with_fields,
};
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
/// config home; and a forge ([`install_forge`]) on which the upstream has
/// pull requests #7 and #12; the fork's own repository has, from `acme`'s
/// branches, #12 (head `feat/alpha`), #13 (head `feat/base`), #14 (head
/// `feat/landed`), #15 (merged, head `feat/base`), #21 (head `main`) and #22
/// (merged into `review/base`, head `feat/gone`, a branch no longer here),
/// and #20 from `bob`'s `main`; and a sibling fork, whose own repository
/// `acme/sibling` has #5 (head `sib/feature`) and #6 (head `feat/landed`, the
/// sibling's own branch of that name).
struct Fixture {
    lab: Lab,
    home: tempfile::TempDir,
    shim: tempfile::TempDir,
    log: PathBuf,
    forge: RefCell<ForgeState>,
}

/// What the fixture's fake forge answers beyond its fixed pull requests:
/// the head commit of each of the fork's #12 to #15, and pull requests added
/// to the upstream.
#[derive(Default)]
struct ForgeState {
    heads: BTreeMap<u64, String>,
    upstream: Vec<String>,
}

fn fixture() -> Fixture {
    fixture_with_origin(ORIGIN)
}

/// [`fixture`] with the checkout's `origin` remote spelled `origin`.
fn fixture_with_origin(origin: &str) -> Fixture {
    fixture_with(origin, REGISTRY_ORIGIN)
}

/// [`fixture`] with the checkout's `origin` remote spelled `origin` and the
/// registry's `origin` spelled `registered`. The registry's spelling is
/// whose upstream pull requests `status` takes as this fork's.
fn fixture_with(origin: &str, registered: &str) -> Fixture {
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
        format!(
            "[repos.demo]\nupstream = \"{UPSTREAM}\"\norigin = \"{registered}\"\n\
             [repos.sibling]\nupstream = \"https://forge.invalid/maintainer/sibling\"\n\
             origin = \"https://forge.invalid/acme/sibling\"\n"
        ),
    )
    .expect("write the registry");
    let shim = tempfile::tempdir().expect("gh shim directory");
    let log = shim.path().join("calls.log");
    let tip = |branch: &str| lab::commit_at(&lab, branch).as_str().to_owned();
    let state = ForgeState {
        heads: BTreeMap::from([
            (12, tip("feat/alpha")),
            (13, tip("feat/base")),
            (14, tip("feat/landed")),
            (15, tip("feat/base")),
        ]),
        upstream: Vec::new(),
    };
    install_forge(shim.path(), &log, &state);
    Fixture {
        lab,
        home,
        shim,
        log,
        forge: RefCell::new(state),
    }
}

/// The fixture's fake forge, as `state` says.
fn install_forge(shim: &Path, log: &Path, state: &ForgeState) {
    let upstream_pulls = format!(
        "[{}]",
        [
            pull_record(7, "MERGED", "main", None),
            pull_record(12, "OPEN", "upstream-twelve", None),
        ]
        .iter()
        .chain(&state.upstream)
        .cloned()
        .collect::<Vec<_>>()
        .join(",")
    );
    let at = |number: u64, pull_state: &str, head: &str| {
        pull_record_with_fields(
            number,
            pull_state,
            head,
            &format!(
                ",\"headRefOid\":\"{}\",\"headRepositoryOwner\":{{\"login\":\"acme\"}}",
                state.heads[&number]
            ),
        )
    };
    let fork_pulls = format!(
        "[{},{},{},{},{},{},{}]",
        at(12, "OPEN", "feat/alpha"),
        at(13, "OPEN", "feat/base"),
        at(14, "OPEN", "feat/landed"),
        at(15, "MERGED", "feat/base"),
        review(20, "OPEN", "main", "bob"),
        review(21, "OPEN", "main", "acme"),
        pull_record_with_fields(
            22,
            "MERGED",
            "feat/gone",
            ",\"headRepositoryOwner\":{\"login\":\"acme\"},\"baseRefName\":\"review/base\""
        )
    );
    let sibling_pulls = format!(
        "[{},{}]",
        review(5, "OPEN", "sib/feature", "acme"),
        review(6, "OPEN", "feat/landed", "acme")
    );
    install_repository_gh(
        shim,
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
            Repository {
                named: "maintainer/sibling",
                canonical: "maintainer/sibling",
                pulls: "[]",
            },
            Repository {
                named: "acme/sibling",
                canonical: "acme/sibling",
                pulls: &sibling_pulls,
            },
        ],
        log,
    );
}

/// An upstream pull request from `acme`'s branch `head` at `head_oid`,
/// merged as `landing`.
fn merged_upstream(number: u64, head: &str, head_oid: &str, landing: &str) -> String {
    pull_record_with_fields(
        number,
        "MERGED",
        head,
        &format!(
            ",\"headRefOid\":\"{head_oid}\",\"headRepositoryOwner\":{{\"login\":\"acme\"}},\
             \"mergeCommit\":{{\"oid\":\"{landing}\"}},\"mergeable\":\"UNKNOWN\",\
             \"mergeStateStatus\":\"UNKNOWN\""
        ),
    )
}

/// A pull request whose head is `owner`'s branch `head`.
fn review(number: u64, state: &str, head: &str, owner: &str) -> String {
    pull_record_with_fields(
        number,
        state,
        head,
        &format!(",\"headRepositoryOwner\":{{\"login\":\"{owner}\"}}"),
    )
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

    /// Reinstall the fixture's forge with `upstream` added to the upstream's
    /// pull requests.
    fn forge(&self, upstream: &[String]) {
        let mut state = self.forge.borrow_mut();
        state.upstream = upstream.to_vec();
        install_forge(self.shim.path(), &self.log, &state);
    }

    /// Reinstall the fixture's forge with the fork's #`number` headed at
    /// `commit`, as when its branch moves on the forge.
    fn move_head(&self, number: u64, commit: &str) {
        let mut state = self.forge.borrow_mut();
        let _ = state.heads.insert(number, commit.to_owned());
        install_forge(self.shim.path(), &self.log, &state);
    }

    fn forge_calls(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Rewrite the one ledger entry of the fork that states the list `from`
    /// to state `to`: a record an older knives, or another machine, could
    /// have written, which this knives refuses to write.
    fn rewrite_requirements(&self, from: &str, to: &str) {
        let dir = self.home.path().join("ledger").join(KEPT_UNDER);
        let (from, to) = (format!("value = \"{from}\""), format!("value = \"{to}\""));
        let mut rewritten = 0;
        for file in std::fs::read_dir(&dir).expect("read the ledger directory") {
            let path = file.expect("a ledger directory entry").path();
            if path.extension().is_none_or(|extension| extension != "md") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read a ledger entry");
            if text.contains(&from) {
                std::fs::write(&path, text.replace(&from, &to)).expect("rewrite a ledger entry");
                rewritten += 1;
            }
        }
        assert_eq!(
            rewritten,
            1,
            "no one entry states {from} in {}",
            dir.display()
        );
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
fn a_review_pull_request_whose_head_is_no_member_of_this_fork_is_refused() {
    // Given: a sibling fork whose own repository has #5 (head sib/feature)
    // and #6 (head feat/landed, the sibling's own branch of a name this fork
    // also has); and this fork's #20, from bob's `main`, and #21, from its
    // own owner's `main`, a base.
    let fixture = fixture();

    // When / Then: each is refused, saying why, however it is named, and
    // nothing is written: `status` here could judge none of their heads.
    for on in ["sibling#5", "acme/sibling#6", "origin:acme/sibling#6"] {
        fixture.refused(
            "feat/alpha",
            &["--on", on],
            &[&format!(
                "{on} is a review pull request on sibling's own repository"
            )],
        );
    }
    fixture.refused(
        "feat/alpha",
        &["--on", "acme/demo#20"],
        &["acme/demo#20's head main is a branch of bob, not of acme"],
    );
    fixture.refused(
        "feat/alpha",
        &["--on", "acme/demo#21"],
        &["acme/demo#21's head main is a base of demo, not a member branch"],
    );

    // And: a member branch named on the sibling's repository is refused as
    // the sibling's, from the registry alone, with no forge call.
    let calls = fixture.forge_calls();
    fixture.refused(
        "feat/alpha",
        &["--on", "origin:acme/sibling:feat/landed"],
        &["which is sibling's own repository, not demo's"],
    );
    assert_eq!(fixture.forge_calls(), calls);

    // And: the sibling's #6 is refused for feat/landed as another fork's,
    // not as feat/landed's own review pull request.
    fixture.refused(
        "feat/landed",
        &["--on", "acme/sibling#6"],
        &["acme/sibling#6 is a review pull request on sibling's own repository"],
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
    fixture.depends(&["--forget", "origin:acme-old/demo#13"]);
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

#[test]
fn a_merged_review_pull_request_whose_head_has_not_landed_stays_unmet() {
    // Given: feat/alpha requires the fork's #15, merged into a review base,
    // whose head feat/base has not landed upstream.
    let fixture = fixture();
    fixture.depends(&["--on", "acme/demo#15"]);

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: the merge does not meet it: the member has not reached upstream.
    assert_eq!(
        details(&report, "unmet-dependency"),
        [
            "feat/alpha requires origin:acme/demo#15 (head feat/base, merged), which has not \
          landed (no-pr)"
        ],
        "{report}"
    );
    assert_eq!(code, Some(1), "{report}");
}

#[test]
fn status_judges_no_review_pull_request_whose_head_is_no_member_here() {
    // Given: records an older knives could write: feat/alpha requires the
    // sibling's #6, whose head shares its name with this fork's landed
    // feat/landed; this fork's #20, from bob's `main`; and this fork's #21,
    // from its own owner's `main`, a base. Both `main`s replay empty onto the
    // trunk.
    let fixture = fixture();
    fixture.depends(&["--on", "acme/demo#13"]);
    fixture.rewrite_requirements(
        "origin:acme/demo#13",
        "origin:acme/demo#20,origin:acme/demo#21,origin:acme/sibling#6",
    );

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: neither reads as met: each is a problem that says why, and the
    // report exits 3.
    let problems = problems(&report);
    for (named, why) in [
        (
            "origin:acme/sibling#6:",
            "acme/sibling is not demo's own repository",
        ),
        (
            "origin:acme/demo#20 (head main, open):",
            "its head main is a branch of bob, not of acme",
        ),
        (
            "origin:acme/demo#21 (head main, open):",
            "its head main is a base of this fork, not a member branch",
        ),
    ] {
        assert!(
            problems.iter().any(|problem| problem.starts_with(&format!(
                "cannot check whether feat/alpha still needs {named}"
            )) && problem.contains(why)),
            "{named}: {problems:?}\n{report}"
        );
    }
    assert_eq!(details(&report, "unmet-dependency"), Vec::<String>::new());
    assert_eq!(code, Some(3), "{report}");
}

#[test]
fn one_member_recorded_under_two_spellings_of_the_fork_is_one_requirement() {
    // Given: feat/alpha requires feat/base under both spellings of the fork's
    // own repository, as two machines whose `origin` remotes differ write it.
    let fixture = fixture();
    fixture.depends(&["--on", "feat/base"]);
    fixture.rewrite_requirements(
        "origin:acme/demo:feat/base",
        "origin:acme-old/demo:feat/base,origin:acme/demo:feat/base",
    );

    // When: status reports the fork, and the branch is named again.
    let (_, report) = status(&fixture);
    fixture.depends(&["--on", "feat/base"]);

    // Then: status reports it once, naming it again adds no third record,
    // and forgetting it by name takes back both records.
    let unmet = details(&report, "unmet-dependency");
    assert_eq!(
        unmet
            .iter()
            .filter(|detail| detail.starts_with("feat/alpha requires"))
            .count(),
        1,
        "{unmet:?}"
    );
    assert_eq!(
        fixture.required("feat/alpha"),
        [
            "origin:acme-old/demo:feat/base",
            "origin:acme/demo:feat/base"
        ]
    );
    fixture.depends(&["--forget", "feat/base"]);
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
}

#[test]
fn status_without_the_landed_probe_keeps_a_member_requirement_unmet() {
    // Given: feat/alpha requires the fork's #13, whose head is feat/base, and
    // feat/base itself.
    let fixture = fixture();
    fixture.depends(&["--on", "acme/demo#13", "--on", "feat/base"]);

    // When: status reports the fork without the landed probe.
    let output = fixture.knives(&["--json", "status", "demo", "--no-landed"]);
    let report: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("status JSON ({error}): {}", said(&output)));

    // Then: each stays an unmet finding that says the probe did not run,
    // not a problem: what is not shown to have landed is still needed.
    let unmet = details(&report, "unmet-dependency");
    for requirement in [
        "origin:acme/demo#13 (head feat/base, open)",
        "origin:acme/demo:feat/base",
    ] {
        assert!(
            unmet.contains(&format!(
                "feat/alpha requires {requirement}, which is not known to have landed: the \
                 landed probe did not run"
            )),
            "{requirement}: {unmet:?}\n{report}"
        );
    }
    assert!(
        !problems(&report)
            .iter()
            .any(|problem| problem.contains("needs")),
        "{report}"
    );
    assert_eq!(output.status.code(), Some(1), "{report}");
}

#[test]
fn status_judges_an_origin_only_member_by_its_squash_merged_pull_request() {
    // Given: members another clone pushed to origin, held here only as
    // `<branch>@origin`, each squash-merged upstream with content the
    // maintainer changed while merging, so neither replays empty:
    // remote-merged, whose origin tip is the merged head, and remote-past,
    // whose origin tip carries a commit past its merged head
    // remote-past-base; feat/alpha requires both.
    let fixture = fixture_with(ORIGIN, ORIGIN);
    let lab = &fixture.lab;
    lab.foreign_origin_branch("main@origin", "remote-merged", "merged\n");
    lab.foreign_origin_branch("main@origin", "remote-past-base", "base\n");
    lab.foreign_origin_branch("remote-past-base@origin", "remote-past", "past\n");
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
    let merged_head = lab::commit_at(lab, "remote-merged@origin");
    let past_merged_head = lab::commit_at(lab, "remote-past-base@origin");
    lab.upstream_trunk_file("remote-merged.txt", "merged, edited while merging\n");
    let merged_landing = lab::commit_at(lab, "main@upstream");
    lab.upstream_trunk_file("remote-past-base.txt", "base, edited while merging\n");
    let past_landing = lab::commit_at(lab, "main@upstream");
    fixture.forge(&[
        merged_upstream(
            30,
            "remote-merged",
            merged_head.as_str(),
            merged_landing.as_str(),
        ),
        merged_upstream(
            31,
            "remote-past",
            past_merged_head.as_str(),
            past_landing.as_str(),
        ),
    ]);
    fixture.depends(&["--on", "remote-merged", "--on", "remote-past"]);

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: the member whose merged pull request's landing is in the trunk
    // is met; the one whose origin tip is past its merged head is not.
    let unmet = details(&report, "unmet-dependency");
    assert!(
        unmet.iter().any(|detail| detail.starts_with(
            "feat/alpha requires origin:acme/demo:remote-past, which has not landed \
             (remote-past@origin "
        )),
        "{unmet:?}\n{report}"
    );
    assert!(
        !unmet.iter().any(|detail| detail.contains("remote-merged")),
        "{unmet:?}\n{report}"
    );
    assert_eq!(problems(&report), Vec::<String>::new(), "{report}");
    assert_eq!(code, Some(1), "{report}");
}

/// The unmet findings `report` gives for feat/alpha's two requirements on
/// feat/base: the member itself and the fork's #13, whose head it is.
fn base_and_its_review_unmet(report: &Value) -> usize {
    details(report, "unmet-dependency")
        .iter()
        .filter(|detail| {
            detail.starts_with(
                "feat/alpha requires origin:acme/demo:feat/base, which has not landed (",
            ) || detail.starts_with(
                "feat/alpha requires origin:acme/demo#13 (head feat/base, open), which has not \
                 landed (",
            )
        })
        .count()
}

/// feat/base's row in `report`: its state and its landed verdict.
fn base_row(report: &Value) -> (String, String) {
    let row = report["branches"]
        .as_array()
        .expect("branches")
        .iter()
        .find(|row| row["name"] == "feat/base")
        .expect("a feat/base row");
    (
        row["state"].as_str().unwrap_or_default().to_owned(),
        row["landed"].as_str().unwrap_or_default().to_owned(),
    )
}

#[test]
fn a_member_whose_upstream_pull_request_merged_outside_the_trunk_stays_unmet() {
    // Given: feat/base, whose upstream pull request #41 merged into
    // something other than the upstream trunk: its merge commit is not in
    // the trunk, and the trunk does not have its content. feat/alpha
    // requires feat/base and the fork's #13, whose head is feat/base.
    let fixture = fixture_with(ORIGIN, ORIGIN);
    let head = lab::commit_at(&fixture.lab, "feat/base");
    fixture.forge(&[merged_upstream(
        41,
        "feat/base",
        head.as_str(),
        head.as_str(),
    )]);
    fixture.depends(&["--on", "feat/base", "--on", "acme/demo#13"]);

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: the merge meets neither: the row says merged, not landed, and
    // the upstream does not have the work.
    assert_eq!(
        base_row(&report),
        ("merged".to_owned(), "not-in-trunk".to_owned()),
        "{report}"
    );
    assert_eq!(base_and_its_review_unmet(&report), 2, "{report}");
    assert_eq!(code, Some(1), "{report}");
}

#[test]
fn a_member_advanced_past_its_squash_merged_pull_request_stays_unmet() {
    // Given: feat/base, pushed and squash-merged upstream as #40. feat/alpha
    // requires feat/base and the fork's #13, whose head is feat/base.
    let fixture = fixture_with(ORIGIN, ORIGIN);
    let lab = &fixture.lab;
    lab.push_branch("feat/base");
    lab.publish_pull("feat/base", 40);
    let merged = lab::commit_at(lab, "feat/base");
    lab.squash_merge_pull(40, None);
    let landing = lab::commit_at(lab, "main@upstream");
    fixture.forge(&[merged_upstream(
        40,
        "feat/base",
        merged.as_str(),
        landing.as_str(),
    )]);
    fixture.depends(&["--on", "feat/base", "--on", "acme/demo#13"]);

    // Then, before any new work: both are met, by the squash merge's
    // landing in the trunk.
    let (_, report) = status(&fixture);
    assert_eq!(base_and_its_review_unmet(&report), 0, "{report}");
    assert_eq!(problems(&report), Vec::<String>::new(), "{report}");

    // When: feat/base is advanced with new work and pushed, and the fork's
    // #13 follows it.
    lab.jj_work(["new", "-r", "feat/base", "-m", "new unmerged work"]);
    std::fs::write(lab.work.join("unmerged.txt"), "new unmerged work\n")
        .expect("write the new work");
    lab.jj_work(["bookmark", "set", "feat/base", "-r", "@"]);
    lab.jj_work(["new"]);
    lab.push_branch("feat/base");
    let advanced = lab::commit_at(lab, "feat/base");
    assert_ne!(advanced, merged);
    fixture.move_head(13, advanced.as_str());
    let (code, report) = status(&fixture);

    // Then: the new work has not landed, so neither is met.
    assert_eq!(
        base_row(&report),
        ("merged".to_owned(), "not-in-trunk".to_owned()),
        "{report}"
    );
    assert_eq!(base_and_its_review_unmet(&report), 2, "{report}");
    assert_eq!(code, Some(1), "{report}");
}

#[test]
fn a_review_pull_request_is_judged_at_the_head_the_forge_reports() {
    // Given: feat/alpha requires the fork's #14, whose head feat/landed has
    // landed: met.
    let fixture = fixture();
    fixture.depends(&["--on", "acme/demo#14"]);
    let (code, report) = status(&fixture);
    assert_eq!(details(&report, "unmet-dependency"), Vec::<String>::new());
    assert_eq!(problems(&report), Vec::<String>::new(), "{report}");
    assert_ne!(code, Some(3), "{report}");

    // When: feat/landed is pushed, then advanced on origin from another
    // clone with new work this checkout has not fetched, and the forge
    // reports #14's head at that new commit.
    fixture.lab.push_branch("feat/landed");
    fixture
        .lab
        .advance_origin_branch("feat/landed", "new work on origin\n");
    let advanced = knives::jj::Repo::open(&fixture.lab.second)
        .expect("open the other clone")
        .resolve_commit("feat/landed")
        .expect("the advanced head");
    fixture.move_head(14, advanced.as_str());
    let (code, report) = status(&fixture);

    // Then: the landed local branch of the same name does not meet it: the
    // head the forge reports is not here to judge.
    let problems = problems(&report);
    assert!(
        problems.iter().any(|problem| problem.starts_with(&format!(
            "cannot check whether feat/alpha still needs origin:acme/demo#14 (head feat/landed, \
             open): its head commit {} is not in this checkout",
            advanced.short()
        ))),
        "{problems:?}\n{report}"
    );
    assert_eq!(code, Some(3), "{report}");

    // And: once fetched, the new work is judged, and has not landed.
    fixture.lab.jj_work(["git", "fetch", "--remote", "origin"]);
    let (code, report) = status(&fixture);
    assert!(
        details(&report, "unmet-dependency")
            .iter()
            .any(|detail| detail.starts_with(
                "feat/alpha requires origin:acme/demo#14 (head feat/landed, open), which has not \
             landed ("
            )),
        "{report}"
    );
    assert_eq!(code, Some(1), "{report}");
}

#[test]
fn a_review_pull_request_recorded_before_a_rename_is_judged_by_its_head() {
    // Given: feat/alpha requires the fork's #14, whose head feat/landed has
    // landed, recorded under the repository's name from before its rename,
    // `acme-old/demo`, which the forge answers as `acme/demo`: once with this
    // machine's checkout and registry still on the old name, once with both
    // on the new one.
    for (origin, registered) in [(REGISTRY_ORIGIN, REGISTRY_ORIGIN), (ORIGIN, ORIGIN)] {
        let fixture = fixture_with(origin, registered);
        fixture.depends(&["--on", "acme/demo#14"]);
        fixture.rewrite_requirements("origin:acme/demo#14", "origin:acme-old/demo#14");

        // When: status reports the fork.
        let (_, report) = status(&fixture);

        // Then: the record is the fork's own review pull request, judged by
        // its head, and met.
        assert_eq!(
            details(&report, "unmet-dependency"),
            Vec::<String>::new(),
            "{origin}: {report}"
        );
        assert_eq!(
            problems(&report),
            Vec::<String>::new(),
            "{origin}: {report}"
        );
    }
}

#[test]
fn a_member_held_under_its_old_spelling_is_named_and_forgotten_by_any_spelling() {
    // Given: feat/alpha requires feat/base, recorded under the fork's
    // repository's old spelling only.
    let fixture = fixture();
    fixture.depends(&["--on", "feat/base"]);
    fixture.rewrite_requirements(
        "origin:acme/demo:feat/base",
        "origin:acme-old/demo:feat/base",
    );

    // When / Then: naming it again says what is held, and adds nothing.
    let named = fixture.depends(&["--on", "feat/base"]);
    assert_eq!(
        named.trim_end(),
        "demo/feat/alpha now requires origin:acme-old/demo:feat/base"
    );
    assert_eq!(
        fixture.required("feat/alpha"),
        ["origin:acme-old/demo:feat/base"]
    );

    // And: its record under the new spelling takes it back.
    fixture.depends(&["--forget", "origin:acme/demo:feat/base"]);
    assert_eq!(fixture.required("feat/alpha"), Vec::<String>::new());
}

#[test]
fn a_merged_review_pull_request_whose_head_is_gone_names_its_base() {
    // Given: feat/alpha requires the fork's #22, merged into review/base,
    // whose head feat/gone is no branch here or on origin any more.
    let fixture = fixture();
    fixture.depends(&["--on", "acme/demo#22"]);

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: a problem that points at the base holding the member's content,
    // not one that says to forget the requirement.
    let problems = problems(&report);
    let problem = problems
        .iter()
        .find(|problem| {
            problem.starts_with(
                "cannot check whether feat/alpha still needs origin:acme/demo#22 (head \
                 feat/gone, merged):",
            )
        })
        .unwrap_or_else(|| panic!("{problems:?}\n{report}"));
    assert!(problem.contains("review/base"), "{problem}");
    assert!(!problem.contains("forget the requirement"), "{problem}");
    assert_eq!(code, Some(3), "{report}");
}

#[test]
fn status_checks_a_dependent_this_checkout_holds_only_on_origin() {
    // Given: remote-dep, a branch another clone pushed to origin, held here
    // only as remote-dep@origin and claimed by nobody; it requires feat/base,
    // which has not landed.
    let fixture = fixture();
    fixture
        .lab
        .foreign_origin_branch("main@origin", "remote-dep", "dep\n");
    fixture.lab.jj_work(["git", "fetch", "--remote", "origin"]);
    fixture.depends_of("remote-dep", &["--on", "feat/base"]);

    // When: status reports the fork.
    let (code, report) = status(&fixture);

    // Then: the requirement is checked, as for a dependent with a row.
    assert!(
        details(&report, "unmet-dependency").contains(
            &"remote-dep requires origin:acme/demo:feat/base, which has not landed (no-pr)"
                .to_owned()
        ),
        "{report}"
    );
    assert_eq!(code, Some(1), "{report}");
}

#[test]
fn one_review_pull_request_does_not_change_another_ones_answer_at_the_same_commit() {
    // Given: the fork's #14, head feat/landed, and #13, head feat/base, both
    // at feat/base's first commit, which no local bookmark points at any more
    // (feat/base has new work). Upstream #40, from acme's feat/landed at that
    // commit, landed it squash-merged with content the maintainer changed,
    // so only the merge evidence for feat/landed shows it landed.
    let fixture = fixture_with(ORIGIN, ORIGIN);
    let lab = &fixture.lab;
    let head = lab::commit_at(lab, "feat/base");
    lab.jj_work(["new", "-r", "feat/base", "-m", "new local work"]);
    std::fs::write(lab.work.join("later.txt"), "later local work\n").expect("write the new work");
    lab.jj_work(["bookmark", "set", "feat/base", "-r", "@"]);
    lab.jj_work(["new"]);
    lab.upstream_trunk_file("base.txt", "base changed while merging\n");
    let landing = lab::commit_at(lab, "main@upstream");
    fixture.forge(&[merged_upstream(
        40,
        "feat/landed",
        head.as_str(),
        landing.as_str(),
    )]);
    fixture.move_head(14, head.as_str());
    fixture.move_head(13, head.as_str());

    // Then, with #14 alone: met, by #40's landing.
    fixture.depends(&["--on", "acme/demo#14"]);
    let (_, control) = status(&fixture);
    assert_eq!(details(&control, "unmet-dependency"), Vec::<String>::new());
    assert_eq!(problems(&control), Vec::<String>::new(), "{control}");

    // When: #13 is required too, judged first: feat/base has no merge
    // evidence, and its replay of that commit conflicts with the trunk.
    fixture.depends(&["--on", "acme/demo#13"]);
    let (_, both) = status(&fixture);

    // Then: #13 is unmet, and #14's answer does not change: the same commit
    // under another branch's evidence is judged again.
    let unmet = details(&both, "unmet-dependency");
    assert!(
        unmet
            .iter()
            .any(|detail| detail.starts_with("feat/alpha requires origin:acme/demo#13")),
        "{unmet:?}\n{both}"
    );
    assert!(
        !unmet
            .iter()
            .any(|detail| detail.contains("origin:acme/demo#14")),
        "{unmet:?}\n{both}"
    );
}
